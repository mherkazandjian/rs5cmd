//! Stateless "S3" stub for benchmarking clients in isolation (feature `bench-stub`).
//!
//! It answers just enough of the S3 REST API (path-style) for s5cmd / rs5cmd benchmarks, keeps
//! no object data and does no disk I/O:
//!
//! | Request                                   | Answer                                            |
//! |-------------------------------------------|---------------------------------------------------|
//! | `PUT /b`                                  | CreateBucket                                      |
//! | `HEAD /b`, `GET /b?location`, `GET /`     | HeadBucket, GetBucketLocation, ListBuckets        |
//! | `PUT /b/k`                                | PutObject: body read and discarded, bytes counted |
//! | `PUT /b/k` + `x-amz-copy-source`          | CopyObject / UploadPartCopy                       |
//! | `GET /b/k`, `HEAD /b/k` (+ `Range`)       | Body generated from a fixed pattern block         |
//! | `GET /b?list-type=2`                      | ListObjectsV2 over the synthetic datasets, paged  |
//! | `POST /b?delete`                          | DeleteObjects, echoing every key                  |
//! | `POST /b/k?uploads`, `PUT ?partNumber`,   | Multipart create / part / complete / abort        |
//! | `POST ?uploadId`, `DELETE ?uploadId`      |                                                   |
//! | `DELETE /b[/k]`                           | 204                                               |
//!
//! Object content: byte `o` of every object is `o % 251`, so ranged and whole downloads can be
//! verified without storing anything.
//!
//! Listings come from synthetic *datasets* (key template + count + size + mtime) configured over
//! the control API, so 1M-key listings cost no memory. GET/HEAD of a key inside a dataset return
//! that dataset's size and mtime; any other key has `STUB_OBJECT_SIZE` bytes.
//!
//! Control API (paths that cannot be bucket names):
//!   `GET  /__stub/stats`             JSON counters since the last reset
//!   `POST /__stub/reset`             zero the counters
//!   `POST /__stub/config?latency_ms=&conn_latency_ms=&fault_rate=&default_size=&dump=&dump_max=`
//!   `PUT  /__stub/dataset?bucket=&prefix=&count=&size=&width=&name=&suffix=&fanout=&dirs=
//!         &mtime=&change_mod=&change_size=`
//!   `DELETE /__stub/dataset[?bucket=]`
//!
//! Environment: `STUB_PORT` (9100), `STUB_TLS_PORT` + `STUB_TLS_CERT` + `STUB_TLS_KEY` (an extra
//! HTTPS listener offering HTTP/1.1 only, as S3 does), `STUB_H2_PORT` (with the same certificate:
//! an HTTPS listener offering HTTP/2 and HTTP/1.1, as an HTTP/2 front end does; over HTTP/2 it
//! answers HEAD, GET and ListObjectsV2 only, at most `STUB_H2_STREAMS` (128) at a time per connection),
//! `STUB_OBJECT_SIZE` (4096), `STUB_THREADS`
//! (default: CPUs in the affinity mask), `STUB_LATENCY_MS`, `STUB_CONN_LATENCY_MS`,
//! `STUB_FAULT_RATE` (fraction of object requests answered 503 SlowDown), `STUB_DUMP` (file for
//! request lines + headers) and `STUB_DUMP_MAX` (200).
//!
//!   cargo run --release --features bench-stub --bin s3stub

use std::fmt::Write as _;
use std::io::Write as _;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;

const PERIOD: u64 = 251;
const CHUNK: usize = 1 << 20;
const READ_BUF: usize = 128 * 1024;
const DEFAULT_MTIME: i64 = 1_704_067_200; // 2024-01-01T00:00:00Z

// ---------------------------------------------------------------------------------------------
// Counters
// ---------------------------------------------------------------------------------------------

macro_rules! counters {
    ($($name:ident),* $(,)?) => {
        #[allow(non_upper_case_globals)]
        mod ctr { use std::sync::atomic::AtomicU64; $(pub static $name: AtomicU64 = AtomicU64::new(0);)* }
        fn stats_json() -> String {
            let mut s = String::from("{");
            $( let _ = write!(s, "\"{}\":{},", stringify!($name), ctr::$name.load(Relaxed)); )*
            let _ = write!(s, "\"conns_active\":{},\"conns_peak\":{}}}",
                ACTIVE.load(Relaxed), PEAK.load(Relaxed));
            s
        }
        fn stats_reset() {
            $( ctr::$name.store(0, Relaxed); )*
            PEAK.store(ACTIVE.load(Relaxed), Relaxed);
        }
    };
}

counters!(
    conns, conns_tls, conns_h2, h2_streams, reqs, puts, put_bytes, gets, get_bytes, heads, lists, list_keys,
    list_prefixes, delete_one, delete_batches, delete_keys, mpu_create, mpu_parts, mpu_part_bytes,
    mpu_complete, mpu_abort, copies, faults, bucket_ops, other, bad_requests,
);

static ACTIVE: AtomicI64 = AtomicI64::new(0);
static PEAK: AtomicI64 = AtomicI64::new(0);

fn inc(c: &AtomicU64) {
    c.fetch_add(1, Relaxed);
}

// ---------------------------------------------------------------------------------------------
// Runtime configuration
// ---------------------------------------------------------------------------------------------

struct Settings {
    latency_us: AtomicU64,
    conn_latency_us: AtomicU64,
    fault_ppm: AtomicU64,
    default_size: AtomicU64,
    dump: Mutex<Option<(std::io::BufWriter<std::fs::File>, u64)>>,
    datasets: RwLock<Vec<Dataset>>,
}

fn settings() -> &'static Settings {
    static S: OnceLock<Settings> = OnceLock::new();
    S.get_or_init(|| Settings {
        latency_us: AtomicU64::new(0),
        conn_latency_us: AtomicU64::new(0),
        fault_ppm: AtomicU64::new(0),
        default_size: AtomicU64::new(4096),
        dump: Mutex::new(None),
        datasets: RwLock::new(Vec::new()),
    })
}

fn env_f64(name: &str) -> Option<f64> {
    std::env::var(name).ok().and_then(|s| s.parse().ok())
}

fn set_dump(path: &str, max: u64) {
    let mut d = settings().dump.lock().unwrap();
    *d = if path.is_empty() {
        None
    } else {
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .ok()
            .map(|f| (std::io::BufWriter::new(f), max))
    };
}

/// The fixed block every body is cut from: byte i is `i % 251`.
fn pattern() -> &'static [u8] {
    static P: OnceLock<Vec<u8>> = OnceLock::new();
    P.get_or_init(|| (0..CHUNK as u64 + PERIOD).map(|i| (i % PERIOD) as u8).collect())
}

// ---------------------------------------------------------------------------------------------
// Synthetic datasets
// ---------------------------------------------------------------------------------------------

/// `count` keys `{prefix}[{i/fanout:0w}/{dirs}]{name}{i:0w}{suffix}`, sorted by `i`.
#[derive(Clone)]
struct Dataset {
    bucket: String,
    prefix: String,
    count: u64,
    width: usize,
    name: String,
    suffix: String,
    fanout: u64,
    dirs: String,
    size: u64,
    change_mod: u64,
    change_size: u64,
    mtime_iso: String,
    mtime_http: String,
}

impl Dataset {
    fn key(&self, i: u64, out: &mut String) {
        out.clear();
        out.push_str(&self.prefix);
        if self.fanout > 0 {
            let _ = write!(out, "{:0w$}/", i / self.fanout, w = self.width);
            out.push_str(&self.dirs);
        }
        out.push_str(&self.name);
        let _ = write!(out, "{:0w$}", i, w = self.width);
        out.push_str(&self.suffix);
    }

    fn size_of(&self, i: u64) -> u64 {
        if self.change_mod > 0 && i % self.change_mod == 0 {
            self.change_size
        } else {
            self.size
        }
    }

    /// Index of `key` in this dataset, if it is one of its keys.
    fn index_of(&self, key: &str) -> Option<u64> {
        let rest = key.strip_prefix(self.prefix.as_str())?.strip_suffix(self.suffix.as_str())?;
        if rest.len() < self.width {
            return None;
        }
        let i: u64 = rest[rest.len() - self.width..].parse().ok()?;
        if i >= self.count {
            return None;
        }
        let mut k = String::with_capacity(key.len());
        self.key(i, &mut k);
        (k == key).then_some(i)
    }

    /// First index in `[lo, count)` for which `pred(key)` holds; `pred` must be monotone.
    fn first(&self, lo: u64, pred: impl Fn(&str) -> bool) -> u64 {
        let (mut lo, mut hi) = (lo, self.count);
        let mut k = String::new();
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            self.key(mid, &mut k);
            if pred(&k) {
                hi = mid;
            } else {
                lo = mid + 1;
            }
        }
        lo
    }
}

fn parse_dataset(q: &Query) -> Result<Dataset, String> {
    let bucket = q.get("bucket").ok_or("bucket= is required")?.to_string();
    let num = |k: &str, d: u64| -> Result<u64, String> {
        q.get(k).map(|v| v.parse::<u64>().map_err(|e| format!("{k}: {e}"))).unwrap_or(Ok(d))
    };
    let mtime = q
        .get("mtime")
        .map(|v| v.parse::<i64>().map_err(|e| format!("mtime: {e}")))
        .unwrap_or(Ok(DEFAULT_MTIME))?;
    let size = num("size", 4096)?;
    let count = num("count", 0)?;
    // Fixed-width indices keep the keys sorted, so the width must fit `count - 1`.
    let digits = count.saturating_sub(1).max(1).ilog10() as usize + 1;
    Ok(Dataset {
        bucket,
        prefix: q.get("prefix").unwrap_or("").to_string(),
        count,
        width: (num("width", 7)? as usize).max(digits),
        name: q.get("name").unwrap_or("obj_").to_string(),
        suffix: q.get("suffix").unwrap_or(".dat").to_string(),
        fanout: num("fanout", 0)?,
        dirs: q.get("dirs").unwrap_or("").to_string(),
        size,
        change_mod: num("change_mod", 0)?,
        change_size: num("change_size", size + 1)?,
        mtime_iso: fmt_time(mtime, true),
        mtime_http: fmt_time(mtime, false),
    })
}

/// Size and Last-Modified of an object (dataset keys) or the defaults.
fn lookup(bucket: &str, key: &str) -> (u64, String) {
    let ds = settings().datasets.read().unwrap();
    for d in ds.iter().filter(|d| d.bucket == bucket) {
        if let Some(i) = d.index_of(key) {
            return (d.size_of(i), d.mtime_http.clone());
        }
    }
    (settings().default_size.load(Relaxed), fmt_time(DEFAULT_MTIME, false))
}

struct Listing {
    contents: Vec<(String, u64, String)>,
    prefixes: Vec<String>,
    truncated: bool,
    next: Option<String>,
}

/// ListObjectsV2 over the bucket's datasets. `after` is an exclusive lower bound; when it ends
/// with the delimiter it was a common prefix and everything under it is skipped too.
fn list(bucket: &str, prefix: &str, delim: &str, after: &str, max: usize) -> Listing {
    let ds = settings().datasets.read().unwrap();
    let mut out = Listing { contents: Vec::new(), prefixes: Vec::new(), truncated: false, next: None };
    let skip_under = !delim.is_empty() && after.ends_with(delim);
    let mut k = String::new();
    let mut sets: Vec<&Dataset> = ds
        .iter()
        .filter(|d| d.bucket == bucket && (d.prefix.starts_with(prefix) || prefix.starts_with(&d.prefix)))
        .collect();
    sets.sort_by(|a, b| a.prefix.cmp(&b.prefix));
    for d in sets {
        let mut i = d.first(0, |key| {
            key >= prefix && key > after && !(skip_under && key.starts_with(after))
        });
        while i < d.count {
            d.key(i, &mut k);
            if !k.starts_with(prefix) {
                break;
            }
            if out.contents.len() + out.prefixes.len() >= max {
                out.truncated = true;
                return out;
            }
            let rest = &k[prefix.len()..];
            match (!delim.is_empty()).then(|| rest.find(delim)).flatten() {
                Some(j) => {
                    let cp = k[..prefix.len() + j + delim.len()].to_string();
                    i = d.first(i, |key| key > cp.as_str() && !key.starts_with(cp.as_str()));
                    out.next = Some(cp.clone());
                    out.prefixes.push(cp);
                }
                None => {
                    out.next = Some(k.clone());
                    out.contents.push((k.clone(), d.size_of(i), d.mtime_iso.clone()));
                    i += 1;
                }
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------------------------
// Small helpers: time, queries, encoding
// ---------------------------------------------------------------------------------------------

/// `1970-01-01T00:00:00.000Z` (iso) or `Thu, 01 Jan 1970 00:00:00 GMT` (http).
fn fmt_time(t: i64, iso: bool) -> String {
    let days = t.div_euclid(86_400);
    let secs = t.rem_euclid(86_400);
    let (hh, mm, ss) = (secs / 3600, secs % 3600 / 60, secs % 60);
    // Civil-from-days (H. Hinnant).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    if iso {
        format!("{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}.000Z")
    } else {
        const WD: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];
        const MO: [&str; 12] =
            ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
        let wd = WD[days.rem_euclid(7) as usize];
        format!("{wd}, {d:02} {} {y:04} {hh:02}:{mm:02}:{ss:02} GMT", MO[(m - 1) as usize])
    }
}

struct Query(Vec<(String, String)>);

impl Query {
    fn parse(q: &str) -> Query {
        Query(
            q.split('&')
                .filter(|s| !s.is_empty())
                .map(|kv| match kv.split_once('=') {
                    Some((k, v)) => (pct_decode(k), pct_decode(v)),
                    None => (pct_decode(kv), String::new()),
                })
                .collect(),
        )
    }
    fn get(&self, k: &str) -> Option<&str> {
        self.0.iter().find(|(a, _)| a == k).map(|(_, v)| v.as_str())
    }
    fn has(&self, k: &str) -> bool {
        self.0.iter().any(|(a, _)| a == k)
    }
}

fn pct_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' if i + 2 < b.len() => {
                match u8::from_str_radix(&s[i + 1..i + 3], 16) {
                    Ok(h) => {
                        out.push(h);
                        i += 3;
                    }
                    Err(_) => {
                        out.push(b'%');
                        i += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn pct_encode(s: &str, out: &mut String) {
    for &c in s.as_bytes() {
        if c.is_ascii_alphanumeric() || b"-_.~/".contains(&c) {
            out.push(c as char);
        } else {
            let _ = write!(out, "%{c:02X}");
        }
    }
}

fn xml_escape(s: &str, out: &mut String) {
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            c => out.push(c),
        }
    }
}

fn hex(s: &str) -> String {
    s.bytes().map(|b| format!("{b:02x}")).collect()
}

fn unhex(s: &str) -> String {
    let b: Vec<u8> = (0..s.len() / 2)
        .filter_map(|i| u8::from_str_radix(s.get(2 * i..2 * i + 2)?, 16).ok())
        .collect();
    String::from_utf8_lossy(&b).into_owned()
}

fn splitmix(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

// ---------------------------------------------------------------------------------------------
// Server
// ---------------------------------------------------------------------------------------------

fn main() -> std::io::Result<()> {
    let threads = std::env::var("STUB_THREADS").ok().and_then(|s| s.parse().ok()).unwrap_or_else(|| {
        std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1)
    });
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(threads)
        .enable_all()
        .build()?
        .block_on(serve(threads))
}

async fn serve(threads: usize) -> std::io::Result<()> {
    let st = settings();
    let ms = |name: &str| (env_f64(name).unwrap_or(0.0) * 1000.0) as u64;
    st.latency_us.store(ms("STUB_LATENCY_MS"), Relaxed);
    st.conn_latency_us.store(ms("STUB_CONN_LATENCY_MS"), Relaxed);
    st.fault_ppm.store((env_f64("STUB_FAULT_RATE").unwrap_or(0.0) * 1e6) as u64, Relaxed);
    if let Some(n) = std::env::var("STUB_OBJECT_SIZE").ok().and_then(|s| s.parse().ok()) {
        st.default_size.store(n, Relaxed);
    }
    if let Ok(p) = std::env::var("STUB_DUMP") {
        set_dump(&p, std::env::var("STUB_DUMP_MAX").ok().and_then(|s| s.parse().ok()).unwrap_or(200));
    }
    pattern();

    let port: u16 = std::env::var("STUB_PORT").ok().and_then(|s| s.parse().ok()).unwrap_or(9100);
    let plain = TcpListener::bind(("0.0.0.0", port)).await?;
    eprintln!("s3stub: http on :{port}, {threads} threads");

    if let Ok(tls_port) = std::env::var("STUB_TLS_PORT") {
        let acceptor = tls_acceptor(&[b"http/1.1"])?;
        let l = TcpListener::bind(("0.0.0.0", tls_port.parse::<u16>().unwrap_or(9443))).await?;
        eprintln!("s3stub: https (http/1.1 only) on :{tls_port}");
        tokio::spawn(async move {
            loop {
                let Ok((tcp, _)) = l.accept().await else { continue };
                let acceptor = acceptor.clone();
                tokio::spawn(async move {
                    let _ = tcp.set_nodelay(true);
                    conn_start(true);
                    if let Ok(tls) = acceptor.accept(tcp).await {
                        let _ = handle_conn(tls).await;
                    }
                    ACTIVE.fetch_sub(1, Relaxed);
                });
            }
        });
    }

    if let Ok(h2_port) = std::env::var("STUB_H2_PORT") {
        let acceptor = tls_acceptor(&[b"h2", b"http/1.1"])?;
        let l = TcpListener::bind(("0.0.0.0", h2_port.parse::<u16>().unwrap_or(9444))).await?;
        eprintln!("s3stub: https (h2 or http/1.1) on :{h2_port}");
        tokio::spawn(async move {
            loop {
                let Ok((tcp, _)) = l.accept().await else { continue };
                let acceptor = acceptor.clone();
                tokio::spawn(async move {
                    let _ = tcp.set_nodelay(true);
                    conn_start(true);
                    if let Ok(tls) = acceptor.accept(tcp).await {
                        if tls.get_ref().1.alpn_protocol() == Some(&b"h2"[..]) {
                            inc(&ctr::conns_h2);
                            serve_h2(tls).await;
                        } else {
                            let _ = handle_conn(tls).await;
                        }
                    }
                    ACTIVE.fetch_sub(1, Relaxed);
                });
            }
        });
    }

    loop {
        let (tcp, _) = plain.accept().await?;
        tokio::spawn(async move {
            let _ = tcp.set_nodelay(true);
            conn_start(false);
            let _ = handle_conn(tcp).await;
            ACTIVE.fetch_sub(1, Relaxed);
        });
    }
}

fn conn_start(tls: bool) {
    inc(&ctr::conns);
    if tls {
        inc(&ctr::conns_tls);
    }
    let n = ACTIVE.fetch_add(1, Relaxed) + 1;
    PEAK.fetch_max(n, Relaxed);
}

fn tls_acceptor(alpn: &[&[u8]]) -> std::io::Result<tokio_rustls::TlsAcceptor> {
    use std::io::{Error, ErrorKind};
    let cert_path = std::env::var("STUB_TLS_CERT").map_err(|_| Error::other("STUB_TLS_CERT unset"))?;
    let key_path = std::env::var("STUB_TLS_KEY").map_err(|_| Error::other("STUB_TLS_KEY unset"))?;
    let certs = rustls_pemfile::certs(&mut std::io::BufReader::new(std::fs::File::open(cert_path)?))
        .collect::<Result<Vec<_>, _>>()?;
    let key = rustls_pemfile::private_key(&mut std::io::BufReader::new(std::fs::File::open(key_path)?))?
        .ok_or_else(|| Error::new(ErrorKind::InvalidData, "no private key"))?;
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut cfg = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(Error::other)?
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(Error::other)?;
    cfg.alpn_protocols = alpn.iter().map(|p| p.to_vec()).collect();
    Ok(tokio_rustls::TlsAcceptor::from(Arc::new(cfg)))
}

/// One HTTP/2 connection: HEAD and GET of objects (ranged or whole), HEAD of buckets and
/// ListObjectsV2, which is what a download asks; anything else is answered 501. Every request runs in its own task, while
/// this one drives the connection, so all of a connection's frames pass through one task, as in a
/// real HTTP/2 server.
async fn serve_h2<S: AsyncRead + AsyncWrite + Unpin + Send + 'static>(io: S) {
    let streams = std::env::var("STUB_H2_STREAMS").ok().and_then(|s| s.parse().ok()).unwrap_or(128);
    let Ok(mut conn) = h2::server::Builder::new()
        .max_concurrent_streams(streams)
        .handshake::<_, bytes::Bytes>(io)
        .await
    else {
        return;
    };
    while let Some(Ok((req, respond))) = conn.accept().await {
        tokio::spawn(h2_request(req, respond));
    }
}

async fn h2_request(
    req: http::Request<h2::RecvStream>,
    mut respond: h2::server::SendResponse<bytes::Bytes>,
) {
    inc(&ctr::reqs);
    inc(&ctr::h2_streams);
    let lat = settings().latency_us.load(Relaxed);
    if lat > 0 {
        tokio::time::sleep(Duration::from_micros(lat)).await;
    }
    let path = pct_decode(req.uri().path());
    let q = Query::parse(req.uri().query().unwrap_or(""));
    let trimmed = path.trim_start_matches('/');
    let (bucket, key) = trimmed.split_once('/').unwrap_or((trimmed, ""));
    let range = req.headers().get("range").and_then(|v| v.to_str().ok());
    let object = |status: u16, len: u64, mtime: &str| {
        http::Response::builder()
            .status(status)
            .header("content-length", len)
            .header("last-modified", mtime)
            .header("etag", "\"stub\"")
            .header("content-type", "application/octet-stream")
    };
    enum Body {
        Empty,
        Pattern(u64, u64),
        Owned(bytes::Bytes),
    }
    let (resp, body) = match (req.method().as_str(), key.is_empty()) {
        ("HEAD", false) => {
            inc(&ctr::heads);
            let (size, mtime) = lookup(bucket, key);
            (object(200, size, &mtime), Body::Empty)
        }
        ("HEAD", true) => {
            inc(&ctr::bucket_ops);
            let r = http::Response::builder().status(200).header("x-amz-bucket-region", "us-east-1");
            (r, Body::Empty)
        }
        ("GET", true) if q.get("list-type") == Some("2") => {
            inc(&ctr::lists);
            let mut out = Vec::new();
            list_response(bucket, &q, &mut out);
            match h1_to_h2(&out) {
                Some((r, body)) => (r, Body::Owned(body)),
                None => (http::Response::builder().status(500), Body::Empty),
            }
        }
        ("GET", false) => {
            inc(&ctr::gets);
            let (size, mtime) = lookup(bucket, key);
            match range.and_then(|r| parse_range(r, size)) {
                Some((a, b)) if size > 0 && a <= b => {
                    let r = object(206, b - a + 1, &mtime)
                        .header("content-range", format!("bytes {a}-{b}/{size}"));
                    (r, Body::Pattern(a, b - a + 1))
                }
                Some(_) => {
                    let r = http::Response::builder()
                        .status(416)
                        .header("content-range", format!("bytes */{size}"));
                    (r, Body::Empty)
                }
                None => (object(200, size, &mtime), Body::Pattern(0, size)),
            }
        }
        _ => {
            inc(&ctr::other);
            (http::Response::builder().status(501), Body::Empty)
        }
    };
    let Ok(resp) = resp.body(()) else { return };
    let empty = match &body {
        Body::Empty => true,
        Body::Pattern(_, len) => *len == 0,
        Body::Owned(b) => b.is_empty(),
    };
    let Ok(send) = respond.send_response(resp, empty) else { return };
    match body {
        _ if empty => {}
        Body::Pattern(off, len) => {
            ctr::get_bytes.fetch_add(len, Relaxed);
            let p = pattern();
            h2_send(send, len, |off_in, n| {
                let a = ((off + off_in) % PERIOD) as usize;
                bytes::Bytes::from_static(&p[a..a + n])
            })
            .await
        }
        Body::Owned(b) => h2_send(send, b.len() as u64, |o, n| b.slice(o as usize..o as usize + n)).await,
        Body::Empty => {}
    }
}

/// A complete HTTP/1.1 response as the stub builds it: status, headers (hop-by-hop ones dropped)
/// and body.
fn h1_to_h2(raw: &[u8]) -> Option<(http::response::Builder, bytes::Bytes)> {
    let end = raw.windows(4).position(|w| w == b"\r\n\r\n")?;
    let head = std::str::from_utf8(&raw[..end]).ok()?;
    let mut lines = head.split("\r\n");
    let status: u16 = lines.next()?.split(' ').nth(1)?.parse().ok()?;
    let mut r = http::Response::builder().status(status);
    for (k, v) in lines.filter_map(|l| l.split_once(':')) {
        let k = k.trim().to_ascii_lowercase();
        if !matches!(k.as_str(), "connection" | "keep-alive" | "transfer-encoding") {
            r = r.header(k, v.trim());
        }
    }
    Some((r, bytes::Bytes::copy_from_slice(&raw[end + 4..])))
}

/// `len` bytes of a body, `chunk(offset, n)` at a time, as fast as the client's flow-control
/// windows allow.
async fn h2_send(
    mut send: h2::SendStream<bytes::Bytes>,
    len: u64,
    chunk: impl Fn(u64, usize) -> bytes::Bytes,
) {
    let mut off = 0;
    while off < len {
        send.reserve_capacity((len - off).min(CHUNK as u64) as usize);
        let n = match std::future::poll_fn(|cx| send.poll_capacity(cx)).await {
            Some(Ok(n)) => (n as u64).min(len - off).min(CHUNK as u64) as usize,
            _ => return, // reset or closed by the client
        };
        if n == 0 {
            continue;
        }
        if send.send_data(chunk(off, n), off + n as u64 == len).is_err() {
            return;
        }
        off += n as u64;
    }
}

/// The parts of a request head the stub needs.
#[derive(Default)]
struct Head {
    method: String,
    target: String,
    content_length: Option<u64>,
    decoded_length: Option<u64>,
    chunked: bool,
    expect_continue: bool,
    close: bool,
    range: Option<String>,
    copy_source: bool,
}

fn parse_head(raw: &[u8]) -> Option<Head> {
    let text = std::str::from_utf8(raw).ok()?;
    let mut lines = text.split("\r\n").flat_map(|l| l.split('\n'));
    let mut rl = lines.next()?.split_ascii_whitespace();
    let mut h = Head { method: rl.next()?.to_string(), target: rl.next()?.to_string(), ..Default::default() };
    for line in lines {
        let Some((k, v)) = line.split_once(':') else { continue };
        let (k, v) = (k.trim(), v.trim());
        if k.eq_ignore_ascii_case("content-length") {
            h.content_length = v.parse().ok();
        } else if k.eq_ignore_ascii_case("x-amz-decoded-content-length") {
            h.decoded_length = v.parse().ok();
        } else if k.eq_ignore_ascii_case("transfer-encoding") {
            h.chunked = v.to_ascii_lowercase().contains("chunked");
        } else if k.eq_ignore_ascii_case("expect") {
            h.expect_continue = v.eq_ignore_ascii_case("100-continue");
        } else if k.eq_ignore_ascii_case("connection") {
            h.close = v.eq_ignore_ascii_case("close");
        } else if k.eq_ignore_ascii_case("range") {
            h.range = Some(v.to_string());
        } else if k.eq_ignore_ascii_case("x-amz-copy-source") {
            h.copy_source = true;
        }
    }
    Some(h)
}

/// One HTTP/1.1 connection, plain or TLS.
async fn handle_conn<S: AsyncRead + AsyncWrite + Unpin>(stream: S) -> std::io::Result<()> {
    let st = settings();
    let cl = st.conn_latency_us.load(Relaxed);
    if cl > 0 {
        tokio::time::sleep(Duration::from_micros(cl)).await;
    }
    let mut s = BufReader::with_capacity(READ_BUF, stream);
    let mut raw: Vec<u8> = Vec::with_capacity(2048);
    let mut out: Vec<u8> = Vec::with_capacity(8192);
    loop {
        // Push out whatever the previous response left buffered: a TLS stream keeps the last
        // records of a large body in rustls's buffer until flushed (no-op for plain TCP).
        s.get_mut().flush().await?;
        // Request head: lines up to the first empty one.
        raw.clear();
        loop {
            let before = raw.len();
            if s.read_until(b'\n', &mut raw).await? == 0 {
                return Ok(());
            }
            let line = &raw[before..];
            if line == b"\r\n" || line == b"\n" {
                if before == 0 {
                    raw.clear(); // stray CRLF between requests
                    continue;
                }
                break;
            }
        }
        let Some(h) = parse_head(&raw) else {
            inc(&ctr::bad_requests);
            return Ok(());
        };
        dump_head(&raw);
        if h.expect_continue {
            s.get_mut().write_all(b"HTTP/1.1 100 Continue\r\n\r\n").await?;
        }
        let (path, query) = h.target.split_once('?').unwrap_or((h.target.as_str(), ""));
        let path = pct_decode(path);
        let q = Query::parse(query);

        // Request body: DeleteObjects and the control API need it; everything else is discarded.
        let wants_body = path.starts_with("/__stub/") || (h.method == "POST" && q.has("delete"));
        let mut body = Vec::new();
        let body_len = if let Some(n) = h.content_length {
            if wants_body {
                body.resize(n as usize, 0);
                s.read_exact(&mut body).await?;
            } else {
                discard(&mut s, n).await?;
            }
            n
        } else if h.chunked {
            read_chunked(&mut s, if wants_body { Some(&mut body) } else { None }).await?
        } else {
            0
        };
        let payload = h.decoded_length.unwrap_or(body_len);

        out.clear();
        if let Some(ctl) = path.strip_prefix("/__stub/") {
            control(&h.method, ctl, &q, &mut out);
            s.get_mut().write_all(&out).await?;
            if h.close {
                s.get_mut().flush().await?;
                return Ok(());
            }
            continue;
        }
        inc(&ctr::reqs);

        let trimmed = path.trim_start_matches('/');
        let (bucket, key) = trimmed.split_once('/').unwrap_or((trimmed, ""));
        let is_obj = !key.is_empty();

        let lat = st.latency_us.load(Relaxed);
        if lat > 0 {
            tokio::time::sleep(Duration::from_micros(lat)).await;
        }
        let ppm = st.fault_ppm.load(Relaxed);
        if is_obj && ppm > 0 && splitmix(ctr::reqs.load(Relaxed)) % 1_000_000 < ppm {
            inc(&ctr::faults);
            let x = "<?xml version=\"1.0\" encoding=\"UTF-8\"?><Error><Code>SlowDown</Code><Message>Please reduce your request rate.</Message></Error>";
            respond(&mut out, "503 Slow Down", "application/xml", &[], x.as_bytes());
            s.get_mut().write_all(&out).await?;
            if h.close {
                s.get_mut().flush().await?;
                return Ok(());
            }
            continue;
        }

        match (h.method.as_str(), is_obj) {
            ("PUT", true) if h.copy_source => {
                inc(&ctr::copies);
                let x = if q.has("partNumber") {
                    "<?xml version=\"1.0\" encoding=\"UTF-8\"?><CopyPartResult><LastModified>2024-01-01T00:00:00.000Z</LastModified><ETag>&quot;stub&quot;</ETag></CopyPartResult>"
                } else {
                    "<?xml version=\"1.0\" encoding=\"UTF-8\"?><CopyObjectResult><LastModified>2024-01-01T00:00:00.000Z</LastModified><ETag>&quot;stub&quot;</ETag></CopyObjectResult>"
                };
                respond(&mut out, "200 OK", "application/xml", &[], x.as_bytes());
            }
            ("PUT", true) if q.has("partNumber") => {
                inc(&ctr::mpu_parts);
                ctr::mpu_part_bytes.fetch_add(payload, Relaxed);
                let etag = format!("ETag: \"part-{}\"", q.get("partNumber").unwrap_or("0"));
                respond(&mut out, "200 OK", "", &[&etag], b"");
            }
            ("PUT", true) => {
                inc(&ctr::puts);
                ctr::put_bytes.fetch_add(payload, Relaxed);
                respond(&mut out, "200 OK", "", &["ETag: \"stub\""], b"");
            }
            ("PUT", false) => {
                inc(&ctr::bucket_ops);
                respond(&mut out, "200 OK", "", &[], b"");
            }
            ("POST", true) if q.has("uploads") => {
                let id = ctr::mpu_create.fetch_add(1, Relaxed) + 1;
                let mut x = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?><InitiateMultipartUploadResult><Bucket>");
                xml_escape(bucket, &mut x);
                x.push_str("</Bucket><Key>");
                xml_escape(key, &mut x);
                let _ = write!(x, "</Key><UploadId>stub-{id}</UploadId></InitiateMultipartUploadResult>");
                respond(&mut out, "200 OK", "application/xml", &[], x.as_bytes());
            }
            ("POST", true) if q.has("uploadId") => {
                inc(&ctr::mpu_complete);
                let mut x = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?><CompleteMultipartUploadResult><Location>http://stub/</Location><Bucket>");
                xml_escape(bucket, &mut x);
                x.push_str("</Bucket><Key>");
                xml_escape(key, &mut x);
                x.push_str("</Key><ETag>&quot;stub-mpu&quot;</ETag></CompleteMultipartUploadResult>");
                respond(&mut out, "200 OK", "application/xml", &[], x.as_bytes());
            }
            ("POST", false) if q.has("delete") => {
                inc(&ctr::delete_batches);
                let text = String::from_utf8_lossy(&body);
                let quiet = text.contains("<Quiet>true</Quiet>");
                let mut x = String::with_capacity(body.len() + 128);
                x.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?><DeleteResult xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\">");
                let mut rest = text.as_ref();
                while let Some(a) = rest.find("<Key>") {
                    let after = &rest[a + 5..];
                    let Some(b) = after.find("</Key>") else { break };
                    inc(&ctr::delete_keys);
                    if !quiet {
                        x.push_str("<Deleted><Key>");
                        x.push_str(&after[..b]); // still XML-escaped as received
                        x.push_str("</Key></Deleted>");
                    }
                    rest = &after[b + 6..];
                }
                x.push_str("</DeleteResult>");
                respond(&mut out, "200 OK", "application/xml", &[], x.as_bytes());
            }
            ("DELETE", true) if q.has("uploadId") => {
                inc(&ctr::mpu_abort);
                respond(&mut out, "204 No Content", "", &[], b"");
            }
            ("DELETE", true) => {
                inc(&ctr::delete_one);
                respond(&mut out, "204 No Content", "", &[], b"");
            }
            ("DELETE", false) => {
                inc(&ctr::bucket_ops);
                respond(&mut out, "204 No Content", "", &[], b"");
            }
            ("HEAD", true) => {
                inc(&ctr::heads);
                let (size, mtime) = lookup(bucket, key);
                let cl = format!("Content-Length: {size}");
                let lm = format!("Last-Modified: {mtime}");
                head_only(&mut out, "200 OK", &[&cl, &lm, "ETag: \"stub\"", "Content-Type: application/octet-stream"]);
            }
            ("HEAD", false) => {
                inc(&ctr::bucket_ops);
                head_only(&mut out, "200 OK", &["x-amz-bucket-region: us-east-1", "Content-Length: 0"]);
            }
            ("GET", true) => {
                inc(&ctr::gets);
                let (size, mtime) = lookup(bucket, key);
                let lm = format!("Last-Modified: {mtime}");
                match h.range.as_deref().and_then(|r| parse_range(r, size)) {
                    Some((a, b)) if size > 0 => {
                        let cr = format!("Content-Range: bytes {a}-{b}/{size}");
                        let cl = format!("Content-Length: {}", b - a + 1);
                        head_only(&mut out, "206 Partial Content", &[&cl, &cr, &lm, "ETag: \"stub\"", "Content-Type: application/octet-stream"]);
                        send_pattern(&mut s, &mut out, a, b - a + 1).await?;
                    }
                    Some(_) => {
                        let cr = format!("Content-Range: bytes */{size}");
                        respond(&mut out, "416 Requested Range Not Satisfiable", "application/xml", &[&cr], b"<Error><Code>InvalidRange</Code></Error>");
                        s.get_mut().write_all(&out).await?;
                    }
                    None => {
                        let cl = format!("Content-Length: {size}");
                        head_only(&mut out, "200 OK", &[&cl, &lm, "ETag: \"stub\"", "Content-Type: application/octet-stream"]);
                        send_pattern(&mut s, &mut out, 0, size).await?;
                    }
                }
                out.clear();
            }
            ("GET", false) if q.get("list-type") == Some("2") => {
                inc(&ctr::lists);
                list_response(bucket, &q, &mut out);
            }
            ("GET", false) if q.has("location") => {
                inc(&ctr::bucket_ops);
                respond(&mut out, "200 OK", "application/xml", &[], b"<?xml version=\"1.0\" encoding=\"UTF-8\"?><LocationConstraint xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\"></LocationConstraint>");
            }
            ("GET", false) if q.has("versioning") => {
                inc(&ctr::bucket_ops);
                respond(&mut out, "200 OK", "application/xml", &[], b"<?xml version=\"1.0\" encoding=\"UTF-8\"?><VersioningConfiguration xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\"></VersioningConfiguration>");
            }
            ("GET", false) if bucket.is_empty() => {
                inc(&ctr::bucket_ops);
                let mut names: Vec<String> =
                    st.datasets.read().unwrap().iter().map(|d| d.bucket.clone()).collect();
                names.sort();
                names.dedup();
                let mut x = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?><ListAllMyBucketsResult><Owner><ID>stub</ID></Owner><Buckets>");
                for n in names {
                    x.push_str("<Bucket><Name>");
                    xml_escape(&n, &mut x);
                    x.push_str("</Name><CreationDate>2024-01-01T00:00:00.000Z</CreationDate></Bucket>");
                }
                x.push_str("</Buckets></ListAllMyBucketsResult>");
                respond(&mut out, "200 OK", "application/xml", &[], x.as_bytes());
            }
            _ => {
                inc(&ctr::other);
                respond(&mut out, "200 OK", "", &[], b"");
            }
        }
        if !out.is_empty() {
            s.get_mut().write_all(&out).await?;
        }
        if h.close {
            s.get_mut().flush().await?;
            return Ok(());
        }
    }
}

fn dump_head(raw: &[u8]) {
    let mut d = settings().dump.lock().unwrap();
    if let Some((w, left)) = d.as_mut() {
        if *left > 0 {
            *left -= 1;
            let _ = w.write_all(raw);
            let _ = w.flush();
        }
    }
}

fn head_only(out: &mut Vec<u8>, status: &str, headers: &[&str]) {
    out.extend_from_slice(b"HTTP/1.1 ");
    out.extend_from_slice(status.as_bytes());
    out.extend_from_slice(b"\r\n");
    for h in headers {
        out.extend_from_slice(h.as_bytes());
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(b"x-amz-request-id: stub\r\n\r\n");
}

fn respond(out: &mut Vec<u8>, status: &str, ctype: &str, headers: &[&str], body: &[u8]) {
    let cl = format!("Content-Length: {}", body.len());
    let ct = format!("Content-Type: {ctype}");
    let mut hs: Vec<&str> = headers.to_vec();
    hs.push(&cl);
    if !ctype.is_empty() {
        hs.push(&ct);
    }
    head_only(out, status, &hs);
    out.extend_from_slice(body);
}

/// Writes the head in `out` plus `len` pattern bytes starting at object offset `off`.
/// Small bodies go out in the same write as the head.
async fn send_pattern<S: AsyncRead + AsyncWrite + Unpin>(
    s: &mut BufReader<S>,
    out: &mut Vec<u8>,
    mut off: u64,
    mut len: u64,
) -> std::io::Result<()> {
    let p = pattern();
    ctr::get_bytes.fetch_add(len, Relaxed);
    if len <= 64 * 1024 {
        let a = (off % PERIOD) as usize;
        out.extend_from_slice(&p[a..a + len as usize]);
        return s.get_mut().write_all(out).await;
    }
    s.get_mut().write_all(out).await?;
    while len > 0 {
        let n = len.min(CHUNK as u64) as usize;
        let a = (off % PERIOD) as usize;
        s.get_mut().write_all(&p[a..a + n]).await?;
        off += n as u64;
        len -= n as u64;
    }
    Ok(())
}

fn list_response(bucket: &str, q: &Query, out: &mut Vec<u8>) {
    let prefix = q.get("prefix").unwrap_or("");
    let delim = q.get("delimiter").unwrap_or("");
    let max = q.get("max-keys").and_then(|v| v.parse().ok()).unwrap_or(1000usize).clamp(1, 1000);
    let url = q.get("encoding-type") == Some("url");
    let token = q.get("continuation-token").map(unhex);
    let start_after = q.get("start-after").unwrap_or("");
    let after = match &token {
        Some(t) if t.as_str() > start_after => t.clone(),
        _ => start_after.to_string(),
    };
    let l = list(bucket, prefix, delim, &after, max);
    ctr::list_keys.fetch_add(l.contents.len() as u64, Relaxed);
    ctr::list_prefixes.fetch_add(l.prefixes.len() as u64, Relaxed);

    let enc = |s: &str, x: &mut String| {
        if url {
            pct_encode(s, x)
        } else {
            xml_escape(s, x)
        }
    };
    let mut x = String::with_capacity(256 + l.contents.len() * 220);
    x.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?><ListBucketResult xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\"><Name>");
    xml_escape(bucket, &mut x);
    x.push_str("</Name><Prefix>");
    enc(prefix, &mut x);
    let _ = write!(x, "</Prefix><KeyCount>{}</KeyCount><MaxKeys>{max}</MaxKeys>", l.contents.len() + l.prefixes.len());
    if !delim.is_empty() {
        x.push_str("<Delimiter>");
        enc(delim, &mut x);
        x.push_str("</Delimiter>");
    }
    if url {
        x.push_str("<EncodingType>url</EncodingType>");
    }
    let _ = write!(x, "<IsTruncated>{}</IsTruncated>", l.truncated);
    if let Some(t) = q.get("continuation-token") {
        x.push_str("<ContinuationToken>");
        xml_escape(t, &mut x);
        x.push_str("</ContinuationToken>");
    }
    if l.truncated {
        if let Some(n) = &l.next {
            let _ = write!(x, "<NextContinuationToken>{}</NextContinuationToken>", hex(n));
        }
    }
    for (k, size, mtime) in &l.contents {
        x.push_str("<Contents><Key>");
        enc(k, &mut x);
        let _ = write!(x, "</Key><LastModified>{mtime}</LastModified><ETag>&quot;stub&quot;</ETag><Size>{size}</Size><StorageClass>STANDARD</StorageClass></Contents>");
    }
    for p in &l.prefixes {
        x.push_str("<CommonPrefixes><Prefix>");
        enc(p, &mut x);
        x.push_str("</Prefix></CommonPrefixes>");
    }
    x.push_str("</ListBucketResult>");
    respond(out, "200 OK", "application/xml", &[], x.as_bytes());
}

fn control(method: &str, what: &str, q: &Query, out: &mut Vec<u8>) {
    let st = settings();
    let ok = |out: &mut Vec<u8>, body: &str| respond(out, "200 OK", "application/json", &[], body.as_bytes());
    match (method, what) {
        ("GET", "stats") => ok(out, &stats_json()),
        ("POST", "reset") => {
            stats_reset();
            ok(out, "{}");
        }
        ("POST", "config") => {
            let f = |k: &str| q.get(k).and_then(|v| v.parse::<f64>().ok());
            if let Some(v) = f("latency_ms") {
                st.latency_us.store((v * 1000.0) as u64, Relaxed);
            }
            if let Some(v) = f("conn_latency_ms") {
                st.conn_latency_us.store((v * 1000.0) as u64, Relaxed);
            }
            if let Some(v) = f("fault_rate") {
                st.fault_ppm.store((v * 1e6) as u64, Relaxed);
            }
            if let Some(v) = f("default_size") {
                st.default_size.store(v as u64, Relaxed);
            }
            if let Some(p) = q.get("dump") {
                set_dump(p, q.get("dump_max").and_then(|v| v.parse().ok()).unwrap_or(200));
            }
            ok(out, &format!(
                "{{\"latency_us\":{},\"conn_latency_us\":{},\"fault_ppm\":{},\"default_size\":{}}}",
                st.latency_us.load(Relaxed),
                st.conn_latency_us.load(Relaxed),
                st.fault_ppm.load(Relaxed),
                st.default_size.load(Relaxed)
            ));
        }
        ("PUT", "dataset") => match parse_dataset(q) {
            Ok(d) => {
                let mut ds = st.datasets.write().unwrap();
                ds.retain(|x| !(x.bucket == d.bucket && x.prefix == d.prefix));
                let msg = format!("{{\"bucket\":\"{}\",\"prefix\":\"{}\",\"count\":{}}}", d.bucket, d.prefix, d.count);
                ds.push(d);
                ok(out, &msg);
            }
            Err(e) => respond(out, "400 Bad Request", "text/plain", &[], e.as_bytes()),
        },
        ("DELETE", "dataset") => {
            let mut ds = st.datasets.write().unwrap();
            match q.get("bucket") {
                Some(b) => ds.retain(|x| x.bucket != b),
                None => ds.clear(),
            }
            ok(out, "{}");
        }
        _ => respond(out, "404 Not Found", "text/plain", &[], b"unknown control request"),
    }
}

async fn discard<R: tokio::io::AsyncBufRead + Unpin>(r: &mut R, mut n: u64) -> std::io::Result<()> {
    while n > 0 {
        let buf = r.fill_buf().await?;
        if buf.is_empty() {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        let k = (buf.len() as u64).min(n) as usize;
        r.consume(k);
        n -= k as u64;
    }
    Ok(())
}

/// Reads a chunked body, keeping it in `keep` if given; returns the payload length.
async fn read_chunked<R: tokio::io::AsyncBufRead + Unpin>(
    r: &mut R,
    mut keep: Option<&mut Vec<u8>>,
) -> std::io::Result<u64> {
    let mut total = 0;
    let mut line = String::new();
    loop {
        line.clear();
        if r.read_line(&mut line).await? == 0 {
            return Ok(total);
        }
        let size = u64::from_str_radix(line.trim().split(';').next().unwrap_or("0").trim(), 16).unwrap_or(0);
        if size == 0 {
            // Trailers up to the empty line.
            loop {
                line.clear();
                if r.read_line(&mut line).await? == 0 || line.trim().is_empty() {
                    return Ok(total);
                }
            }
        }
        match keep.as_deref_mut() {
            Some(v) => {
                let at = v.len();
                v.resize(at + size as usize, 0);
                r.read_exact(&mut v[at..]).await?;
                discard(r, 2).await?;
            }
            None => discard(r, size + 2).await?,
        }
        total += size;
    }
}

fn parse_range(h: &str, size: u64) -> Option<(u64, u64)> {
    let spec = h.trim().strip_prefix("bytes=")?;
    let (a, b) = spec.split_once('-')?;
    if a.is_empty() {
        // Suffix range: the last n bytes.
        let n: u64 = b.parse().ok()?;
        return Some((size.saturating_sub(n), size.saturating_sub(1)));
    }
    let start: u64 = a.parse().ok()?;
    let end: u64 = if b.is_empty() { size.saturating_sub(1) } else { b.parse().ok()? };
    if size == 0 || start >= size {
        return Some((1, 0)); // unsatisfiable; the caller answers 416
    }
    Some((start, end.min(size - 1)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ds(count: u64, fanout: u64) -> Dataset {
        let mut q = format!("bucket=b&prefix=p/&count={count}&width=3");
        if fanout > 0 {
            q.push_str(&format!("&fanout={fanout}&dirs=x/y/"));
        }
        parse_dataset(&Query::parse(&q)).unwrap()
    }

    #[test]
    fn keys_sorted_and_indexed() {
        let d = ds(250, 10);
        let mut prev = String::new();
        let mut k = String::new();
        for i in 0..d.count {
            d.key(i, &mut k);
            assert!(k > prev, "{k} <= {prev}");
            assert_eq!(d.index_of(&k), Some(i));
            prev = k.clone();
        }
        assert_eq!(d.index_of("p/000/x/y/obj_999.dat"), None);
    }

    #[test]
    fn time_formats() {
        assert_eq!(fmt_time(DEFAULT_MTIME, true), "2024-01-01T00:00:00.000Z");
        assert_eq!(fmt_time(DEFAULT_MTIME, false), "Mon, 01 Jan 2024 00:00:00 GMT");
        assert_eq!(fmt_time(0, false), "Thu, 01 Jan 1970 00:00:00 GMT");
    }

    #[test]
    fn listing_pages_and_prefixes() {
        {
            let mut w = settings().datasets.write().unwrap();
            w.clear();
            w.push(ds(2500, 0));
            let mut f = ds(250, 10);
            f.bucket = "f".into();
            w.push(f);
        }
        // Flat: three pages of 1000/1000/500 with tokens.
        let mut after = String::new();
        let mut n = 0;
        loop {
            let l = list("b", "p/", "", &after, 1000);
            n += l.contents.len();
            if !l.truncated {
                break;
            }
            after = l.next.unwrap();
        }
        assert_eq!(n, 2500);
        // Delimiter: 25 groups of 10 collapse to 25 common prefixes.
        let l = list("f", "p/", "/", "", 1000);
        assert_eq!(l.prefixes.len(), 25);
        assert!(l.contents.is_empty());
        // Paging over common prefixes skips whole groups.
        let l1 = list("f", "p/", "/", "", 10);
        assert!(l1.truncated);
        let l2 = list("f", "p/", "/", l1.next.as_deref().unwrap(), 1000);
        assert_eq!(l1.prefixes.len() + l2.prefixes.len(), 25);
        // A sub-prefix.
        let l = list("f", "p/003/", "", "", 1000);
        assert_eq!(l.contents.len(), 10);
        assert_eq!(parse_range("bytes=0-99", 50), Some((0, 49)));
        assert_eq!(parse_range("bytes=-10", 50), Some((40, 49)));
    }
}
