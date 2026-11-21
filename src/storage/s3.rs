//! S3 `Storage` implementation backed by the official `aws-sdk-s3` crate.
//! Ported from s5cmd's `storage/s3.go` (core operations).

use std::path::Path;

use async_trait::async_trait;
use aws_sdk_s3::config::{BehaviorVersion, Region};
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::types::{
    CompletedMultipartUpload, CompletedPart, Delete, EncodingType, ObjectIdentifier,
};
use aws_sdk_s3::Client;
use futures::stream::{self, StreamExt};
use tokio::sync::mpsc;

use super::partial::PartialFile;
use super::url::Url;
use super::{
    Bucket, Metadata, NoObjectFound, Object, ObjectNotFound, ObjectType, Options, StorageClass,
    Storage,
};
use crate::ratelimit::RateLimiter;

/// Max keys per DeleteObjects request (S3 API limit).
const DELETE_CHUNK_SIZE: usize = 1000;

/// Process-wide counter for unique `--client-copy` temp file names.
static CLIENT_COPY_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Hard S3 limit on the source size of a single `CopyObject`: 5 GiB. Sources
/// larger than this must be copied server-side with multipart `UploadPartCopy`.
const DEFAULT_MULTIPART_COPY_THRESHOLD: u64 = 5 * 1024 * 1024 * 1024;

/// Max parts in a multipart upload/copy (S3 API limit). Used to grow the copy
/// part size for very large sources so the part count stays within bounds.
const MAX_MULTIPART_PARTS: u64 = 10_000;

/// Returns true if a `CopyObject` error indicates the source exceeds the 5 GiB
/// single-copy limit, so we should retry via multipart copy. Matches the S3
/// message ("larger than the maximum allowable size for a copy source") and the
/// `EntityTooLarge` code, case-insensitively against the full Display chain.
fn is_copy_source_too_large(e: &anyhow::Error) -> bool {
    let m = format!("{e:#}").to_ascii_lowercase();
    m.contains("larger than the maximum allowable size") || m.contains("entitytoolarge")
}

/// True if a write error is a failed conditional write (`If-None-Match: "*"`
/// against an existing object -> HTTP 412 `PreconditionFailed`). Matched
/// case-insensitively against the full Debug/Display chain so it is robust
/// across SDK error shapes (#752).
fn is_precondition_failed<E: std::fmt::Debug>(e: &E) -> bool {
    let m = format!("{e:?}").to_ascii_lowercase();
    m.contains("preconditionfailed") || m.contains("precondition failed")
}

/// Maps an SDK write error to a typed [`crate::storage::PreconditionFailedError`]
/// when it is a 412 `PreconditionFailed` (failed `If-None-Match`), so callers can
/// treat it as "object already exists, skipped" instead of a hard error (#752);
/// otherwise returns the original error unchanged.
fn map_conditional_write_error(e: anyhow::Error, dst: &Url) -> anyhow::Error {
    if is_precondition_failed(&e) {
        anyhow::Error::new(crate::storage::PreconditionFailedError { url: dst.to_string() })
    } else {
        e
    }
}

/// Streams a response body into `file` from `offset`, chunk by chunk as it arrives (E2: no
/// buffer of a whole part), pacing each received chunk through the download limiter (F05).
/// Returns the offset after the last byte written.
async fn write_body(
    mut body: ByteStream,
    file: &std::fs::File,
    mut offset: u64,
    limiter: Option<&RateLimiter>,
) -> anyhow::Result<u64> {
    use std::os::unix::fs::FileExt;
    while let Some(chunk) = body.try_next().await? {
        if let Some(limiter) = limiter {
            limiter.acquire(chunk.len() as u64).await;
        }
        file.write_all_at(&chunk, offset)?;
        offset += chunk.len() as u64;
    }
    Ok(offset)
}

/// The fewest download parts in flight the process-wide budget allows: the default
/// `--numworkers`. Below it, workers x `--concurrency` stays small anyway, and a smaller budget
/// would only slow down a few large downloads at once.
const MIN_PARTS_IN_FLIGHT: usize = 256;

/// Download parts in flight over the whole process, beyond each object's first part.
static PARTS_IN_FLIGHT: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(MIN_PARTS_IN_FLIGHT);

/// Sets the process-wide budget of download parts in flight from `--numworkers` (256 at least).
/// Takes effect if called before the first multipart download, as `main` does.
pub fn set_download_parts_in_flight(numworkers: usize) {
    PARTS_IN_FLIGHT.store(
        numworkers.max(MIN_PARTS_IN_FLIGHT),
        std::sync::atomic::Ordering::Relaxed,
    );
}

/// One permit per download part in flight. Each connection holds about twice what its reads
/// return while its part streams (~128 KiB once many are open: `FillingReads`), so with
/// --numworkers objects of several parts each, the parts in
/// flight bound the memory: 256 workers x 8 parts opened 2,048 connections.
struct PartBudget {
    sem: tokio::sync::Semaphore,
    size: std::sync::Mutex<usize>,
}

impl PartBudget {
    fn new(n: usize) -> Self {
        PartBudget {
            sem: tokio::sync::Semaphore::new(n),
            size: std::sync::Mutex::new(n),
        }
    }

    fn global() -> &'static PartBudget {
        static B: std::sync::OnceLock<PartBudget> = std::sync::OnceLock::new();
        B.get_or_init(|| {
            PartBudget::new(PARTS_IN_FLIGHT.load(std::sync::atomic::Ordering::Relaxed))
        })
    }

    /// The budget, raised to at least `n` permits first: a `--concurrency` above `--numworkers`
    /// still gets its parts.
    fn at_least(&self, n: usize) -> &tokio::sync::Semaphore {
        let mut size = self.size.lock().unwrap_or_else(|e| e.into_inner());
        if n > *size {
            self.sem.add_permits(n - *size);
            *size = n;
        }
        &self.sem
    }
}

/// Parses the total object size from a `Content-Range: bytes START-END/TOTAL`
/// header value (the part after the final `/`).
fn parse_content_range_total(cr: Option<&str>) -> Option<u64> {
    cr?.rsplit('/').next()?.trim().parse::<u64>().ok()
}

/// Percent-decodes a string echoed back by S3 when the list request was made
/// with `EncodingType=Url`. We must request URL encoding so that keys containing
/// XML-illegal control characters (e.g. ESC `0x1b`) can be represented in the
/// XML response and deserialized at all (upstream s5cmd #677); the echoed keys,
/// prefixes and pagination markers then have to be decoded back to their raw
/// bytes before use. `%XX` sequences are decoded as bytes and the result is
/// interpreted as UTF-8 (lossily, to never panic on malformed input); a lone or
/// malformed `%` is left verbatim.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        // A '%' followed by two hex digits decodes to a single byte.
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(h), Some(l)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2])) {
                out.push((h << 4) | l);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Returns the numeric value of an ASCII hex digit, or `None` if not hex.
fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// True if a GetObject error indicates the requested range was not satisfiable
/// (HTTP 416) — e.g. a zero-byte object probed with a `bytes=0-N` range. Matched
/// against the Debug chain; misclassification is harmless because the caller
/// falls back to a plain (correct) GET on a match.
fn is_range_not_satisfiable<E: std::fmt::Debug>(e: &E) -> bool {
    let m = format!("{e:?}").to_ascii_lowercase();
    m.contains("rangenotsatisfiable")
        || m.contains("invalidrange")
        || m.contains("range not satisfiable")
        || m.contains("requested range not satisfiable")
}

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// Loads a PEM bundle of CA certificates from `path` (for `--ca-certs-file`).
fn load_ca_bundle(path: &str) -> anyhow::Result<Vec<rustls::pki_types::CertificateDer<'static>>> {
    let pem = std::fs::read(path).map_err(|e| anyhow::anyhow!("ca-certs-file {path}: {e}"))?;
    let certs = rustls_pemfile::certs(&mut pem.as_slice())
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| anyhow::anyhow!("parsing ca-certs-file {path}: {e}"))?;
    if certs.is_empty() {
        anyhow::bail!("ca-certs-file {path} contained no certificates");
    }
    Ok(certs)
}

/// The SDK's default region chain without its last step, the EC2 instance-metadata probe: the
/// environment, then the profile file (`--profile` or AWS_PROFILE). Outside EC2 the probe waited
/// about 3 s for a timeout on every invocation that had no region configured (#20). s5cmd never
/// asks instance metadata either; it detects the bucket's region (see `bucket_region`).
pub(crate) fn region_chain(profile: Option<&str>) -> aws_config::meta::region::RegionProviderChain {
    use aws_config::environment::region::EnvironmentVariableRegionProvider;
    use aws_config::meta::region::RegionProviderChain;
    use aws_config::profile::region::ProfileFileRegionProvider;
    let mut from_profile = ProfileFileRegionProvider::builder();
    if let Some(name) = profile {
        from_profile = from_profile.profile_name(name);
    }
    RegionProviderChain::first_try(EnvironmentVariableRegionProvider::new())
        .or_else(from_profile.build())
}

/// The configured region: `--region`, else the environment, else the profile file; `None` when
/// none is set. Config loaders get the result as a fixed region (us-east-1 when `None`): given
/// no region, the SDK's default credentials chain resolves one itself, instance metadata
/// included, which cost two 1 s timeouts outside EC2 (#20).
pub(crate) async fn configured_region(
    region: Option<&str>,
    profile: Option<&str>,
) -> Option<Region> {
    match region {
        Some(r) => Some(Region::new(r.to_string())),
        None => region_chain(profile).region().await,
    }
}

/// The region a bucket reports in its `x-amz-bucket-region` header (HeadBucket answers with it on
/// success, and on the 301/400/403 a wrong-region or unauthorized request gets). Asked once per
/// (endpoint, bucket) for the process; `None` when the request fails without the header.
async fn bucket_region(client: &Client, endpoint: &str, bucket: &str) -> Option<String> {
    static CACHE: tokio::sync::Mutex<Vec<((String, String), Option<String>)>> =
        tokio::sync::Mutex::const_new(Vec::new());
    let key = (endpoint.to_string(), bucket.to_string());
    let mut cache = CACHE.lock().await;
    if let Some((_, r)) = cache.iter().find(|(k, _)| *k == key) {
        return r.clone();
    }
    let r = match client.head_bucket().bucket(bucket).send().await {
        Ok(out) => out.bucket_region().map(str::to_owned),
        Err(e) => e
            .raw_response()
            .and_then(|resp| resp.headers().get("x-amz-bucket-region"))
            .map(str::to_owned),
    };
    cache.push((key, r.clone()));
    r
}

/// The HTTP client of every S3 client in the process, one per distinct (no-verify, proxy, CA
/// bundle), so they share connection pools: the SDK builds its default client per S3 client, which
/// opened 10,001 connections for a 10k-line `run` file (#19).
fn cached_http_client(
    no_verify: bool,
    proxy: Option<ProxyConfig>,
    ca_certs: Option<&str>,
) -> anyhow::Result<aws_smithy_runtime_api::client::http::SharedHttpClient> {
    use aws_smithy_runtime_api::client::http::SharedHttpClient;
    static CACHE: std::sync::Mutex<Vec<(String, SharedHttpClient)>> =
        std::sync::Mutex::new(Vec::new());
    let key = format!("{no_verify}\u{0}{proxy:?}\u{0}{ca_certs:?}");
    if let Some((_, c)) = CACHE.lock().unwrap().iter().find(|(k, _)| *k == key) {
        return Ok(c.clone());
    }
    let client = build_http_client(no_verify, proxy, ca_certs)?;
    let mut cache = CACHE.lock().unwrap();
    if let Some((_, c)) = cache.iter().find(|(k, _)| *k == key) {
        return Ok(c.clone());
    }
    cache.push((key, client.clone()));
    Ok(client)
}

/// rs5cmd's HTTP client for S3 requests: hyper 1 over rustls (aws-lc-rs).
///
/// - HTTP/1.1 only, as S3 itself and s5cmd's Go transport. The SDK's client also offers HTTP/2,
///   and behind an HTTP/2 front end hyper then sends every request to the endpoint over one
///   connection: one TCP flow and one thread for all parts of a download, which capped a 100 GB
///   download at ~700 MB/s where s5cmd did 1.6 GB/s (#14).
/// - Trust: the platform's roots, `SSL_CERT_FILE` and `SSL_CERT_DIR` included, as the SDK's
///   client (read on the first TLS handshake: `platform_trust`); with a proxy or
///   `--ca-certs-file`, the bundled webpki roots plus the bundle;
///   `--no-verify-ssl` verifies nothing (no hostname check either: the intended effect).
/// - As the SDK's client: TCP_NODELAY, the SDK's connect timeout (3.1 s unless configured; TLS
///   handshake included), idle connections closed after 90 s, and errors classified so the SDK
///   retries the same ones (`connector_error`).
/// - Reads over TLS gather what is ready, up to 64 KiB (`FillingReads`), instead of one TLS record;
///   reads on plain connections return up to 2 MiB shared by the connections open (64-512 KiB).
///   Either bounds each connection's read buffer.
/// - Crypto: aws-lc-rs, as the SDK's client: it offers post-quantum hybrid key exchange
///   (X25519MLKEM768) first, and its AES-GCM costs less CPU than ring's.
fn build_http_client(
    no_verify: bool,
    proxy: Option<ProxyConfig>,
    ca_certs: Option<&str>,
) -> anyhow::Result<aws_smithy_runtime_api::client::http::SharedHttpClient> {
    use aws_smithy_runtime_api::client::http::SharedHttpClient;
    use hyper_rustls::HttpsConnectorBuilder;
    use hyper_util::client::legacy::connect::HttpConnector;
    use std::sync::Arc;

    // Process-wide default crypto provider (idempotent; the fast path may also
    // install one). Ignore the error if it's already installed.
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());

    let tls = if no_verify {
        // Verification disabled — a custom CA bundle would be moot, so skip it.
        rustls::ClientConfig::builder_with_provider(provider.clone())
            .with_safe_default_protocol_versions()
            .expect("rustls safe default protocol versions")
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(danger::NoVerify(provider)))
            .with_no_client_auth()
    } else if proxy.is_none() && ca_certs.is_none() {
        // What the SDK's client trusted, loaded on the first TLS handshake.
        rustls::ClientConfig::builder_with_provider(provider.clone())
            .with_safe_default_protocol_versions()
            .expect("rustls safe default protocol versions")
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(platform_trust::Verifier::new(provider)))
            .with_no_client_auth()
    } else {
        let mut roots = rustls::RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        // `--ca-certs-file`: trust additional (e.g. private/self-signed) CAs on
        // top of the bundled webpki roots.
        if let Some(path) = ca_certs {
            for cert in load_ca_bundle(path)? {
                roots
                    .add(cert)
                    .map_err(|e| anyhow::anyhow!("adding ca-certs-file cert: {e}"))?;
            }
        }
        rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .expect("rustls safe default protocol versions")
            .with_root_certificates(roots)
            .with_no_client_auth()
    };

    // HTTP/1.1 alone: hyper-rustls then offers no ALPN, so no server can choose HTTP/2.
    let https = HttpsConnectorBuilder::new()
        .with_tls_config(tls)
        .https_or_http()
        .enable_http1();
    Ok(match proxy {
        None => {
            let mut tcp = HttpConnector::new();
            tcp.enforce_http(false);
            tcp.set_nodelay(true);
            SharedHttpClient::new(SdkHyperClient::new(https.wrap_connector(tcp)))
        }
        Some(p) => SharedHttpClient::new(SdkHyperClient::new(
            https.wrap_connector(ProxyConnector { proxy: p }),
        )),
    })
}

/// A smithy `HttpClient` over a pooled hyper-util client on connector `C` (direct or through a
/// proxy). The connect timeout comes with the SDK's settings, so the client is built when the
/// SDK first asks for a connector: one per distinct timeout, one in practice.
struct SdkHyperClient<C> {
    connector: C,
    clients: std::sync::Mutex<
        Vec<(
            Option<std::time::Duration>,
            aws_smithy_runtime_api::client::http::SharedHttpConnector,
        )>,
    >,
}

impl<C> SdkHyperClient<C> {
    fn new(connector: C) -> Self {
        SdkHyperClient {
            connector,
            clients: std::sync::Mutex::new(Vec::new()),
        }
    }
}

impl<C> std::fmt::Debug for SdkHyperClient<C> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SdkHyperClient")
    }
}

impl<C> aws_smithy_runtime_api::client::http::HttpClient for SdkHyperClient<C>
where
    C: tower_service::Service<http::Uri> + Clone + Send + Sync + 'static,
    C::Response: hyper::rt::Read
        + hyper::rt::Write
        + hyper_util::client::legacy::connect::Connection
        + Send
        + Unpin
        + 'static,
    C::Future: Send + 'static,
    C::Error: Into<BoxError>,
{
    fn http_connector(
        &self,
        settings: &aws_smithy_runtime_api::client::http::HttpConnectorSettings,
        _components: &aws_smithy_runtime_api::client::runtime_components::RuntimeComponents,
    ) -> aws_smithy_runtime_api::client::http::SharedHttpConnector {
        use aws_smithy_runtime_api::client::http::SharedHttpConnector;
        use hyper_util::client::legacy::Client;
        use hyper_util::rt::{TokioExecutor, TokioTimer};

        let timeout = settings.connect_timeout();
        let mut clients = self.clients.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((_, c)) = clients.iter().find(|(t, _)| *t == timeout) {
            return c.clone();
        }
        let mut builder = Client::builder(TokioExecutor::new());
        // With a timer the pool closes connections idle for 90 s (hyper-util's default).
        builder.pool_timer(TokioTimer::new());
        let client = builder.build(ConnectTimeout {
            inner: self.connector.clone(),
            timeout,
        });
        let c = SharedHttpConnector::new(SdkHyperConnector {
            client: std::sync::Arc::new(client),
        });
        clients.push((timeout, c.clone()));
        c
    }
}

/// Adapts a hyper 1.x legacy `Client` to the smithy `HttpConnector` trait,
/// mirroring `aws-smithy-http-client`'s internal `Adapter`.
struct SdkHyperConnector<C> {
    client: std::sync::Arc<
        hyper_util::client::legacy::Client<ConnectTimeout<C>, aws_smithy_types::body::SdkBody>,
    >,
}

impl<C> std::fmt::Debug for SdkHyperConnector<C> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SdkHyperConnector")
    }
}

impl<C> aws_smithy_runtime_api::client::http::HttpConnector for SdkHyperConnector<C>
where
    C: tower_service::Service<http::Uri> + Clone + Send + Sync + 'static,
    C::Response: hyper::rt::Read
        + hyper::rt::Write
        + hyper_util::client::legacy::connect::Connection
        + Send
        + Unpin
        + 'static,
    C::Future: Send + 'static,
    C::Error: Into<BoxError>,
{
    fn call(
        &self,
        request: aws_smithy_runtime_api::client::orchestrator::HttpRequest,
    ) -> aws_smithy_runtime_api::client::http::HttpConnectorFuture {
        use aws_smithy_runtime_api::client::http::HttpConnectorFuture;
        use aws_smithy_runtime_api::client::orchestrator::HttpResponse;
        use aws_smithy_runtime_api::client::result::ConnectorError;
        use aws_smithy_types::body::SdkBody;

        let request: http::Request<SdkBody> = match request.try_into() {
            Ok(req) => req,
            Err(err) => return HttpConnectorFuture::ready(Err(ConnectorError::user(err.into()))),
        };
        let client = self.client.clone();
        HttpConnectorFuture::new(async move {
            let response = client.request(request).await.map_err(connector_error)?;
            let (parts, body) = response.into_parts();
            let body = SdkBody::from_body_1_x(body);
            HttpResponse::try_from(http::Response::from_parts(parts, body))
                .map_err(|err| ConnectorError::other(err.into(), None))
        })
    }
}

/// The SDK's connect timeout around a connector, TLS handshake included (as the SDK's client
/// times it). Without one a connection to an unreachable host waited for the kernel's SYN
/// retries, about two minutes. Its connections read as `FillingReads`.
#[derive(Clone)]
struct ConnectTimeout<C> {
    inner: C,
    timeout: Option<std::time::Duration>,
}

#[derive(Debug)]
struct ConnectTimedOut(std::time::Duration);

impl std::fmt::Display for ConnectTimedOut {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "no connection within {:?}", self.0)
    }
}

impl std::error::Error for ConnectTimedOut {}

impl<C> tower_service::Service<http::Uri> for ConnectTimeout<C>
where
    C: tower_service::Service<http::Uri>,
    C::Response: Send + 'static,
    C::Future: Send + 'static,
    C::Error: Into<BoxError>,
{
    type Response = FillingReads<C::Response>;
    type Error = BoxError;
    type Future = std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Self::Response, BoxError>> + Send>,
    >;

    fn poll_ready(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), BoxError>> {
        self.inner.poll_ready(cx).map_err(Into::into)
    }

    fn call(&mut self, uri: http::Uri) -> Self::Future {
        let fill = uri.scheme() == Some(&http::uri::Scheme::HTTPS);
        let connect = self.inner.call(uri);
        let timeout = self.timeout;
        Box::pin(async move {
            let io = match timeout {
                None => connect.await.map_err(Into::into)?,
                Some(d) => match tokio::time::timeout(d, connect).await {
                    Ok(r) => r.map_err(Into::into)?,
                    Err(_) => return Err(Box::new(ConnectTimedOut(d)) as BoxError),
                },
            };
            Ok(FillingReads {
                io,
                fill,
                deferred: None,
                _open: OpenConnection::new(),
            })
        })
    }
}

/// A connection whose reads over TLS go on while more is ready, up to `FILL_LIMIT`. A TLS read
/// returns one record (16 KiB at most) however much more has arrived, so a body came in 16 KiB
/// chunks, each one write into the downloaded file under the file's lock; the runtime workers
/// waiting on it left too few to decrypt (one 4 GiB object: 2.2 s, s5cmd 1.5 s). Plain
/// connections read once per call (their reads already return what is ready, and another read
/// there would find nothing), up to `plain_read_limit`.
struct FillingReads<T> {
    io: T,
    fill: bool,
    /// An error met after some bytes were read: the next read returns it.
    deferred: Option<std::io::Error>,
    _open: OpenConnection,
}

/// The most one read over TLS returns: 64 KiB (four TLS records). hyper's read buffer settles at
/// twice what its reads return, so this bounds each connection's buffer near 128 KiB. Reading up
/// to hyper's whole buffer (~400 KiB) was faster still for one object over TLS but tripled the
/// memory of downloads over many connections, and wrote data already gone from the CPU's cache.
const FILL_LIMIT: usize = 64 << 10;

/// What reads on plain connections may return together: each returns this much shared by the
/// connections open (`plain_read_limit`). Uncapped, plain connections held ~400 KiB each, so 256
/// downloads of 1 MiB objects peaked at 4x s5cmd's memory; capped at `FILL_LIMIT` for all, one
/// object over a few connections came 12 % slower (8 parts at a time) and `cat` 2.3x slower.
const PLAIN_READ_BUDGET: usize = 2 << 20;

/// More than hyper's largest read buffer (~400 KiB): with four connections open or fewer, plain
/// reads are not capped.
const PLAIN_READ_MAX: usize = 512 << 10;

/// The most one read on a plain connection returns while `open` connections are open: 64 KiB
/// from 32 of them on, uncapped up to 4.
fn plain_read_limit(open: usize) -> usize {
    (PLAIN_READ_BUDGET / open.max(1)).clamp(FILL_LIMIT, PLAIN_READ_MAX)
}

/// The connections of rs5cmd's HTTP client open now, idle ones in the pool included: each keeps
/// its read buffer.
static OPEN_CONNECTIONS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Counts one connection in `OPEN_CONNECTIONS` while it lives.
struct OpenConnection;

impl OpenConnection {
    fn new() -> Self {
        OPEN_CONNECTIONS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        OpenConnection
    }
}

impl Drop for OpenConnection {
    fn drop(&mut self) {
        OPEN_CONNECTIONS.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    }
}

impl<T: hyper::rt::Read + Unpin> hyper::rt::Read for FillingReads<T> {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        mut buf: hyper::rt::ReadBufCursor<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        use std::pin::Pin;
        use std::task::Poll;
        let this = self.get_mut();
        if let Some(e) = this.deferred.take() {
            return Poll::Ready(Err(e));
        }
        let limit = if this.fill {
            FILL_LIMIT
        } else {
            plain_read_limit(OPEN_CONNECTIONS.load(std::sync::atomic::Ordering::Relaxed))
        };
        let capacity = buf.remaining().min(limit);
        if capacity == 0 {
            return Pin::new(&mut this.io).poll_read(cx, buf);
        }
        // SAFETY: the inner reader only writes into this unfilled part, and only the bytes it
        // reports filled (so initialized) are advanced over below.
        let mut part = hyper::rt::ReadBuf::uninit(unsafe { &mut buf.as_mut()[..capacity] });
        let mut result = Poll::Pending;
        loop {
            let before = part.filled().len();
            match Pin::new(&mut this.io).poll_read(cx, part.unfilled()) {
                Poll::Ready(Ok(())) => {
                    result = Poll::Ready(Ok(()));
                    let now = part.filled().len();
                    // A plain connection reads once; over TLS, until the end of the stream or a
                    // full buffer.
                    if !this.fill || now == before || now == capacity {
                        break;
                    }
                }
                Poll::Ready(Err(e)) => {
                    if part.filled().is_empty() {
                        result = Poll::Ready(Err(e));
                    } else {
                        this.deferred = Some(e);
                    }
                    break;
                }
                // Nothing more ready: return what was read, if anything (the inner reader has
                // registered the waker either way).
                Poll::Pending => break,
            }
        }
        let n = part.filled().len();
        // SAFETY: `n` bytes were filled by the inner reader.
        unsafe { buf.advance(n) };
        result
    }
}

impl<T: hyper::rt::Write + Unpin> hyper::rt::Write for FillingReads<T> {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::pin::Pin::new(&mut self.get_mut().io).poll_write(cx, buf)
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.get_mut().io).poll_flush(cx)
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.get_mut().io).poll_shutdown(cx)
    }

    fn is_write_vectored(&self) -> bool {
        self.io.is_write_vectored()
    }

    fn poll_write_vectored(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        bufs: &[std::io::IoSlice<'_>],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::pin::Pin::new(&mut self.get_mut().io).poll_write_vectored(cx, bufs)
    }
}

impl<T> hyper_util::client::legacy::connect::Connection for FillingReads<T>
where
    T: hyper_util::client::legacy::connect::Connection,
{
    fn connected(&self) -> hyper_util::client::legacy::connect::Connected {
        self.io.connected()
    }
}

/// A hyper error as the SDK's retry policy needs it, classified as the SDK's own client does
/// (`aws-smithy-http-client`): timeouts and I/O errors (a refused, reset or closed connection) are
/// retried, a response cut short is transient, an error of the request body is not retried. All
/// were "other" before, which the SDK does not retry: the `--no-verify-ssl`, `--proxy` and
/// `--ca-certs-file` clients gave up on the first dropped connection.
fn connector_error(
    err: hyper_util::client::legacy::Error,
) -> aws_smithy_runtime_api::client::result::ConnectorError {
    use aws_smithy_runtime_api::client::result::ConnectorError;
    use aws_smithy_types::retry::ErrorKind;
    use std::error::Error;

    fn find<'a, E: Error + 'static>(mut e: &'a (dyn Error + 'static)) -> Option<&'a E> {
        loop {
            if let Some(found) = e.downcast_ref::<E>() {
                return Some(found);
            }
            e = e.source()?;
        }
    }
    enum Kind {
        Timeout,
        Io,
        User,
        Transient,
        Other,
    }
    let kind = if find::<ConnectTimedOut>(&err).is_some() {
        Kind::Timeout
    } else if let Some(h) = find::<hyper::Error>(&err) {
        if h.is_timeout() {
            Kind::Timeout
        } else if h.is_user() {
            Kind::User
        } else if h.is_closed() || h.is_canceled() || find::<std::io::Error>(h).is_some() {
            Kind::Io
        } else if h.is_incomplete_message() {
            Kind::Transient
        } else {
            Kind::Other
        }
    } else if err.is_connect() || find::<std::io::Error>(&err).is_some() {
        Kind::Io
    } else {
        Kind::Other
    };
    let err: BoxError = err.into();
    match kind {
        Kind::Timeout => ConnectorError::timeout(err),
        Kind::Io => ConnectorError::io(err),
        Kind::User => ConnectorError::user(err),
        Kind::Transient => ConnectorError::other(err, Some(ErrorKind::TransientError)),
        Kind::Other => ConnectorError::other(err, None),
    }
}

/// Proxy configuration parsed from a `scheme://[user:pass@]host:port` URL,
/// where scheme is `socks5`/`socks5h` (SOCKS5) or `http`/`https` (HTTP CONNECT).
#[derive(Debug, Clone)]
pub(crate) struct ProxyConfig {
    socks5: bool,
    host: String,
    port: u16,
    auth: Option<(String, String)>,
}

impl ProxyConfig {
    fn parse(s: &str) -> anyhow::Result<ProxyConfig> {
        let u = url::Url::parse(s)
            .map_err(|e| anyhow::anyhow!("invalid proxy URL {s:?}: {e}"))?;
        let socks5 = match u.scheme().to_ascii_lowercase().as_str() {
            "socks5" | "socks5h" => true,
            "http" | "https" => false,
            other => anyhow::bail!("unsupported proxy scheme {other:?} (use socks5/socks5h/http/https)"),
        };
        let host = u
            .host_str()
            .ok_or_else(|| anyhow::anyhow!("proxy URL {s:?} has no host"))?
            .to_string();
        let port = u
            .port_or_known_default()
            .ok_or_else(|| anyhow::anyhow!("proxy URL {s:?} has no port"))?;
        let auth = if !u.username().is_empty() {
            Some((
                u.username().to_string(),
                u.password().unwrap_or("").to_string(),
            ))
        } else {
            None
        };
        Ok(ProxyConfig { socks5, host, port, auth })
    }

    fn addr(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }
}

/// Resolves the effective proxy: explicit `--proxy` first, else the standard
/// `ALL_PROXY` → `HTTPS_PROXY` → `HTTP_PROXY` environment variables (any case).
pub(crate) fn effective_proxy(opts: &Options) -> anyhow::Result<Option<ProxyConfig>> {
    fn env_any(names: &[&str]) -> Option<String> {
        names
            .iter()
            .find_map(|n| std::env::var(n).ok())
            .filter(|s| !s.is_empty())
    }
    let raw = opts
        .proxy
        .clone()
        .filter(|s| !s.is_empty())
        .or_else(|| env_any(&["ALL_PROXY", "all_proxy"]))
        .or_else(|| env_any(&["HTTPS_PROXY", "https_proxy"]))
        .or_else(|| env_any(&["HTTP_PROXY", "http_proxy"]));
    match raw {
        Some(s) => Ok(Some(ProxyConfig::parse(&s)?)),
        None => Ok(None),
    }
}

/// A `tower` connector that establishes a raw TCP byte stream to the request's
/// target host:port *through* a proxy. hyper-rustls then layers TLS on top for
/// `https://` targets; `http://` targets use the stream directly. Supports
/// SOCKS5 (remote DNS) and HTTP `CONNECT` tunneling.
#[derive(Clone)]
struct ProxyConnector {
    proxy: ProxyConfig,
}

impl tower_service::Service<http::Uri> for ProxyConnector {
    type Response = ProxyIo;
    type Error = BoxError;
    type Future =
        std::pin::Pin<Box<dyn std::future::Future<Output = Result<ProxyIo, BoxError>> + Send>>;

    fn poll_ready(
        &mut self,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), BoxError>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn call(&mut self, dst: http::Uri) -> Self::Future {
        let proxy = self.proxy.clone();
        Box::pin(async move {
            let host = dst
                .host()
                .ok_or_else(|| -> BoxError { "proxy: target URI has no host".into() })?
                .to_string();
            let port = dst
                .port_u16()
                .unwrap_or(if dst.scheme_str() == Some("https") { 443 } else { 80 });
            let tcp = if proxy.socks5 {
                connect_via_socks5(&proxy, &host, port).await?
            } else {
                connect_via_http(&proxy, &host, port).await?
            };
            Ok(ProxyIo { inner: hyper_util::rt::TokioIo::new(tcp) })
        })
    }
}

/// Connects to `host:port` through a SOCKS5 proxy (remote DNS), returning the
/// tunneled TCP stream. The proxy address is resolved locally to a concrete
/// `SocketAddr` first (tokio-socks's own proxy-name resolution returned
/// spurious NODATA errors here); the *target* is passed as a domain so the
/// proxy performs its DNS.
async fn connect_via_socks5(
    p: &ProxyConfig,
    host: &str,
    port: u16,
) -> Result<tokio::net::TcpStream, BoxError> {
    use tokio_socks::tcp::Socks5Stream;
    use tokio_socks::TargetAddr;

    let proxy_sa = tokio::net::lookup_host(p.addr())
        .await?
        .next()
        .ok_or_else(|| -> BoxError { format!("proxy {} did not resolve", p.addr()).into() })?;
    let target = TargetAddr::Domain(host.into(), port);
    let stream = match &p.auth {
        Some((u, pw)) => {
            Socks5Stream::connect_with_password(proxy_sa, target, u, pw).await?
        }
        None => Socks5Stream::connect(proxy_sa, target).await?,
    };
    Ok(stream.into_inner())
}

/// Connects to `host:port` through an HTTP proxy using a `CONNECT` tunnel,
/// returning the established TCP stream.
async fn connect_via_http(
    p: &ProxyConfig,
    host: &str,
    port: u16,
) -> Result<tokio::net::TcpStream, BoxError> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let mut tcp = tokio::net::TcpStream::connect(p.addr()).await?;
    let mut req = format!("CONNECT {host}:{port} HTTP/1.1\r\nHost: {host}:{port}\r\n");
    if let Some((u, pw)) = &p.auth {
        use base64::Engine;
        let creds = base64::engine::general_purpose::STANDARD.encode(format!("{u}:{pw}"));
        req.push_str(&format!("Proxy-Authorization: Basic {creds}\r\n"));
    }
    req.push_str("\r\n");
    tcp.write_all(req.as_bytes()).await?;

    // Read the status line + headers up to the blank line.
    let mut buf = Vec::with_capacity(256);
    let mut byte = [0u8; 1];
    loop {
        let n = tcp.read(&mut byte).await?;
        if n == 0 {
            return Err("proxy closed connection during CONNECT".into());
        }
        buf.push(byte[0]);
        if buf.ends_with(b"\r\n\r\n") {
            break;
        }
        if buf.len() > 8192 {
            return Err("proxy CONNECT response headers too large".into());
        }
    }
    let head = String::from_utf8_lossy(&buf);
    let status = head.lines().next().unwrap_or("");
    if !status.contains(" 200") {
        return Err(format!("proxy CONNECT failed: {status}").into());
    }
    Ok(tcp)
}

/// The connection type produced by `ProxyConnector`: a TCP stream wrapped for
/// hyper, carrying a `Connection` impl so hyper-util can pool it.
struct ProxyIo {
    inner: hyper_util::rt::TokioIo<tokio::net::TcpStream>,
}

impl hyper::rt::Read for ProxyIo {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: hyper::rt::ReadBufCursor<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl hyper::rt::Write for ProxyIo {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::pin::Pin::new(&mut self.inner).poll_write(cx, buf)
    }
    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_flush(cx)
    }
    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

impl hyper_util::client::legacy::connect::Connection for ProxyIo {
    fn connected(&self) -> hyper_util::client::legacy::connect::Connected {
        hyper_util::client::legacy::connect::Connected::new()
    }
}

/// A rustls verifier that accepts any certificate (for self-signed endpoints).
/// Mirrors the fast path's `NoVerify`. Only reachable when `--no-verify-ssl`.
mod danger {
    use std::sync::Arc;

    use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
    use rustls::crypto::{verify_tls12_signature, verify_tls13_signature, CryptoProvider};
    use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
    use rustls::{DigitallySignedStruct, Error, SignatureScheme};

    #[derive(Debug)]
    pub struct NoVerify(pub Arc<CryptoProvider>);

    impl ServerCertVerifier for NoVerify {
        fn verify_server_cert(
            &self,
            _end_entity: &CertificateDer<'_>,
            _intermediates: &[CertificateDer<'_>],
            _server_name: &ServerName<'_>,
            _ocsp: &[u8],
            _now: UnixTime,
        ) -> Result<ServerCertVerified, Error> {
            Ok(ServerCertVerified::assertion())
        }

        fn verify_tls12_signature(
            &self,
            message: &[u8],
            cert: &CertificateDer<'_>,
            dss: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, Error> {
            verify_tls12_signature(message, cert, dss, &self.0.signature_verification_algorithms)
        }

        fn verify_tls13_signature(
            &self,
            message: &[u8],
            cert: &CertificateDer<'_>,
            dss: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, Error> {
            verify_tls13_signature(message, cert, dss, &self.0.signature_verification_algorithms)
        }

        fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
            self.0.signature_verification_algorithms.supported_schemes()
        }
    }
}

/// Server certificates checked against the platform's trust store, as the SDK's client did, but
/// with the store read on the first TLS handshake: loading it (a 220 KB bundle and ~300 files in
/// the certificate directory, each parsed) cost ~5 ms per process, which commands against an
/// http:// endpoint never need. Every check is the one `with_root_certificates` installs
/// (rustls's `WebPkiServerVerifier`): certificates through it, handshake signatures with the
/// provider's algorithms as it does.
mod platform_trust {
    use std::sync::{Arc, OnceLock};

    use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
    use rustls::client::WebPkiServerVerifier;
    use rustls::crypto::{verify_tls12_signature, verify_tls13_signature, CryptoProvider};
    use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
    use rustls::{DigitallySignedStruct, Error, RootCertStore, SignatureScheme};

    #[derive(Debug)]
    pub struct Verifier {
        provider: Arc<CryptoProvider>,
        roots: OnceLock<Result<Arc<WebPkiServerVerifier>, String>>,
    }

    impl Verifier {
        pub fn new(provider: Arc<CryptoProvider>) -> Self {
            Verifier {
                provider,
                roots: OnceLock::new(),
            }
        }

        fn webpki(&self) -> Result<&WebPkiServerVerifier, Error> {
            let built = self.roots.get_or_init(|| {
                // Certificates that fail to parse are skipped, as the SDK's client did.
                let mut roots = RootCertStore::empty();
                roots.add_parsable_certificates(rustls_native_certs::load_native_certs().certs);
                WebPkiServerVerifier::builder_with_provider(Arc::new(roots), self.provider.clone())
                    .build()
                    .map_err(|e| format!("the platform's trust store: {e}"))
            });
            match built {
                Ok(v) => Ok(v),
                Err(e) => Err(Error::General(e.clone())),
            }
        }
    }

    impl ServerCertVerifier for Verifier {
        fn verify_server_cert(
            &self,
            end_entity: &CertificateDer<'_>,
            intermediates: &[CertificateDer<'_>],
            server_name: &ServerName<'_>,
            ocsp: &[u8],
            now: UnixTime,
        ) -> Result<ServerCertVerified, Error> {
            self.webpki()?.verify_server_cert(end_entity, intermediates, server_name, ocsp, now)
        }

        fn verify_tls12_signature(
            &self,
            message: &[u8],
            cert: &CertificateDer<'_>,
            dss: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, Error> {
            verify_tls12_signature(
                message,
                cert,
                dss,
                &self.provider.signature_verification_algorithms,
            )
        }

        fn verify_tls13_signature(
            &self,
            message: &[u8],
            cert: &CertificateDer<'_>,
            dss: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, Error> {
            verify_tls13_signature(
                message,
                cert,
                dss,
                &self.provider.signature_verification_algorithms,
            )
        }

        fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
            self.provider.signature_verification_algorithms.supported_schemes()
        }
    }
}

#[derive(Clone)]
pub struct S3 {
    pub(crate) client: Client,
    pub(crate) dry_run: bool,
    pub(crate) use_list_objects_v1: bool,
    pub(crate) request_payer_str: Option<String>,
    part_size: u64,
    concurrency: usize,
    /// Source-size threshold above which server-side copy switches from a single
    /// `CopyObject` to multipart `UploadPartCopy`. Defaults to the 5 GiB S3
    /// limit; overridable via `RS5CMD_MULTIPART_COPY_THRESHOLD` (bytes) so the
    /// multipart-copy path can be exercised in tests without a 5 GiB object.
    multipart_copy_threshold: u64,
    preserve_timestamps: bool,
    /// Aggregate upload bandwidth cap (`--limit-upload`), shared across all
    /// workers (Arc). `None` means no upload throttling (#433).
    upload_limiter: Option<std::sync::Arc<RateLimiter>>,
    /// Aggregate download bandwidth cap (`--limit-download`), shared across all
    /// workers (Arc). `None` means no download throttling (#433).
    download_limiter: Option<std::sync::Arc<RateLimiter>>,
}

impl S3 {
    /// Builds an S3 client. Honors a custom endpoint (e.g. MinIO) via
    /// `opts.endpoint` or the `AWS_ENDPOINT_URL`/`S3_ENDPOINT_URL` env vars,
    /// switching to path-style addressing in that case.
    pub async fn new(_u: &Url, opts: &Options) -> anyhow::Result<S3> {
        // One config load (region, credentials provider and its cache) per (region, profile) for
        // the whole process, however many S3 clients the command builds (F09). The loader reads
        // nothing else from `opts`. The HTTP client is shared separately below. Cached with it:
        // whether no region was configured at all (then the bucket's region is asked below).
        type Loaded = (aws_config::SdkConfig, bool);
        static SHARED: tokio::sync::Mutex<Vec<((Option<String>, Option<String>), Loaded)>> =
            tokio::sync::Mutex::const_new(Vec::new());
        let key = (opts.region.clone(), opts.profile.clone());
        let (shared, region_unset) = {
            let mut cache = SHARED.lock().await;
            match cache.iter().find(|(k, _)| *k == key) {
                Some((_, c)) => c.clone(),
                None => {
                    let configured =
                        configured_region(opts.region.as_deref(), opts.profile.as_deref()).await;
                    let unset = configured.is_none();
                    let mut loader = aws_config::defaults(BehaviorVersion::latest())
                        .region(configured.unwrap_or_else(|| Region::new("us-east-1")));
                    if let Some(profile) = &opts.profile {
                        loader = loader.profile_name(profile);
                    }
                    let c = loader.load().await;
                    cache.push((key, (c.clone(), unset)));
                    (c, unset)
                }
            }
        };

        let mut builder = aws_sdk_s3::config::Builder::from(&shared);

        // Skip the default per-request flexible checksum (CRC32) computation and
        // response validation — pure CPU/latency overhead for our transfers
        // (S3 still protects integrity via TLS + ETag). Big win for many small
        // objects.
        builder = builder
            .request_checksum_calculation(
                aws_sdk_s3::config::RequestChecksumCalculation::WhenRequired,
            )
            .response_checksum_validation(
                aws_sdk_s3::config::ResponseChecksumValidation::WhenRequired,
            );

        let endpoint = opts
            .endpoint
            .clone()
            .filter(|s| !s.is_empty())
            .or_else(|| std::env::var("AWS_ENDPOINT_URL").ok())
            .or_else(|| std::env::var("S3_ENDPOINT_URL").ok());

        let endpoint_key = endpoint.clone().unwrap_or_default();
        if let Some(ep) = endpoint {
            builder = builder.endpoint_url(ep);
            // Default to path-style for custom endpoints (MinIO and most
            // S3-compatible servers only support path-style), unless the user
            // forces an addressing style explicitly.
            let force_path = match opts.addressing_style.as_deref() {
                Some("virtual") => false,
                Some("path") => true,
                _ => true,
            };
            builder = builder.force_path_style(force_path);
        } else if let Some(style) = opts.addressing_style.as_deref() {
            // No custom endpoint (real AWS): honor an explicit override; the SDK
            // default is virtual-host style.
            builder = builder.force_path_style(style == "path");
        }
        // `--use-dualstack-endpoint` (#719): resolve the dual-stack (IPv4+IPv6)
        // S3 endpoint so traffic can use IPv6. `--use-fips-endpoint`: resolve the
        // FIPS endpoint. Both feed the SDK endpoint-resolver via the config
        // builder and are no-ops against a custom endpoint (used verbatim), so
        // they fall back harmlessly when pointed at MinIO.
        if opts.use_dualstack_endpoint {
            builder = builder.use_dual_stack(true);
        }
        if opts.use_fips_endpoint {
            builder = builder.use_fips(true);
        }

        // rs5cmd's own HTTP client for every configuration (`build_http_client`): HTTP/1.1 only,
        // and the only way to express `--no-verify-ssl`, `--proxy` and `--ca-certs-file`.
        builder = builder.http_client(cached_http_client(
            opts.no_verify_ssl,
            effective_proxy(opts)?,
            opts.ca_certs_file.as_deref(),
        )?);
        // `--retry-count` (default 10, like s5cmd) is the number of retries after the first
        // attempt. It never reached the SDK, which then gave up after 3 attempts (F02, #17).
        builder = builder.retry_config(
            aws_config::retry::RetryConfig::standard()
                .with_max_attempts(opts.max_retries.saturating_add(1)),
        );

        let mut client = Client::from_conf(builder.clone().build());
        // No region configured: like s5cmd, ask the bucket where it lives (x-amz-bucket-region)
        // and use that region, else keep us-east-1.
        if region_unset && !_u.bucket.is_empty() {
            if let Some(r) = bucket_region(&client, &endpoint_key, &_u.bucket).await {
                if r != "us-east-1" {
                    client = Client::from_conf(builder.region(Region::new(r)).build());
                }
            }
        }
        Ok(S3 {
            client,
            dry_run: opts.dry_run,
            use_list_objects_v1: opts.use_list_objects_v1,
            request_payer_str: opts.request_payer.clone(),
            part_size: opts.part_size.max(5 * 1024 * 1024),
            concurrency: opts.concurrency.max(1),
            multipart_copy_threshold: std::env::var("RS5CMD_MULTIPART_COPY_THRESHOLD")
                .ok()
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(DEFAULT_MULTIPART_COPY_THRESHOLD),
            preserve_timestamps: opts.preserve_timestamps,
            upload_limiter: opts.upload_limiter.clone(),
            download_limiter: opts.download_limiter.clone(),
        })
    }

    /// Waits until `n` more downloaded bytes fit under --limit-download, if one is set. For
    /// callers that consume a `read` body themselves (`cat`, F05).
    pub async fn pace_download(&self, n: usize) {
        if let Some(limiter) = &self.download_limiter {
            limiter.acquire(n as u64).await;
        }
    }

    pub(crate) fn request_payer(&self) -> Option<aws_sdk_s3::types::RequestPayer> {
        self.request_payer_str
            .as_deref()
            .map(aws_sdk_s3::types::RequestPayer::from)
    }

    /// Returns the object body as a streaming reader.
    pub async fn read(&self, src: &Url) -> anyhow::Result<ByteStream> {
        let mut req = self
            .client
            .get_object()
            .bucket(&src.bucket)
            .key(&src.path)
            .set_request_payer(self.request_payer());
        if !src.version_id.is_empty() {
            req = req.version_id(&src.version_id);
        }
        let resp = req.send().await?;
        Ok(resp.body)
    }

    /// Reads the byte range `[offset, offset+len)` of an object in a single
    /// ranged GET, returning the bytes. Used by the FUSE mount's chunked reader.
    /// A request that starts past EOF yields an empty vec rather than an error.
    pub async fn read_range(&self, src: &Url, offset: u64, len: u64) -> anyhow::Result<Vec<u8>> {
        if len == 0 {
            return Ok(Vec::new());
        }
        let range = format!("bytes={}-{}", offset, offset + len - 1);
        let mut req = self
            .client
            .get_object()
            .bucket(&src.bucket)
            .key(&src.path)
            .range(range)
            .set_request_payer(self.request_payer());
        if !src.version_id.is_empty() {
            req = req.version_id(&src.version_id);
        }
        let resp = match req.send().await {
            Ok(r) => r,
            Err(e) if is_range_not_satisfiable(&e) => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
        };
        let data = resp.body.collect().await?.into_bytes().to_vec();
        Ok(data)
    }

    /// Returns the object's size via HeadObject.
    async fn head_size(&self, src: &Url) -> anyhow::Result<u64> {
        let mut req = self
            .client
            .head_object()
            .bucket(&src.bucket)
            .key(&src.path)
            .set_request_payer(self.request_payer());
        if !src.version_id.is_empty() {
            req = req.version_id(&src.version_id);
        }
        let out = req.send().await?;
        Ok(out.content_length().unwrap_or(0).max(0) as u64)
    }

    /// Downloads the remote object to a local path.
    ///
    /// The first part is fetched with a ranged GET (`bytes=0-part_size-1`) whose
    /// `Content-Range` header reports the object's total size, so the
    /// single-vs-ranged decision is made from that one response — there is no
    /// separate HeadObject round-trip (saving a request per download; cf.
    /// upstream s5cmd #793). Small objects (<= part size) are therefore one
    /// request total; larger objects fetch the remaining parts as concurrent
    /// byte ranges, reusing the already-downloaded first part.
    pub async fn download(&self, src: &Url, dst: &Path) -> anyhow::Result<()> {
        if self.dry_run {
            return Ok(());
        }
        if let Some(parent) = dst.parent() {
            if !parent.as_os_str().is_empty() {
                tokio::fs::create_dir_all(parent).await?;
            }
        }

        let part_size = self.part_size;
        let mut req = self
            .client
            .get_object()
            .bucket(&src.bucket)
            .key(&src.path)
            .range(format!("bytes=0-{}", part_size - 1))
            .set_request_payer(self.request_payer());
        if !src.version_id.is_empty() {
            req = req.version_id(&src.version_id);
        }
        let resp = match req.send().await {
            Ok(r) => r,
            // A zero-byte object makes the ranged request unsatisfiable (HTTP
            // 416); fall back to a plain GET, which returns the whole (possibly
            // empty) body and creates the file correctly.
            Err(e) if is_range_not_satisfiable(&e) => {
                return self.download_single(src, dst).await;
            }
            Err(e) => return Err(e.into()),
        };

        // A 200 without Content-Range: the server ignored the Range header and sent the whole
        // object, so there is nothing more to fetch.
        let ranged = resp.content_range().is_some();
        let total = parse_content_range_total(resp.content_range())
            .or_else(|| resp.content_length().map(|l| l.max(0) as u64))
            .unwrap_or(0);

        // With --preserve-timestamps, capture the stored mtime metadata before
        // the body is consumed, to restore it after the file is written.
        let mtime_value = if self.preserve_timestamps {
            resp.metadata()
                .and_then(|m| m.get(crate::storage::MTIME_METADATA_KEY))
                .cloned()
        } else {
            None
        };

        // Written under a temporary name and renamed over `dst` once complete (F03); on any
        // error or cancellation the guard removes it and an existing `dst` stays as it was.
        let (partial, file) = PartialFile::create(dst)?;
        file.set_len(total)?;
        let file = std::sync::Arc::new(file);
        let first_len = if ranged { part_size.min(total) } else { total };
        let end = write_body(resp.body, &file, 0, self.download_limiter.as_deref()).await?;
        anyhow::ensure!(end == first_len, "{src}: got {end} of {first_len} bytes");

        if ranged && total > part_size {
            // Large object: fetch parts 1..n concurrently into the file.
            self.download_remaining_parts(src, file, total, part_size).await?;
        }

        // Restore the preserved mtime, if any (best-effort: a malformed value or
        // an unsupported filesystem must not fail the otherwise-complete copy).
        if let Some(v) = mtime_value {
            if let Err(e) = set_file_mtime_from_value(partial.path(), &v) {
                eprintln!("warning: could not restore mtime on {}: {e:#}", dst.display());
            }
        }
        partial.commit()?;
        Ok(())
    }

    /// Single streaming GET to a file. Used as the zero-byte / range-error
    /// fallback for `download`.
    async fn download_single(&self, src: &Url, dst: &Path) -> anyhow::Result<()> {
        let body = self.read(src).await?;
        let (partial, file) = PartialFile::create(dst)?;
        write_body(body, &file, 0, self.download_limiter.as_deref()).await?;
        drop(file);
        partial.commit()?;
        Ok(())
    }

    /// Fetches parts `1..n` (part 0 is already written) as concurrent ranged
    /// GETs, each streamed into the preallocated file at its offset.
    async fn download_remaining_parts(
        &self,
        src: &Url,
        file: std::sync::Arc<std::fs::File>,
        total: u64,
        part_size: u64,
    ) -> anyhow::Result<()> {
        use futures::TryStreamExt;
        let n_parts = total.div_ceil(part_size);
        // Every object's parts draw on one budget, so many large downloads at once do not open
        // up to --numworkers x --concurrency connections; one object still gets its --concurrency.
        let budget = PartBudget::global().at_least(self.concurrency);
        let tasks = (1..n_parts).map(|i| {
            let offset = i * part_size;
            let len = part_size.min(total - offset);
            let client = self.client.clone();
            let bucket = src.bucket.clone();
            let key = src.path.clone();
            let vid = src.version_id.clone();
            let rp = self.request_payer();
            let file = std::sync::Arc::clone(&file);
            let limiter = self.download_limiter.clone();
            async move {
                // Held for this part's request and body only, never while waiting for another.
                let _permit = budget.acquire().await?;
                let range = format!("bytes={}-{}", offset, offset + len - 1);
                let mut req = client
                    .get_object()
                    .bucket(&bucket)
                    .key(&key)
                    .range(range)
                    .set_request_payer(rp);
                if !vid.is_empty() {
                    req = req.version_id(vid);
                }
                let resp = req.send().await?;
                let end = write_body(resp.body, &file, offset, limiter.as_deref()).await?;
                anyhow::ensure!(
                    end == offset + len,
                    "s3://{bucket}/{key}: part at {offset} got {} of {len} bytes",
                    end - offset
                );
                anyhow::Ok(())
            }
        });
        // Stop at the first failed part; the parts still in flight are dropped (F04). With
        // collect(), a part that failed for good still let every other part download first.
        stream::iter(tasks)
            .buffer_unordered(self.concurrency)
            .try_collect::<Vec<()>>()
            .await?;
        Ok(())
    }

    /// Uploads a local file. Files larger than the configured part size use a
    /// concurrent multipart upload; smaller ones use a single PUT.
    pub async fn upload(&self, src: &Path, dst: &Url, metadata: &Metadata) -> anyhow::Result<()> {
        if self.dry_run {
            return Ok(());
        }
        let size = std::fs::metadata(src)?.len();

        // With --preserve-timestamps, carry the source file's mtime as object
        // metadata so a later download can restore it.
        let owned;
        let metadata = if self.preserve_timestamps {
            let mut m = metadata.clone();
            if let Some(v) = file_mtime_value(src) {
                m.user_defined
                    .insert(crate::storage::MTIME_METADATA_KEY.to_string(), v);
            }
            owned = m;
            &owned
        } else {
            metadata
        };

        if size <= self.part_size {
            return self.upload_single(src, dst, metadata).await;
        }
        self.upload_multipart(src, dst, metadata, size).await
    }

    /// Files up to this size are read fully into memory for the request body, avoiding the
    /// per-request streaming-from-disk overhead (blocking-pool file reads) that throttles many
    /// small concurrent uploads. Larger files stream in `upload_body`'s chunks under its
    /// process-wide budget: at 1 MiB, 256 uploads of 1 MiB files held 256 MiB of bodies at once
    /// (peak memory 8x s5cmd's).
    const INLINE_BODY_MAX: u64 = 256 * 1024;

    async fn upload_single(&self, src: &Path, dst: &Url, metadata: &Metadata) -> anyhow::Result<()> {
        let size = std::fs::metadata(src).map(|m| m.len()).unwrap_or(u64::MAX);
        // Throttle the upload to the aggregate cap, if configured (#433). Done
        // before issuing the PUT so the bytes-per-second budget is reserved.
        if let Some(limiter) = &self.upload_limiter {
            if size != u64::MAX {
                limiter.acquire(size).await;
            }
        }
        let body = if size <= Self::INLINE_BODY_MAX {
            // Allocated here and filled on the blocking pool, as upload_body's chunks are, with
            // their lead (upload_body::LEAD).
            let mut buf = super::upload_body::buffer(size as usize);
            let path = src.to_path_buf();
            let buf = tokio::task::spawn_blocking(move || -> std::io::Result<Vec<u8>> {
                use std::io::Read;
                std::fs::File::open(&path)?.read_to_end(&mut buf)?;
                Ok(buf)
            })
            .await??;
            ByteStream::from(super::upload_body::bytes(buf))
        } else {
            let len = tokio::fs::metadata(src).await?.len();
            super::upload_body::file_range(src, 0, len)
        };
        let content_type = metadata
            .content_type
            .clone()
            .unwrap_or_else(|| "application/octet-stream".to_string());

        let mut req = self
            .client
            .put_object()
            .bucket(&dst.bucket)
            .key(&dst.path)
            .body(body)
            .content_type(content_type)
            .set_request_payer(self.request_payer());

        req = apply_put_metadata(req, metadata)?;
        if metadata.if_none_match {
            // Conditional write: fail with HTTP 412 if the object already exists.
            req = req.if_none_match("*");
        }
        req.send()
            .await
            .map_err(|e| map_conditional_write_error(e.into(), dst))?;
        Ok(())
    }

    async fn upload_multipart(
        &self,
        src: &Path,
        dst: &Url,
        metadata: &Metadata,
        size: u64,
    ) -> anyhow::Result<()> {
        let content_type = metadata
            .content_type
            .clone()
            .unwrap_or_else(|| "application/octet-stream".to_string());

        let mut create = self
            .client
            .create_multipart_upload()
            .bucket(&dst.bucket)
            .key(&dst.path)
            .content_type(content_type)
            .set_request_payer(self.request_payer());
        if let Some(sc) = &metadata.storage_class {
            if !sc.is_empty() {
                create = create.storage_class(aws_sdk_s3::types::StorageClass::from(sc.as_str()));
            }
        }
        if let Some(acl) = &metadata.acl {
            if !acl.is_empty() {
                create = create.acl(aws_sdk_s3::types::ObjectCannedAcl::from(acl.as_str()));
            }
        }
        for (k, v) in &metadata.user_defined {
            create = create.metadata(k, v);
        }
        let created = create.send().await?;
        let upload_id = created
            .upload_id()
            .ok_or_else(|| anyhow::anyhow!("CreateMultipartUpload returned no upload id"))?
            .to_string();

        match self.upload_parts(src, dst, &upload_id, size).await {
            Ok(parts) => {
                let completed = CompletedMultipartUpload::builder()
                    .set_parts(Some(parts))
                    .build();
                let mut complete = self
                    .client
                    .complete_multipart_upload()
                    .bucket(&dst.bucket)
                    .key(&dst.path)
                    .upload_id(&upload_id)
                    .multipart_upload(completed)
                    .set_request_payer(self.request_payer());
                if metadata.if_none_match {
                    // Conditional write: fail with 412 if the object already exists.
                    complete = complete.if_none_match("*");
                }
                complete
                    .send()
                    .await
                    .map_err(|e| map_conditional_write_error(e.into(), dst))?;
                Ok(())
            }
            Err(e) => {
                // Best-effort abort so we don't leak an incomplete upload; say so if it fails,
                // because the parts then stay billed until a lifecycle rule removes them.
                let aborted = self
                    .client
                    .abort_multipart_upload()
                    .bucket(&dst.bucket)
                    .key(&dst.path)
                    .upload_id(&upload_id)
                    .set_request_payer(self.request_payer())
                    .send()
                    .await;
                if let Err(ae) = aborted {
                    eprintln!(
                        "warning: could not abort multipart upload {upload_id} of {dst}: {}",
                        crate::error::format_error(&anyhow::Error::from(ae))
                    );
                }
                Err(e)
            }
        }
    }

    async fn upload_parts(
        &self,
        src: &Path,
        dst: &Url,
        upload_id: &str,
        size: u64,
    ) -> anyhow::Result<Vec<CompletedPart>> {
        use futures::TryStreamExt;
        let part_size = self.part_size;
        let n_parts = size.div_ceil(part_size) as i32;
        let src = src.to_path_buf();

        let tasks = (0..n_parts).map(|i| {
            let part_number = i + 1;
            let offset = i as u64 * part_size;
            let len = part_size.min(size - offset);
            let client = self.client.clone();
            let bucket = dst.bucket.clone();
            let key = dst.path.clone();
            let upload_id = upload_id.to_string();
            let src = src.clone();
            let rp = self.request_payer();
            let limiter = self.upload_limiter.clone();
            async move {
                // Throttle this part to the aggregate upload cap (#433), before
                // streaming it from disk and issuing the UploadPart request.
                if let Some(limiter) = &limiter {
                    limiter.acquire(len).await;
                }
                let body = super::upload_body::file_range(&src, offset, len);
                let resp = client
                    .upload_part()
                    .bucket(bucket)
                    .key(key)
                    .upload_id(upload_id)
                    .part_number(part_number)
                    .body(body)
                    .set_request_payer(rp)
                    .send()
                    .await?;
                anyhow::Ok(
                    CompletedPart::builder()
                        .part_number(part_number)
                        .set_e_tag(resp.e_tag().map(|s| s.to_string()))
                        .build(),
                )
            }
        });

        let mut parts: Vec<CompletedPart> = stream::iter(tasks)
            .buffer_unordered(self.concurrency)
            // Stop at the first failed part and drop the rest (F04).
            .try_collect::<Vec<_>>()
            .await?;
        // CompleteMultipartUpload requires parts in ascending part-number order.
        parts.sort_by_key(|p| p.part_number().unwrap_or(0));
        Ok(parts)
    }

    pub async fn list_buckets(&self, prefix: &str) -> anyhow::Result<Vec<Bucket>> {
        let resp = self.client.list_buckets().send().await?;
        let mut out = Vec::new();
        for b in resp.buckets() {
            let name = b.name().unwrap_or_default().to_string();
            if prefix.is_empty() || name.starts_with(prefix) {
                out.push(Bucket {
                    creation_date: b.creation_date().and_then(|t| {
                        std::time::UNIX_EPOCH
                            .checked_add(std::time::Duration::from_secs(t.secs().max(0) as u64))
                    }),
                    name,
                });
            }
        }
        Ok(out)
    }

    pub async fn make_bucket(&self, name: &str) -> anyhow::Result<()> {
        if self.dry_run {
            return Ok(());
        }
        self.client.create_bucket().bucket(name).send().await?;
        Ok(())
    }

    pub async fn remove_bucket(&self, name: &str) -> anyhow::Result<()> {
        if self.dry_run {
            return Ok(());
        }
        self.client.delete_bucket().bucket(name).send().await?;
        Ok(())
    }

    /// Deletes many objects, batching into chunks of up to 1000 keys.
    pub fn multi_delete(&self, mut urls: mpsc::Receiver<Url>) -> mpsc::Receiver<Object> {
        let (tx, rx) = mpsc::channel::<Object>(128);
        let this = self.clone();
        tokio::spawn(async move {
            // Up to DELETE_BATCHES_IN_FLIGHT DeleteObjects requests at once (s5cmd runs 10).
            const DELETE_BATCHES_IN_FLIGHT: usize = 10;
            let mut set = tokio::task::JoinSet::new();
            let mut batch: Vec<Url> = Vec::with_capacity(DELETE_CHUNK_SIZE);
            let flush = |batch: Vec<Url>, set: &mut tokio::task::JoinSet<()>| {
                let (this, tx) = (this.clone(), tx.clone());
                set.spawn(async move { this.delete_chunk(batch, &tx).await });
            };
            while let Some(u) = urls.recv().await {
                batch.push(u);
                if batch.len() >= DELETE_CHUNK_SIZE {
                    while set.len() >= DELETE_BATCHES_IN_FLIGHT {
                        let _ = set.join_next().await;
                    }
                    flush(std::mem::take(&mut batch), &mut set);
                }
            }
            if !batch.is_empty() {
                flush(batch, &mut set);
            }
            while set.join_next().await.is_some() {}
        });
        rx
    }

    async fn delete_chunk(&self, urls: Vec<Url>, tx: &mpsc::Sender<Object>) {
        if urls.is_empty() {
            return;
        }
        let bucket = urls[0].bucket.clone();

        if self.dry_run {
            // One "(dry-run) rm" line per object that would be deleted; no DeleteObjects call.
            // (The send was not awaited, so nothing reached the printer: #5.)
            for u in urls {
                let sent = tx.send(Object {
                    url: Some(u),
                    ..Default::default()
                });
                if sent.await.is_err() {
                    return;
                }
            }
            return;
        }

        let mut objs = Vec::with_capacity(urls.len());
        for u in &urls {
            match ObjectIdentifier::builder().key(&u.path).build() {
                Ok(o) => objs.push(o),
                Err(e) => {
                    let _ = tx.send(Object::with_error(e.into())).await;
                    return;
                }
            }
        }
        let delete = match Delete::builder().set_objects(Some(objs)).build() {
            Ok(d) => d,
            Err(e) => {
                let _ = tx.send(Object::with_error(e.into())).await;
                return;
            }
        };

        let resp = self
            .client
            .delete_objects()
            .bucket(&bucket)
            .delete(delete)
            .set_request_payer(self.request_payer())
            .send()
            .await;

        match resp {
            Ok(out) => {
                for d in out.deleted() {
                    let key = format!("s3://{}/{}", bucket, d.key().unwrap_or_default());
                    if let Ok(u) = Url::parse(&key) {
                        let _ = tx
                            .send(Object {
                                url: Some(u),
                                ..Default::default()
                            })
                            .await;
                    }
                }
                for e in out.errors() {
                    let key = format!("s3://{}/{}", bucket, e.key().unwrap_or_default());
                    let u = Url::parse(&key).ok();
                    let _ = tx
                        .send(Object {
                            url: u,
                            err: Some(anyhow::anyhow!(
                                "{}",
                                e.message().unwrap_or("delete failed")
                            )),
                            ..Default::default()
                        })
                        .await;
                }
            }
            Err(e) => {
                let _ = tx.send(Object::with_error(e.into())).await;
            }
        }
    }

    fn list_v2(&self, src: &Url) -> mpsc::Receiver<Object> {
        let (tx, rx) = mpsc::channel::<Object>(128);
        let this = self.clone();
        let mut src = src.clone();
        tokio::spawn(async move {
            let mut paginator = this
                .client
                .list_objects_v2()
                .bucket(&src.bucket)
                .prefix(&src.prefix)
                // Request URL-encoded keys so keys with XML-illegal control
                // chars (e.g. ESC 0x1b) deserialize; echoed keys/prefixes are
                // percent-decoded below (upstream s5cmd #677). The opaque V2
                // continuation token is handled internally by the paginator and
                // is NOT decoded.
                .encoding_type(EncodingType::Url)
                .set_delimiter(if src.delimiter.is_empty() {
                    None
                } else {
                    Some(src.delimiter.clone())
                })
                .set_start_after(if src.start_after.is_empty() {
                    None
                } else {
                    Some(src.start_after.clone())
                })
                .set_request_payer(this.request_payer())
                .into_paginator()
                .send();

            let mut object_found = false;
            loop {
                match paginator.next().await {
                    None => break,
                    Some(Err(e)) => {
                        let _ = tx.send(Object::with_error(e.into())).await;
                        return;
                    }
                    Some(Ok(page)) => {
                        for cp in page.common_prefixes() {
                            let Some(prefix) = cp.prefix() else { continue };
                            let prefix = percent_decode(prefix);
                            let prefix = prefix.as_str();
                            if !src.matches(prefix) {
                                continue;
                            }
                            let mut newurl = src.clone();
                            newurl.path = prefix.to_string();
                            object_found = true;
                            if tx
                                .send(Object {
                                    url: Some(newurl),
                                    typ: ObjectType::Dir,
                                    ..Default::default()
                                })
                                .await
                                .is_err()
                            {
                                return;
                            }
                        }
                        for c in page.contents() {
                            let Some(key) = c.key() else { continue };
                            let key = percent_decode(key);
                            let key = key.as_str();
                            if !src.matches(key) {
                                continue;
                            }
                            let mut newurl = src.clone();
                            newurl.path = key.to_string();
                            // A key returned in Contents is a real object, even a
                            // console dir-marker ending in "/" (upstream #517). Only
                            // CommonPrefixes (handled in the loop above) are dirs.
                            let typ = ObjectType::File;
                            object_found = true;
                            let obj = Object {
                                url: Some(newurl),
                                etag: c.e_tag().unwrap_or_default().trim_matches('"').to_string(),
                                mod_time: c.last_modified().and_then(|t| {
                                    std::time::UNIX_EPOCH.checked_add(
                                        std::time::Duration::from_secs(t.secs().max(0) as u64),
                                    )
                                }),
                                typ,
                                size: c.size().unwrap_or(0),
                                storage_class: StorageClass(
                                    c.storage_class()
                                        .map(|s| s.as_str().to_string())
                                        .unwrap_or_default(),
                                ),
                                is_delete_marker: false,
                                err: None,
                            };
                            if tx.send(obj).await.is_err() {
                                return;
                            }
                        }
                    }
                }
            }

            if !object_found && !src.is_bucket() {
                let _ = tx.send(Object::with_error(NoObjectFound.into())).await;
            }
        });
        rx
    }

    /// Lists objects using the legacy ListObjects (V1) API, for providers that
    /// do not support ListObjectsV2 (e.g. GCS). Mirrors `list_v2` but paginates
    /// manually with the marker, since the V1 builder has no `into_paginator`.
    fn list_objects_v1(&self, src: &Url) -> mpsc::Receiver<Object> {
        let (tx, rx) = mpsc::channel::<Object>(128);
        let this = self.clone();
        let mut src = src.clone();
        tokio::spawn(async move {
            let mut object_found = false;
            let mut marker: Option<String> = None;
            loop {
                let mut req = this
                    .client
                    .list_objects()
                    .bucket(&src.bucket)
                    .prefix(&src.prefix)
                    // URL-encode echoed keys so XML-illegal control chars
                    // deserialize (upstream s5cmd #677). Keys, prefixes and the
                    // pagination marker are percent-decoded below.
                    .encoding_type(EncodingType::Url)
                    .set_delimiter(if src.delimiter.is_empty() {
                        None
                    } else {
                        Some(src.delimiter.clone())
                    });
                if let Some(m) = &marker {
                    req = req.marker(m);
                }
                let page = match req.send().await {
                    Ok(p) => p,
                    Err(e) => {
                        let _ = tx.send(Object::with_error(e.into())).await;
                        return;
                    }
                };

                for cp in page.common_prefixes() {
                    let Some(prefix) = cp.prefix() else { continue };
                    let prefix = percent_decode(prefix);
                    let prefix = prefix.as_str();
                    if !src.matches(prefix) {
                        continue;
                    }
                    let mut newurl = src.clone();
                    newurl.path = prefix.to_string();
                    object_found = true;
                    if tx
                        .send(Object {
                            url: Some(newurl),
                            typ: ObjectType::Dir,
                            ..Default::default()
                        })
                        .await
                        .is_err()
                    {
                        return;
                    }
                }

                let mut last_key: Option<String> = None;
                for c in page.contents() {
                    let Some(key) = c.key() else { continue };
                    let key = percent_decode(key);
                    let key = key.as_str();
                    last_key = Some(key.to_string());
                    if !src.matches(key) {
                        continue;
                    }
                    let mut newurl = src.clone();
                    newurl.path = key.to_string();
                    // Real object from Contents -> File even with a trailing "/"
                    // (upstream #517); only CommonPrefixes (above) are dirs.
                    let typ = ObjectType::File;
                    object_found = true;
                    let obj = Object {
                        url: Some(newurl),
                        etag: c.e_tag().unwrap_or_default().trim_matches('"').to_string(),
                        mod_time: c.last_modified().and_then(|t| {
                            std::time::UNIX_EPOCH
                                .checked_add(std::time::Duration::from_secs(t.secs().max(0) as u64))
                        }),
                        typ,
                        size: c.size().unwrap_or(0),
                        storage_class: StorageClass(
                            c.storage_class()
                                .map(|s| s.as_str().to_string())
                                .unwrap_or_default(),
                        ),
                        is_delete_marker: false,
                    err: None,
                    };
                    if tx.send(obj).await.is_err() {
                        return;
                    }
                }

                if !page.is_truncated().unwrap_or(false) {
                    break;
                }
                marker = page
                    .next_marker()
                    .map(percent_decode)
                    .or(last_key);
                if marker.is_none() {
                    break;
                }
            }

            if !object_found && !src.is_bucket() {
                let _ = tx.send(Object::with_error(NoObjectFound.into())).await;
            }
        });
        rx
    }

    /// Lists all versions (and delete markers) of objects under `src` via the
    /// ListObjectVersions API. Each emitted Object carries its version id.
    fn list_object_versions(&self, src: &Url) -> mpsc::Receiver<Object> {
        let (tx, rx) = mpsc::channel::<Object>(128);
        let this = self.clone();
        let mut src = src.clone();
        tokio::spawn(async move {
            let mut object_found = false;
            let mut key_marker: Option<String> = None;
            let mut version_marker: Option<String> = None;
            loop {
                let mut req = this
                    .client
                    .list_object_versions()
                    .bucket(&src.bucket)
                    .prefix(&src.prefix)
                    // URL-encode echoed keys so XML-illegal control chars
                    // deserialize (upstream s5cmd #677). Keys, prefixes and the
                    // key/version-id pagination markers are percent-decoded
                    // below.
                    .encoding_type(EncodingType::Url)
                    .set_delimiter(if src.delimiter.is_empty() {
                        None
                    } else {
                        Some(src.delimiter.clone())
                    });
                if let Some(k) = &key_marker {
                    req = req.key_marker(k);
                }
                if let Some(v) = &version_marker {
                    req = req.version_id_marker(v);
                }
                let page = match req.send().await {
                    Ok(p) => p,
                    Err(e) => {
                        let _ = tx.send(Object::with_error(e.into())).await;
                        return;
                    }
                };

                for cp in page.common_prefixes() {
                    let Some(prefix) = cp.prefix() else { continue };
                    let prefix = percent_decode(prefix);
                    let prefix = prefix.as_str();
                    if !src.matches(prefix) {
                        continue;
                    }
                    let mut nu = src.clone();
                    nu.path = prefix.to_string();
                    object_found = true;
                    if tx
                        .send(Object { url: Some(nu), typ: ObjectType::Dir, ..Default::default() })
                        .await
                        .is_err()
                    {
                        return;
                    }
                }

                for v in page.versions() {
                    let Some(key) = v.key() else { continue };
                    let key = percent_decode(key);
                    let key = key.as_str();
                    if !src.matches(key) {
                        continue;
                    }
                    let vid = v.version_id().unwrap_or_default();
                    if !src.version_id.is_empty() && src.version_id != vid {
                        continue;
                    }
                    let mut nu = src.clone();
                    nu.path = key.to_string();
                    nu.version_id = vid.to_string();
                    // Real object version from Versions -> File even with a trailing
                    // "/" (upstream #517); only CommonPrefixes (above) are dirs.
                    let typ = ObjectType::File;
                    object_found = true;
                    let obj = Object {
                        url: Some(nu),
                        etag: v.e_tag().unwrap_or_default().trim_matches('"').to_string(),
                        mod_time: v.last_modified().and_then(|t| {
                            std::time::UNIX_EPOCH
                                .checked_add(std::time::Duration::from_secs(t.secs().max(0) as u64))
                        }),
                        typ,
                        size: v.size().unwrap_or(0),
                        storage_class: StorageClass(
                            v.storage_class().map(|s| s.as_str().to_string()).unwrap_or_default(),
                        ),
                        is_delete_marker: false,
                    err: None,
                    };
                    if tx.send(obj).await.is_err() {
                        return;
                    }
                }

                for d in page.delete_markers() {
                    let Some(key) = d.key() else { continue };
                    let key = percent_decode(key);
                    let key = key.as_str();
                    if !src.matches(key) {
                        continue;
                    }
                    let vid = d.version_id().unwrap_or_default();
                    if !src.version_id.is_empty() && src.version_id != vid {
                        continue;
                    }
                    let mut nu = src.clone();
                    nu.path = key.to_string();
                    nu.version_id = vid.to_string();
                    object_found = true;
                    let obj = Object {
                        url: Some(nu),
                        mod_time: d.last_modified().and_then(|t| {
                            std::time::UNIX_EPOCH
                                .checked_add(std::time::Duration::from_secs(t.secs().max(0) as u64))
                        }),
                        typ: ObjectType::File,
                        is_delete_marker: true,
                        ..Default::default()
                    };
                    if tx.send(obj).await.is_err() {
                        return;
                    }
                }

                if !page.is_truncated().unwrap_or(false) {
                    break;
                }
                key_marker = page.next_key_marker().map(percent_decode);
                version_marker = page.next_version_id_marker().map(percent_decode);
                if key_marker.is_none() && version_marker.is_none() {
                    break;
                }
            }

            if !object_found && !src.is_bucket() {
                let _ = tx.send(Object::with_error(NoObjectFound.into())).await;
            }
        });
        rx
    }

    /// Single-shot PUT of an in-memory body to `dst`. Used by the `--links`
    /// symlink round-trip to store a small placeholder object whose body is the
    /// link target string. (#785)
    pub async fn put_object_bytes(&self, dst: &Url, body: Vec<u8>) -> anyhow::Result<()> {
        self.client
            .put_object()
            .bucket(&dst.bucket)
            .key(&dst.path)
            .body(ByteStream::from(body))
            .set_request_payer(self.request_payer())
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("put object s3://{}/{}: {e}", dst.bucket, dst.path))?;
        Ok(())
    }

    /// Fetches the full body of `src` into memory. Used by the `--links`
    /// symlink round-trip to read a placeholder object's stored target. (#785)
    pub async fn get_object_bytes(&self, src: &Url) -> anyhow::Result<Vec<u8>> {
        let mut req = self
            .client
            .get_object()
            .bucket(&src.bucket)
            .key(&src.path)
            .set_request_payer(self.request_payer());
        if !src.version_id.is_empty() {
            req = req.version_id(&src.version_id);
        }
        let resp = req
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("get object s3://{}/{}: {e}", src.bucket, src.path))?;
        let data = resp.body.collect().await.map_err(|e| {
            anyhow::anyhow!("reading object body s3://{}/{}: {e}", src.bucket, src.path)
        })?;
        Ok(data.to_vec())
    }

    /// Returns the bucket's versioning status ("Enabled"/"Suspended"/"Unset").
    pub async fn get_bucket_versioning(&self, bucket: &str) -> anyhow::Result<String> {
        let out = self.client.get_bucket_versioning().bucket(bucket).send().await?;
        Ok(out
            .status()
            .map(|s| s.as_str().to_string())
            .unwrap_or_else(|| "Unset".to_string()))
    }

    /// Sets the bucket's versioning status ("Enabled" or "Suspended").
    pub async fn set_bucket_versioning(&self, bucket: &str, status: &str) -> anyhow::Result<()> {
        if self.dry_run {
            return Ok(());
        }
        let cfg = aws_sdk_s3::types::VersioningConfiguration::builder()
            .status(aws_sdk_s3::types::BucketVersioningStatus::from(status))
            .build();
        self.client
            .put_bucket_versioning()
            .bucket(bucket)
            .versioning_configuration(cfg)
            .send()
            .await?;
        Ok(())
    }
}

#[async_trait]
impl Storage for S3 {
    async fn stat(&self, src: &Url) -> anyhow::Result<Object> {
        let mut req = self
            .client
            .head_object()
            .bucket(&src.bucket)
            .key(&src.path)
            .set_request_payer(self.request_payer());
        if !src.version_id.is_empty() {
            req = req.version_id(&src.version_id);
        }

        match req.send().await {
            Ok(out) => Ok(Object {
                url: Some(src.clone()),
                etag: out.e_tag().unwrap_or_default().trim_matches('"').to_string(),
                mod_time: out.last_modified().and_then(|t| {
                    std::time::UNIX_EPOCH
                        .checked_add(std::time::Duration::from_secs(t.secs().max(0) as u64))
                }),
                size: out.content_length().unwrap_or(0),
                typ: ObjectType::File,
                ..Default::default()
            }),
            Err(e) => {
                if e.as_service_error().map(|se| se.is_not_found()).unwrap_or(false) {
                    Err(ObjectNotFound(src.absolute()).into())
                } else {
                    Err(e.into())
                }
            }
        }
    }

    fn list(&self, src: &Url, _follow_symlinks: bool) -> mpsc::Receiver<Object> {
        if src.is_versioned() {
            return self.list_object_versions(src);
        }
        // Version listing is not yet ported; fall back to V2/V1 listing.
        if self.use_list_objects_v1 {
            return self.list_objects_v1(src);
        }
        self.list_v2(src)
    }

    async fn delete(&self, src: &Url) -> anyhow::Result<()> {
        if self.dry_run {
            return Ok(());
        }
        let mut req = self
            .client
            .delete_object()
            .bucket(&src.bucket)
            .key(&src.path)
            .set_request_payer(self.request_payer());
        if !src.version_id.is_empty() {
            req = req.version_id(&src.version_id);
        }
        req.send().await?;
        Ok(())
    }

    async fn copy(&self, src: &Url, dst: &Url, metadata: &Metadata) -> anyhow::Result<()> {
        if self.dry_run {
            return Ok(());
        }

        // When the threshold has been lowered (only happens via the test
        // override env var), decide by source size up front so the multipart
        // path can be exercised without a literal 5 GiB object. In production
        // the threshold is the 5 GiB S3 limit, so this branch is skipped and we
        // pay no extra HeadObject — we rely on the error-driven fallback below.
        if self.multipart_copy_threshold < DEFAULT_MULTIPART_COPY_THRESHOLD {
            let size = self.head_size(src).await?;
            if size > self.multipart_copy_threshold {
                return self.copy_multipart(src, dst, metadata, size).await;
            }
        }

        // Common path: a single server-side CopyObject. If S3 rejects it because
        // the source exceeds the 5 GiB single-copy limit, fall back to multipart
        // copy (s5cmd upstream PR#856).
        match self.copy_object_single(src, dst, metadata).await {
            Ok(()) => Ok(()),
            Err(e) if is_copy_source_too_large(&e) => {
                let size = self.head_size(src).await?;
                self.copy_multipart(src, dst, metadata, size).await
            }
            Err(e) => Err(e),
        }
    }
}

impl S3 {
    /// Builds the `x-amz-copy-source` value (`bucket/key[?versionId=...]`).
    fn copy_source_header(src: &Url) -> String {
        let mut s = src.escaped_path();
        if !src.version_id.is_empty() {
            s = format!("{s}?versionId={}", src.version_id);
        }
        s
    }

    /// A single server-side `CopyObject` (the original copy implementation).
    async fn copy_object_single(
        &self,
        src: &Url,
        dst: &Url,
        metadata: &Metadata,
    ) -> anyhow::Result<()> {
        if metadata.if_none_match {
            // CopyObject in aws-sdk-s3 1.x does not surface `If-None-Match`, so
            // emulate the conditional write: if the destination already exists,
            // skip it with the same typed error the PUT/multipart paths raise.
            // (A pre-existing object makes this a guaranteed skip; the small TOCTOU
            // window is acceptable for a best-effort no-clobber, matching upstream
            // intent.)
            if self.head_size(dst).await.is_ok() {
                anyhow::bail!(crate::storage::PreconditionFailedError { url: dst.to_string() });
            }
        }
        let mut req = self
            .client
            .copy_object()
            .bucket(&dst.bucket)
            .key(&dst.path)
            .copy_source(Self::copy_source_header(src))
            .set_request_payer(self.request_payer());

        if let Some(sc) = &metadata.storage_class {
            if !sc.is_empty() {
                req = req.storage_class(aws_sdk_s3::types::StorageClass::from(sc.as_str()));
            }
        }
        if let Some(acl) = &metadata.acl {
            if !acl.is_empty() {
                req = req.acl(aws_sdk_s3::types::ObjectCannedAcl::from(acl.as_str()));
            }
        }
        if let Some(cc) = &metadata.cache_control {
            if !cc.is_empty() {
                req = req.cache_control(cc);
            }
        }
        if let Some(ct) = &metadata.content_type {
            if !ct.is_empty() {
                req = req.content_type(ct);
            }
        }
        if let Some(dir) = &metadata.directive {
            if !dir.is_empty() {
                req = req
                    .metadata_directive(aws_sdk_s3::types::MetadataDirective::from(dir.as_str()));
            }
        }
        for (k, v) in &metadata.user_defined {
            req = req.metadata(k, v);
        }

        req.send().await?;
        Ok(())
    }

    /// Server-side copy of a large object via multipart `UploadPartCopy`. Used
    /// for sources over the 5 GiB single-`CopyObject` limit. The new object's
    /// metadata is set on `CreateMultipartUpload`; when no content-type override
    /// is supplied, the source's content-type is carried over (a HeadObject),
    /// since a multipart copy cannot use a COPY metadata-directive.
    async fn copy_multipart(
        &self,
        src: &Url,
        dst: &Url,
        metadata: &Metadata,
        size: u64,
    ) -> anyhow::Result<()> {
        let copy_source = Self::copy_source_header(src);

        // Resolve the content-type to stamp on the new object.
        let content_type = match &metadata.content_type {
            Some(ct) if !ct.is_empty() => Some(ct.clone()),
            _ => self.head_content_type(src).await.ok().flatten(),
        };

        let mut create = self
            .client
            .create_multipart_upload()
            .bucket(&dst.bucket)
            .key(&dst.path)
            .set_request_payer(self.request_payer());
        if let Some(ct) = content_type {
            create = create.content_type(ct);
        }
        if let Some(sc) = &metadata.storage_class {
            if !sc.is_empty() {
                create = create.storage_class(aws_sdk_s3::types::StorageClass::from(sc.as_str()));
            }
        }
        if let Some(acl) = &metadata.acl {
            if !acl.is_empty() {
                create = create.acl(aws_sdk_s3::types::ObjectCannedAcl::from(acl.as_str()));
            }
        }
        if let Some(cc) = &metadata.cache_control {
            if !cc.is_empty() {
                create = create.cache_control(cc);
            }
        }
        let created = create.send().await?;
        let upload_id = created
            .upload_id()
            .ok_or_else(|| anyhow::anyhow!("CreateMultipartUpload returned no upload id"))?
            .to_string();

        match self.copy_parts(&copy_source, dst, &upload_id, size).await {
            Ok(parts) => {
                let completed = CompletedMultipartUpload::builder()
                    .set_parts(Some(parts))
                    .build();
                self.client
                    .complete_multipart_upload()
                    .bucket(&dst.bucket)
                    .key(&dst.path)
                    .upload_id(&upload_id)
                    .multipart_upload(completed)
                    .set_request_payer(self.request_payer())
                    .send()
                    .await?;
                Ok(())
            }
            Err(e) => {
                // Best-effort abort so we don't leak an incomplete upload; say so if it fails,
                // because the parts then stay billed until a lifecycle rule removes them.
                let aborted = self
                    .client
                    .abort_multipart_upload()
                    .bucket(&dst.bucket)
                    .key(&dst.path)
                    .upload_id(&upload_id)
                    .set_request_payer(self.request_payer())
                    .send()
                    .await;
                if let Err(ae) = aborted {
                    eprintln!(
                        "warning: could not abort multipart upload {upload_id} of {dst}: {}",
                        crate::error::format_error(&anyhow::Error::from(ae))
                    );
                }
                Err(e)
            }
        }
    }

    /// Copies the source object in byte-range parts via `UploadPartCopy`,
    /// returning the completed parts in ascending part-number order.
    async fn copy_parts(
        &self,
        copy_source: &str,
        dst: &Url,
        upload_id: &str,
        size: u64,
    ) -> anyhow::Result<Vec<CompletedPart>> {
        use futures::TryStreamExt;
        // Grow the part size if needed so the part count stays within the S3
        // 10,000-part limit (a 5 TB object with the default 50 MiB part size
        // would otherwise need 100k parts).
        let mut part_size = self.part_size;
        let min_part = size.div_ceil(MAX_MULTIPART_PARTS);
        if part_size < min_part {
            part_size = min_part.div_ceil(1024 * 1024) * 1024 * 1024;
        }
        let n_parts = size.div_ceil(part_size) as i32;

        let tasks = (0..n_parts).map(|i| {
            let part_number = i + 1;
            let offset = i as u64 * part_size;
            let len = part_size.min(size - offset);
            let range = format!("bytes={}-{}", offset, offset + len - 1);
            let client = self.client.clone();
            let bucket = dst.bucket.clone();
            let key = dst.path.clone();
            let upload_id = upload_id.to_string();
            let copy_source = copy_source.to_string();
            let rp = self.request_payer();
            async move {
                let resp = client
                    .upload_part_copy()
                    .bucket(bucket)
                    .key(key)
                    .upload_id(upload_id)
                    .part_number(part_number)
                    .copy_source(copy_source)
                    .copy_source_range(range)
                    .set_request_payer(rp)
                    .send()
                    .await?;
                anyhow::Ok(
                    CompletedPart::builder()
                        .part_number(part_number)
                        .set_e_tag(resp.copy_part_result().and_then(|r| r.e_tag().map(String::from)))
                        .build(),
                )
            }
        });

        let mut parts: Vec<CompletedPart> = stream::iter(tasks)
            .buffer_unordered(self.concurrency)
            // Stop at the first failed part and drop the rest (F04).
            .try_collect::<Vec<_>>()
            .await?;
        parts.sort_by_key(|p| p.part_number().unwrap_or(0));
        Ok(parts)
    }

    /// Copies a remote object to another remote key by streaming through the
    /// client: download to a temp file, then upload. Used for `--client-copy`,
    /// when a server-side `CopyObject` is unavailable or disallowed. The
    /// source's content-type is carried over unless `metadata` overrides it.
    pub async fn client_copy(
        &self,
        src: &Url,
        dst: &Url,
        metadata: &Metadata,
    ) -> anyhow::Result<()> {
        if self.dry_run {
            return Ok(());
        }
        // Unique temp path (no Date/rand needed): pid + a process-wide counter.
        let n = CLIENT_COPY_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let tmp = std::env::temp_dir().join(format!("rs5cmd-cc-{}-{}", std::process::id(), n));

        self.download(src, &tmp).await?;

        // Preserve the source content-type unless the caller set one.
        let owned;
        let upload_meta = if metadata.content_type.as_deref().unwrap_or("").is_empty() {
            let mut m = metadata.clone();
            m.content_type = self.head_content_type(src).await.ok().flatten();
            owned = m;
            &owned
        } else {
            metadata
        };

        let r = self.upload(&tmp, dst, upload_meta).await;
        let _ = std::fs::remove_file(&tmp);
        r
    }

    /// Cross-client streaming copy: downloads `src` through *this* (source)
    /// client and uploads it to `dst` through `dst_client`. Used when the two
    /// sides of a remote→remote copy resolve to different regions/endpoints, so
    /// a single server-side `CopyObject` (which one client issues against one
    /// endpoint) cannot bridge them (#858/#816/#514/#702/#700/#671). The
    /// source's content-type is carried over unless `metadata` overrides it.
    ///
    /// Mechanically identical to [`Self::client_copy`] except the upload runs on
    /// a second client; reuses the same `download` + `upload` machinery.
    pub async fn client_copy_to(
        &self,
        dst_client: &S3,
        src: &Url,
        dst: &Url,
        metadata: &Metadata,
    ) -> anyhow::Result<()> {
        if self.dry_run || dst_client.dry_run {
            return Ok(());
        }
        let n = CLIENT_COPY_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let tmp = std::env::temp_dir().join(format!("rs5cmd-cc-{}-{}", std::process::id(), n));

        // Download from the source-side client.
        self.download(src, &tmp).await?;

        // Preserve the source content-type unless the caller set one.
        let owned;
        let upload_meta = if metadata.content_type.as_deref().unwrap_or("").is_empty() {
            let mut m = metadata.clone();
            m.content_type = self.head_content_type(src).await.ok().flatten();
            owned = m;
            &owned
        } else {
            metadata
        };

        // Upload to the destination-side client.
        let r = dst_client.upload(&tmp, dst, upload_meta).await;
        let _ = std::fs::remove_file(&tmp);
        r
    }

    /// Returns the source object's content-type via HeadObject, if any.
    async fn head_content_type(&self, src: &Url) -> anyhow::Result<Option<String>> {
        let mut req = self
            .client
            .head_object()
            .bucket(&src.bucket)
            .key(&src.path)
            .set_request_payer(self.request_payer());
        if !src.version_id.is_empty() {
            req = req.version_id(&src.version_id);
        }
        let out = req.send().await?;
        Ok(out.content_type().map(|s| s.to_string()))
    }
}

/// Formats a local file's modification time as `seconds.nanoseconds` since the
/// Unix epoch, for storage as object metadata under `MTIME_METADATA_KEY`.
fn file_mtime_value(src: &Path) -> Option<String> {
    let mtime = std::fs::metadata(src).ok()?.modified().ok()?;
    let dur = mtime.duration_since(std::time::UNIX_EPOCH).ok()?;
    Some(format!("{}.{:09}", dur.as_secs(), dur.subsec_nanos()))
}

/// Restores a file's mtime from a `seconds[.nanoseconds]` metadata value.
fn set_file_mtime_from_value(path: &Path, value: &str) -> anyhow::Result<()> {
    let (secs, nanos) = match value.split_once('.') {
        Some((s, n)) => (s.parse::<i64>()?, n.parse::<u32>().unwrap_or(0)),
        None => (value.parse::<i64>()?, 0),
    };
    let ft = filetime::FileTime::from_unix_time(secs, nanos);
    filetime::set_file_mtime(path, ft)?;
    Ok(())
}

fn apply_put_metadata(
    mut req: aws_sdk_s3::operation::put_object::builders::PutObjectFluentBuilder,
    metadata: &Metadata,
) -> anyhow::Result<aws_sdk_s3::operation::put_object::builders::PutObjectFluentBuilder> {
    if let Some(sc) = &metadata.storage_class {
        if !sc.is_empty() {
            req = req.storage_class(aws_sdk_s3::types::StorageClass::from(sc.as_str()));
        }
    }
    if let Some(acl) = &metadata.acl {
        if !acl.is_empty() {
            req = req.acl(aws_sdk_s3::types::ObjectCannedAcl::from(acl.as_str()));
        }
    }
    if let Some(cc) = &metadata.cache_control {
        if !cc.is_empty() {
            req = req.cache_control(cc);
        }
    }
    if let Some(ce) = &metadata.content_encoding {
        if !ce.is_empty() {
            req = req.content_encoding(ce);
        }
    }
    if let Some(cd) = &metadata.content_disposition {
        if !cd.is_empty() {
            req = req.content_disposition(cd);
        }
    }
    for (k, v) in &metadata.user_defined {
        req = req.metadata(k, v);
    }
    Ok(req)
}

#[cfg(test)]
mod tests {
    use super::percent_decode;

    /// A scripted connection for `FillingReads`: each step is one inner read's outcome.
    enum Step {
        Data(Vec<u8>),
        Pending,
        Fail,
    }

    struct Scripted(std::collections::VecDeque<Step>);

    impl hyper::rt::Read for Scripted {
        fn poll_read(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            mut buf: hyper::rt::ReadBufCursor<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            use std::task::Poll;
            let steps = &mut self.get_mut().0;
            match steps.pop_front() {
                Some(Step::Data(mut d)) => {
                    let n = d.len().min(buf.remaining());
                    buf.put_slice(&d[..n]);
                    if n < d.len() {
                        steps.push_front(Step::Data(d.split_off(n)));
                    }
                    Poll::Ready(Ok(()))
                }
                Some(Step::Pending) => Poll::Pending,
                Some(Step::Fail) => Poll::Ready(Err(std::io::Error::other("reset"))),
                None => Poll::Ready(Ok(())), // end of stream
            }
        }
    }

    /// One `poll_read` of `r` into a buffer of `size`: the outcome and the bytes read.
    fn read_once(
        r: &mut super::FillingReads<Scripted>,
        size: usize,
    ) -> (std::task::Poll<std::io::Result<()>>, Vec<u8>) {
        let mut mem = vec![std::mem::MaybeUninit::<u8>::uninit(); size];
        let mut buf = hyper::rt::ReadBuf::uninit(&mut mem);
        let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
        let poll = hyper::rt::Read::poll_read(std::pin::Pin::new(r), &mut cx, buf.unfilled());
        (poll, buf.filled().to_vec())
    }

    fn filling(steps: Vec<Step>, fill: bool) -> super::FillingReads<Scripted> {
        super::FillingReads {
            io: Scripted(steps.into()),
            fill,
            deferred: None,
            _open: super::OpenConnection::new(),
        }
    }

    #[test]
    fn filling_reads_gathers_what_is_ready() {
        use std::task::Poll;
        let mut r = filling(
            vec![
                Step::Data(vec![1; 10]),
                Step::Data(vec![2; 20]),
                Step::Pending,
                Step::Data(vec![3; 5]),
                Step::Fail,
            ],
            true,
        );
        // Two reads' worth, then nothing more ready: both are returned at once.
        let (poll, got) = read_once(&mut r, 1 << 20);
        assert!(matches!(poll, Poll::Ready(Ok(()))));
        assert_eq!(got, [vec![1; 10], vec![2; 20]].concat());
        // Bytes, then an error: the bytes now, the error on the next read.
        let (poll, got) = read_once(&mut r, 1 << 20);
        assert!(matches!(poll, Poll::Ready(Ok(()))));
        assert_eq!(got, vec![3; 5]);
        let (poll, got) = read_once(&mut r, 1 << 20);
        assert!(matches!(poll, Poll::Ready(Err(_))));
        assert!(got.is_empty());
        // End of stream reads as zero bytes.
        let (poll, got) = read_once(&mut r, 1 << 20);
        assert!(matches!(poll, Poll::Ready(Ok(()))));
        assert!(got.is_empty());
    }

    #[test]
    fn filling_reads_stop_at_their_limits_and_read_plain_connections_once() {
        use std::task::Poll;
        let limit = super::FILL_LIMIT;
        let data: Vec<u8> = (0..3 * limit).map(|i| (i % 251) as u8).collect();
        let chunks = data.chunks(16 << 10).map(|c| Step::Data(c.to_vec())).collect();
        let mut r = filling(chunks, true);
        let mut got = Vec::new();
        for _ in 0..3 {
            let (poll, part) = read_once(&mut r, 1 << 20);
            assert!(matches!(poll, Poll::Ready(Ok(()))));
            assert_eq!(part.len(), limit);
            got.extend(part);
        }
        assert_eq!(got, data);
        // Nothing ready on the first read: pending, as the inner reader is.
        let mut r = filling(vec![Step::Pending], true);
        assert!(read_once(&mut r, 1 << 20).0.is_pending());
        // A plain connection reads once per call...
        let mut r = filling(vec![Step::Data(vec![7; 10]), Step::Data(vec![8; 10])], false);
        assert_eq!(read_once(&mut r, 1 << 20).1, vec![7; 10]);
        // ...and no more than its limit, however much is ready and however large hyper's
        // buffer (other tests' connections count too, so the limit is between its bounds).
        let big: Vec<u8> = (0..3 * super::PLAIN_READ_MAX).map(|i| (i % 251) as u8).collect();
        let mut r = filling(vec![Step::Data(big.clone())], false);
        let (poll, part) = read_once(&mut r, 1 << 20);
        assert!(matches!(poll, Poll::Ready(Ok(()))));
        assert!((limit..=super::PLAIN_READ_MAX).contains(&part.len()), "{}", part.len());
        assert_eq!(part, big[..part.len()]);
        // Pending and errors pass through.
        let mut r = filling(vec![Step::Pending, Step::Fail], false);
        assert!(read_once(&mut r, 1 << 20).0.is_pending());
        assert!(matches!(read_once(&mut r, 1 << 20).0, Poll::Ready(Err(_))));
    }

    #[test]
    fn plain_reads_share_their_budget_over_the_connections_open() {
        use super::{plain_read_limit, FILL_LIMIT, PLAIN_READ_MAX};
        assert_eq!(plain_read_limit(0), PLAIN_READ_MAX);
        assert_eq!(plain_read_limit(4), PLAIN_READ_MAX);
        assert_eq!(plain_read_limit(8), 256 << 10);
        assert_eq!(plain_read_limit(16), 128 << 10);
        assert_eq!(plain_read_limit(32), FILL_LIMIT);
        assert_eq!(plain_read_limit(2048), FILL_LIMIT);
    }

    #[tokio::test]
    async fn part_budget_bounds_parts_and_grows_to_the_concurrency_asked() {
        let b = super::PartBudget::new(2);
        let sem = b.at_least(1);
        assert_eq!(sem.available_permits(), 2);
        let p1 = sem.acquire().await.unwrap();
        let p2 = sem.acquire().await.unwrap();
        assert!(sem.try_acquire().is_err(), "a third part started beyond the budget");
        // A larger --concurrency raises the budget; a smaller one never lowers it.
        assert_eq!(b.at_least(3).available_permits(), 1);
        assert_eq!(b.at_least(1).available_permits(), 1);
        drop((p1, p2));
        assert_eq!(b.at_least(1).available_permits(), 3);
    }

    #[test]
    fn percent_decode_passthrough_plain() {
        assert_eq!(percent_decode("hello/world.txt"), "hello/world.txt");
    }

    #[test]
    fn percent_decode_control_char() {
        // S3 with EncodingType=Url URL-encodes an ESC (0x1b) as "%1B"; we must
        // decode it back to the raw control byte (upstream s5cmd #677).
        assert_eq!(percent_decode("dir/key%1Bend"), "dir/key\u{1b}end");
        // Lowercase hex is equally valid.
        assert_eq!(percent_decode("dir/key%1bend"), "dir/key\u{1b}end");
    }

    #[test]
    fn percent_decode_common_encodings() {
        // Space, plus and percent are the cases that actually differ under
        // URL-encoding of keys.
        assert_eq!(percent_decode("a%20b"), "a b");
        assert_eq!(percent_decode("a%2Bb"), "a+b");
        assert_eq!(percent_decode("a%25b"), "a%b");
        // The encoded forward slash must round-trip to a real path separator.
        assert_eq!(percent_decode("a%2Fb"), "a/b");
    }

    #[test]
    fn percent_decode_malformed_is_verbatim() {
        // A lone or malformed '%' (no two following hex digits) is left as-is
        // rather than panicking or dropping bytes.
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("50%of"), "50%of");
        assert_eq!(percent_decode("trailing%2"), "trailing%2");
        assert_eq!(percent_decode("%zz"), "%zz");
    }

    #[test]
    fn percent_decode_multibyte_utf8() {
        // A multibyte UTF-8 char (é = 0xC3 0xA9) encoded byte-by-byte must
        // reassemble correctly.
        assert_eq!(percent_decode("caf%C3%A9"), "café");
    }

    // ---- Conditional write (`--if-none-match`) error mapping (#752) ----

    #[test]
    fn precondition_failed_is_detected_case_insensitively() {
        // The 412 code as the SDK/MinIO surface it, in any case, is detected; an
        // unrelated error is not misclassified.
        assert!(super::is_precondition_failed(&"PreconditionFailed"));
        assert!(super::is_precondition_failed(&"at least one of the pre-conditions you specified did not hold (PreconditionFailed)"));
        assert!(super::is_precondition_failed(&"Precondition Failed"));
        assert!(!super::is_precondition_failed(&"NoSuchKey"));
        assert!(!super::is_precondition_failed(&"AccessDenied"));
        assert!(!super::is_precondition_failed(&""));
    }

    #[test]
    fn map_conditional_write_error_converts_412_to_typed_skip() {
        let dst = super::Url::parse("s3://bucket/key").unwrap();
        // A 412 maps to the typed PreconditionFailedError so cp can skip it.
        let mapped = super::map_conditional_write_error(
            anyhow::anyhow!("server said PreconditionFailed"),
            &dst,
        );
        let pf = mapped.downcast_ref::<crate::storage::PreconditionFailedError>();
        assert!(pf.is_some(), "412 should map to PreconditionFailedError");
        let msg = mapped.to_string();
        assert!(msg.contains("already exists"), "got: {msg}");
        assert!(msg.contains("s3://bucket/key"), "got: {msg}");
    }

    #[test]
    fn map_conditional_write_error_passes_through_unrelated_errors() {
        let dst = super::Url::parse("s3://bucket/key").unwrap();
        let mapped =
            super::map_conditional_write_error(anyhow::anyhow!("AccessDenied"), &dst);
        // Unrelated errors are returned unchanged (NOT a precondition skip).
        assert!(mapped
            .downcast_ref::<crate::storage::PreconditionFailedError>()
            .is_none());
        assert!(mapped.to_string().contains("AccessDenied"));
    }
}
