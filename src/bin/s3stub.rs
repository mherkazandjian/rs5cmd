//! Stateless in-memory "S3" stub for benchmarking the *client* in isolation.
//!
//! It implements just enough of the S3 REST surface (path-style) for s5cmd /
//! rs5cmd transfer benchmarks, with no disk and no state:
//!   - PUT  /bucket            -> CreateBucket (200)
//!   - PUT  /bucket/key        -> PutObject (read+discard body, 200 + ETag)
//!   - GET  /bucket?list-type=2-> ListObjectsV2 (XML, STUB_LIST_COUNT synthetic keys)
//!   - GET  /bucket/key        -> GetObject (200 + STUB_OBJECT_SIZE bytes; 206 for Range)
//!   - HEAD /bucket/key        -> 200 + Content-Length
//!   - POST /bucket?delete     -> DeleteObjects (200 + minimal XML)
//!   - DELETE /bucket[/key]    -> 204
//!
//! Auth headers are ignored. Bodies are consumed (Content-Length or chunked) so
//! connections stay in sync for keep-alive. Build/run:
//!   cargo run --release --features bench-stub --bin s3stub
//! Env: STUB_PORT (9100), STUB_OBJECT_SIZE (4096), STUB_LIST_COUNT (10000).

use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::Arc;

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;

// Connection stats (enabled with STUB_STATS=1) — used to probe how much
// concurrency a client actually drives at the TCP level.
static ACTIVE: AtomicI64 = AtomicI64::new(0);
static PEAK: AtomicI64 = AtomicI64::new(0);
static TOTAL: AtomicU64 = AtomicU64::new(0);
static REQS: AtomicU64 = AtomicU64::new(0);

struct Config {
    object_size: usize,
    list_count: usize,
    body: Vec<u8>, // preallocated GET payload of object_size bytes
}

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let port: u16 = std::env::var("STUB_PORT").ok().and_then(|s| s.parse().ok()).unwrap_or(9100);
    let object_size: usize = std::env::var("STUB_OBJECT_SIZE").ok().and_then(|s| s.parse().ok()).unwrap_or(4096);
    let list_count: usize = std::env::var("STUB_LIST_COUNT").ok().and_then(|s| s.parse().ok()).unwrap_or(10000);

    let cfg = Arc::new(Config {
        object_size,
        list_count,
        body: vec![b'x'; object_size],
    });

    let listener = TcpListener::bind(("0.0.0.0", port)).await?;
    eprintln!("s3stub listening on :{port} (object_size={object_size}, list_count={list_count})");

    let stats = std::env::var("STUB_STATS").is_ok();
    if stats {
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                eprintln!(
                    "[stub] active_conns={} peak_conns={} total_conns={} reqs={}",
                    ACTIVE.load(Ordering::Relaxed),
                    PEAK.load(Ordering::Relaxed),
                    TOTAL.load(Ordering::Relaxed),
                    REQS.load(Ordering::Relaxed),
                );
            }
        });
    }

    loop {
        let (stream, _) = listener.accept().await?;
        let cfg = Arc::clone(&cfg);
        let n = ACTIVE.fetch_add(1, Ordering::Relaxed) + 1;
        TOTAL.fetch_add(1, Ordering::Relaxed);
        PEAK.fetch_max(n, Ordering::Relaxed);
        tokio::spawn(async move {
            let _ = handle_conn(stream, cfg).await;
            ACTIVE.fetch_sub(1, Ordering::Relaxed);
        });
    }
}

async fn handle_conn(stream: tokio::net::TcpStream, cfg: Arc<Config>) -> std::io::Result<()> {
    let _ = stream.set_nodelay(true);
    let (rh, mut wh) = stream.into_split();
    let mut reader = BufReader::new(rh);

    loop {
        // Request line.
        let mut line = String::new();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            return Ok(()); // client closed
        }
        let line = line.trim_end();
        if line.is_empty() {
            continue;
        }
        REQS.fetch_add(1, Ordering::Relaxed);
        let mut it = line.split_whitespace();
        let method = it.next().unwrap_or("").to_string();
        let target = it.next().unwrap_or("/").to_string();

        // Headers.
        let mut headers: HashMap<String, String> = HashMap::new();
        loop {
            let mut h = String::new();
            let hn = reader.read_line(&mut h).await?;
            if hn == 0 {
                return Ok(());
            }
            let h = h.trim_end();
            if h.is_empty() {
                break;
            }
            if let Some((k, v)) = h.split_once(':') {
                headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
            }
        }

        let keep_alive = headers
            .get("connection")
            .map(|c| !c.eq_ignore_ascii_case("close"))
            .unwrap_or(true);

        // Honor Expect: 100-continue before draining the body.
        if headers
            .get("expect")
            .map(|e| e.eq_ignore_ascii_case("100-continue"))
            .unwrap_or(false)
        {
            wh.write_all(b"HTTP/1.1 100 Continue\r\n\r\n").await?;
        }

        // Drain the request body so the stream stays framed for keep-alive.
        if let Some(cl) = headers.get("content-length").and_then(|v| v.parse::<usize>().ok()) {
            discard_n(&mut reader, cl).await?;
        } else if headers
            .get("transfer-encoding")
            .map(|t| t.to_ascii_lowercase().contains("chunked"))
            .unwrap_or(false)
        {
            discard_chunked(&mut reader).await?;
        }

        // Route on method + path (path-style: /bucket[/key]).
        let (path, query) = match target.split_once('?') {
            Some((p, q)) => (p, q),
            None => (target.as_str(), ""),
        };
        let trimmed = path.trim_start_matches('/');
        let (_bucket, key) = match trimmed.split_once('/') {
            Some((b, k)) => (b, k),
            None => (trimmed, ""),
        };
        let is_object = !key.is_empty();

        match method.as_str() {
            "PUT" if is_object => {
                wh.write_all(b"HTTP/1.1 200 OK\r\nETag: \"stub\"\r\nContent-Length: 0\r\n\r\n")
                    .await?;
            }
            "PUT" => {
                // CreateBucket
                wh.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n").await?;
            }
            "POST" if query.contains("delete") => {
                let xml = b"<?xml version=\"1.0\" encoding=\"UTF-8\"?><DeleteResult></DeleteResult>";
                write_body(&mut wh, 200, "OK", "application/xml", xml).await?;
            }
            "DELETE" => {
                wh.write_all(b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n").await?;
            }
            "HEAD" if is_object => {
                let h = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: application/octet-stream\r\nETag: \"stub\"\r\n\r\n",
                    cfg.object_size
                );
                wh.write_all(h.as_bytes()).await?;
            }
            "GET" if !is_object && query.contains("list-type=2") => {
                let prefix = query
                    .split('&')
                    .find_map(|kv| kv.strip_prefix("prefix="))
                    .map(url_decode)
                    .unwrap_or_default();
                let xml = list_xml(&prefix, cfg.list_count, cfg.object_size);
                write_body(&mut wh, 200, "OK", "application/xml", xml.as_bytes()).await?;
            }
            "GET" if is_object => {
                if let Some(range) = headers.get("range").and_then(|r| parse_range(r, cfg.object_size)) {
                    let (start, end) = range;
                    let len = end - start + 1;
                    let h = format!(
                        "HTTP/1.1 206 Partial Content\r\nContent-Length: {len}\r\nContent-Range: bytes {start}-{end}/{}\r\nContent-Type: application/octet-stream\r\n\r\n",
                        cfg.object_size
                    );
                    wh.write_all(h.as_bytes()).await?;
                    wh.write_all(&cfg.body[..len.min(cfg.body.len())]).await?;
                } else {
                    let h = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: application/octet-stream\r\nETag: \"stub\"\r\n\r\n",
                        cfg.object_size
                    );
                    wh.write_all(h.as_bytes()).await?;
                    wh.write_all(&cfg.body).await?;
                }
            }
            _ => {
                wh.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n").await?;
            }
        }

        if !keep_alive {
            return Ok(());
        }
    }
}

async fn write_body<W: AsyncWriteExt + Unpin>(
    w: &mut W,
    code: u16,
    reason: &str,
    ctype: &str,
    body: &[u8],
) -> std::io::Result<()> {
    let h = format!(
        "HTTP/1.1 {code} {reason}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\n\r\n",
        body.len()
    );
    w.write_all(h.as_bytes()).await?;
    w.write_all(body).await
}

async fn discard_n<R: AsyncReadExt + Unpin>(r: &mut R, mut n: usize) -> std::io::Result<()> {
    let mut scratch = [0u8; 65536];
    while n > 0 {
        let want = n.min(scratch.len());
        let got = r.read(&mut scratch[..want]).await?;
        if got == 0 {
            break;
        }
        n -= got;
    }
    Ok(())
}

async fn discard_chunked<R: AsyncBufReadExt + Unpin>(r: &mut R) -> std::io::Result<()> {
    loop {
        let mut size_line = String::new();
        if r.read_line(&mut size_line).await? == 0 {
            return Ok(());
        }
        // Chunk size is hex, optionally with ;ext.
        let hex = size_line.trim().split(';').next().unwrap_or("0");
        let size = usize::from_str_radix(hex.trim(), 16).unwrap_or(0);
        if size == 0 {
            // Consume trailing CRLF / trailers up to blank line.
            let mut t = String::new();
            let _ = r.read_line(&mut t).await?;
            return Ok(());
        }
        discard_n(r, size + 2).await?; // data + CRLF
    }
}

fn parse_range(h: &str, size: usize) -> Option<(usize, usize)> {
    let spec = h.strip_prefix("bytes=")?;
    let (a, b) = spec.split_once('-')?;
    let start: usize = a.parse().ok()?;
    let end: usize = if b.is_empty() { size - 1 } else { b.parse().ok()? };
    Some((start, end.min(size.saturating_sub(1))))
}

fn list_xml(prefix: &str, count: usize, size: usize) -> String {
    let mut s = String::with_capacity(count * 200 + 256);
    s.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?><ListBucketResult xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\"><IsTruncated>false</IsTruncated>");
    for i in 0..count {
        s.push_str("<Contents><Key>");
        s.push_str(prefix);
        s.push_str(&format!("obj_{i:06}.dat"));
        s.push_str("</Key><LastModified>2024-01-01T00:00:00.000Z</LastModified><ETag>&quot;stub&quot;</ETag><Size>");
        s.push_str(&size.to_string());
        s.push_str("</Size><StorageClass>STANDARD</StorageClass></Contents>");
    }
    s.push_str("<KeyCount>");
    s.push_str(&count.to_string());
    s.push_str("</KeyCount></ListBucketResult>");
    s
}

/// Minimal percent-decode for the prefix query value.
fn url_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' if i + 2 < b.len() => {
                let h = u8::from_str_radix(&s[i + 1..i + 3], 16).unwrap_or(b'%');
                out.push(h);
                i += 3;
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
