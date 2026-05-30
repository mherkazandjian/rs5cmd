//! Per-core io_uring S3 client built on monoio + monoio-transports. Supports
//! plain HTTP/1.1 and HTTPS (rustls), with connection pooling; signs each
//! request with SigV4. Designed to live on a single thread (monoio is
//! thread-per-core, futures are !Send).

use std::net::{SocketAddr, ToSocketAddrs};
use std::rc::Rc;
use std::sync::Arc;

use bytes::Bytes;
use http::{Request, StatusCode, Uri};
use monoio::net::TcpStream;
use monoio_http::common::body::{BodyExt, FixedBody, HttpBody};
use monoio_rustls::ClientTlsStream;
use monoio_transports::connectors::{Connector, TcpConnector, TcpTlsAddr, TlsConnector};
use monoio_transports::http::HttpConnector;

use super::sign::Signer;

/// Connection-pool key for plain HTTP: one pooled set per host:port.
#[derive(Debug, Clone, Eq, PartialEq, Hash)]
pub struct HostKey {
    pub host: String,
    pub port: u16,
}

impl ToSocketAddrs for HostKey {
    type Iter = std::vec::IntoIter<SocketAddr>;
    fn to_socket_addrs(&self) -> std::io::Result<Self::Iter> {
        (self.host.as_str(), self.port).to_socket_addrs()
    }
}

/// Endpoint configuration (path-style S3, e.g. MinIO or AWS).
#[derive(Clone)]
pub struct Endpoint {
    pub scheme: String, // "http" or "https"
    pub host: String,
    pub port: u16,
    /// Skip TLS certificate verification (for self-signed endpoints).
    pub no_verify: bool,
}

impl Endpoint {
    fn is_tls(&self) -> bool {
        self.scheme == "https"
    }
    fn host_header(&self) -> String {
        let default = if self.is_tls() { 443 } else { 80 };
        if self.port == default {
            self.host.clone()
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }
    /// Absolute path-style URL: scheme://host:port/bucket/key
    fn url(&self, bucket: &str, key: &str) -> String {
        format!("{}://{}/{}/{}", self.scheme, self.host_header(), bucket, key)
    }
}

type HttpPool = HttpConnector<TcpConnector, HostKey, TcpStream>;
type HttpsPool =
    HttpConnector<TlsConnector<TcpConnector>, TcpTlsAddr, ClientTlsStream<TcpStream>>;

/// The transport: plain HTTP or TLS, each with its own pooled connector + key.
#[derive(Clone)]
enum Transport {
    Http(Rc<HttpPool>, HostKey),
    Https(Rc<HttpsPool>, TcpTlsAddr),
}

/// A single-thread S3 client over io_uring. Cheap to clone (shares the pool).
#[derive(Clone)]
pub struct FastClient {
    transport: Transport,
    endpoint: Endpoint,
    signer: Signer,
}

impl FastClient {
    pub fn new(endpoint: Endpoint, signer: Signer) -> anyhow::Result<FastClient> {
        let transport = if endpoint.is_tls() {
            // Ensure a process-wide rustls crypto provider exists.
            let _ = rustls::crypto::ring::default_provider().install_default();

            let connector: HttpsPool = if endpoint.no_verify {
                let provider = Arc::new(rustls::crypto::ring::default_provider());
                let mut cfg = rustls::ClientConfig::builder_with_provider(provider.clone())
                    .with_safe_default_protocol_versions()?
                    .dangerous()
                    .with_custom_certificate_verifier(Arc::new(danger::NoVerify(provider)))
                    .with_no_client_auth();
                cfg.alpn_protocols = vec![b"http/1.1".to_vec()];
                let tls = TlsConnector::new(TcpConnector::default(), cfg.into());
                let mut hc = HttpConnector::new(tls);
                hc.set_http1_only();
                hc
            } else {
                // Default: webpki roots (works against real AWS).
                HttpConnector::build_tls_http1_only()
            };

            let uri: Uri = format!("https://{}", endpoint.host_header()).parse()?;
            let addr = TcpTlsAddr::try_from(&uri)
                .map_err(|e| anyhow::anyhow!("invalid TLS endpoint {uri}: {e:?}"))?;
            Transport::Https(Rc::new(connector), addr)
        } else {
            let key = HostKey {
                host: endpoint.host.clone(),
                port: endpoint.port,
            };
            Transport::Http(Rc::new(HttpConnector::default()), key)
        };

        Ok(FastClient {
            transport,
            endpoint,
            signer,
        })
    }

    /// Builds a signed monoio request. Takes ownership of `body` so the payload
    /// moves into the request without a copy.
    fn build_request(
        &self,
        method: http::Method,
        bucket: &str,
        key: &str,
        body: Bytes,
    ) -> anyhow::Result<Request<HttpBody>> {
        self.build_request_with_headers(method, bucket, key, body, &[])
    }

    /// Like `build_request`, but adds the given extra headers BEFORE signing so
    /// they are covered by the SigV4 signed-headers set (required for headers
    /// like `x-amz-copy-source` that S3 authenticates).
    fn build_request_with_headers(
        &self,
        method: http::Method,
        bucket: &str,
        key: &str,
        body: Bytes,
        extra_headers: &[(&str, String)],
    ) -> anyhow::Result<Request<HttpBody>> {
        let url = self.endpoint.url(bucket, key);
        let uri: Uri = url.parse()?;

        let mut sign_builder = http::Request::builder()
            .method(method.clone())
            .uri(uri.clone())
            .header("host", self.endpoint.host_header());
        for (k, v) in extra_headers {
            sign_builder = sign_builder.header(*k, v);
        }
        let mut to_sign = sign_builder.body(())?;
        self.signer.sign_s3(&mut to_sign, &body)?;

        let path_and_query = uri
            .path_and_query()
            .map(|p| p.as_str())
            .unwrap_or("/")
            .to_string();
        let mut builder = http::Request::builder().method(method).uri(path_and_query);
        for (k, v) in to_sign.headers() {
            builder = builder.header(k, v);
        }
        let body = if body.is_empty() {
            HttpBody::fixed_body(None)
        } else {
            HttpBody::fixed_body(Some(body))
        };
        Ok(builder.body(body)?)
    }

    /// Sends a pre-built request over the configured transport, returning the
    /// status and full response body.
    async fn send(&self, req: Request<HttpBody>) -> anyhow::Result<(StatusCode, Bytes)> {
        match &self.transport {
            Transport::Http(connector, key) => {
                let mut conn = connector
                    .connect(key.clone())
                    .await
                    .map_err(|e| anyhow::anyhow!("connect: {e:?}"))?;
                let (res, _reuse) = conn.send_request(req).await;
                let resp = res.map_err(|e| anyhow::anyhow!("send: {e:?}"))?;
                let status = resp.status();
                let (_p, rbody) = resp.into_parts();
                let bytes = rbody
                    .bytes()
                    .await
                    .map_err(|e| anyhow::anyhow!("read body: {e:?}"))?;
                Ok((status, bytes))
            }
            Transport::Https(connector, addr) => {
                let mut conn = connector
                    .connect(addr.clone())
                    .await
                    .map_err(|e| anyhow::anyhow!("tls connect: {e:?}"))?;
                let (res, _reuse) = conn.send_request(req).await;
                let resp = res.map_err(|e| anyhow::anyhow!("send: {e:?}"))?;
                let status = resp.status();
                let (_p, rbody) = resp.into_parts();
                let bytes = rbody
                    .bytes()
                    .await
                    .map_err(|e| anyhow::anyhow!("read body: {e:?}"))?;
                Ok((status, bytes))
            }
        }
    }

    /// PUT an object from owned bytes.
    pub async fn put(&self, bucket: &str, key: &str, body: Bytes) -> anyhow::Result<()> {
        let req = self.build_request(http::Method::PUT, bucket, key, body)?;
        let (status, body) = self.send(req).await?;
        if !status.is_success() {
            return Err(HttpStatusError {
                status,
                message: format!(
                    "PUT {bucket}/{key} -> {status}: {}",
                    String::from_utf8_lossy(&body)
                ),
            }
            .into());
        }
        Ok(())
    }

    /// GET an object, returning its bytes.
    pub async fn get(&self, bucket: &str, key: &str) -> anyhow::Result<Bytes> {
        let req = self.build_request(http::Method::GET, bucket, key, Bytes::new())?;
        let (status, body) = self.send(req).await?;
        if !status.is_success() {
            return Err(HttpStatusError {
                status,
                message: format!(
                    "GET {bucket}/{key} -> {status}: {}",
                    String::from_utf8_lossy(&body)
                ),
            }
            .into());
        }
        Ok(body)
    }

    /// DELETE an object. A 404 is treated as success (already absent), matching
    /// S3 DeleteObject semantics.
    pub async fn delete(&self, bucket: &str, key: &str) -> anyhow::Result<()> {
        let req = self.build_request(http::Method::DELETE, bucket, key, Bytes::new())?;
        let (status, body) = self.send(req).await?;
        if !status.is_success() && status != StatusCode::NOT_FOUND {
            return Err(HttpStatusError {
                status,
                message: format!(
                    "DELETE {bucket}/{key} -> {status}: {}",
                    String::from_utf8_lossy(&body)
                ),
            }
            .into());
        }
        Ok(())
    }

    /// Server-side copy `src_bucket/src_key` -> `dst_bucket/dst_key` via S3
    /// CopyObject: a PUT to the destination with an `x-amz-copy-source` header
    /// of `/<src_bucket>/<src_key>` (the key path segments URL-encoded). The
    /// header is signed (covered by SigV4), and the request body is empty.
    pub async fn copy(
        &self,
        src_bucket: &str,
        src_key: &str,
        dst_bucket: &str,
        dst_key: &str,
    ) -> anyhow::Result<()> {
        let copy_source = format!("/{}/{}", src_bucket, encode_key_path(src_key));
        let req = self.build_request_with_headers(
            http::Method::PUT,
            dst_bucket,
            dst_key,
            Bytes::new(),
            &[("x-amz-copy-source", copy_source)],
        )?;
        let (status, body) = self.send(req).await?;
        if !status.is_success() {
            return Err(HttpStatusError {
                status,
                message: format!(
                    "COPY {src_bucket}/{src_key} -> {dst_bucket}/{dst_key} -> {status}: {}",
                    String::from_utf8_lossy(&body)
                ),
            }
            .into());
        }
        // CopyObject can return 200 OK with an <Error> in the body (a late
        // failure after the response headers were already sent). Treat that as
        // a non-retryable failure.
        if memmem(&body, b"<Error") {
            return Err(HttpStatusError {
                status: StatusCode::INTERNAL_SERVER_ERROR,
                message: format!(
                    "COPY {src_bucket}/{src_key} -> {dst_bucket}/{dst_key}: 200 OK with error body: {}",
                    String::from_utf8_lossy(&body)
                ),
            }
            .into());
        }
        Ok(())
    }

    /// Creates a bucket via a signed `PUT /<bucket>`. Returns the HTTP status so
    /// callers can treat an already-exists response as success.
    pub async fn create_bucket(&self, bucket: &str) -> anyhow::Result<StatusCode> {
        // PUT to the bucket root (empty key) — endpoint.url yields
        // `scheme://host/bucket/`, which path-style S3/MinIO accept for create.
        let req = self.build_request(http::Method::PUT, bucket, "", Bytes::new())?;
        let (status, _body) = self.send(req).await?;
        Ok(status)
    }
}

/// URL-encodes each segment of an S3 key for use in the `x-amz-copy-source`
/// header, preserving `/` separators. Unreserved characters (RFC 3986) and the
/// path-safe set are left as-is; everything else is percent-encoded.
fn encode_key_path(key: &str) -> String {
    let mut out = String::with_capacity(key.len());
    for &b in key.as_bytes() {
        match b {
            b'/' => out.push('/'),
            b'A'..=b'Z'
            | b'a'..=b'z'
            | b'0'..=b'9'
            | b'-'
            | b'_'
            | b'.'
            | b'~' => out.push(b as char),
            _ => {
                out.push('%');
                out.push(char::from_digit((b >> 4) as u32, 16).unwrap().to_ascii_uppercase());
                out.push(char::from_digit((b & 0xf) as u32, 16).unwrap().to_ascii_uppercase());
            }
        }
    }
    out
}

/// Returns whether `haystack` contains the byte subsequence `needle`.
fn memmem(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() || needle.len() > haystack.len() {
        return needle.is_empty();
    }
    haystack.windows(needle.len()).any(|w| w == needle)
}

/// An HTTP response with a non-success status. Carries the status so the retry
/// loop can tell transient failures (5xx, 408, 429) from permanent ones (other
/// 4xx like 403/404), which must not consume the retry budget.
#[derive(Debug)]
pub struct HttpStatusError {
    pub status: StatusCode,
    pub message: String,
}

impl HttpStatusError {
    /// Whether this status is worth retrying.
    pub fn is_retryable(&self) -> bool {
        self.status.is_server_error()
            || self.status == StatusCode::REQUEST_TIMEOUT
            || self.status == StatusCode::TOO_MANY_REQUESTS
    }
}

impl std::fmt::Display for HttpStatusError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for HttpStatusError {}

/// A rustls verifier that accepts any certificate (for self-signed endpoints).
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_key_path_preserves_slashes_and_encodes_specials() {
        assert_eq!(encode_key_path("a/b/c.txt"), "a/b/c.txt");
        assert_eq!(encode_key_path("dir/name with space"), "dir/name%20with%20space");
        assert_eq!(encode_key_path("plus+and%percent"), "plus%2Band%25percent");
        assert_eq!(encode_key_path("k/é"), "k/%C3%A9");
    }

    #[test]
    fn memmem_detects_error_marker() {
        assert!(memmem(b"<?xml ?><Error><Code>x</Code></Error>", b"<Error"));
        assert!(!memmem(b"<CopyObjectResult></CopyObjectResult>", b"<Error"));
        assert!(memmem(b"anything", b""));
    }

    fn endpoint_from_env(var: &str, no_verify: bool) -> Option<Endpoint> {
        let raw = std::env::var(var).ok()?;
        let uri: Uri = raw.parse().ok()?;
        let scheme = uri.scheme_str().unwrap_or("http").to_string();
        let port = uri
            .port_u16()
            .unwrap_or(if scheme == "https" { 443 } else { 80 });
        Some(Endpoint {
            host: uri.host()?.to_string(),
            port,
            scheme,
            no_verify,
        })
    }

    fn test_signer() -> Signer {
        Signer::new(
            std::env::var("AWS_ACCESS_KEY_ID").unwrap_or_else(|_| "minioadmin".into()),
            std::env::var("AWS_SECRET_ACCESS_KEY").unwrap_or_else(|_| "minioadmin".into()),
            None,
            std::env::var("AWS_REGION").unwrap_or_else(|_| "us-east-1".into()),
        )
    }

    async fn roundtrip(endpoint: Endpoint) {
        let client = FastClient::new(endpoint, test_signer()).expect("client");
        let bucket = "rs5cmd-fasttest";
        let key = "fastpath/roundtrip.txt";
        let payload = b"fast path payload";
        // The test-fast service only sets the endpoint; provision the bucket
        // ourselves (idempotent — an already-exists status is fine) so the test
        // is hermetic.
        let _ = client.create_bucket(bucket).await;
        client
            .put(bucket, key, Bytes::from_static(payload))
            .await
            .expect("put");
        let got = client.get(bucket, key).await.expect("get");
        assert_eq!(got.as_ref(), payload);
    }

    // HTTP round-trip (uses AWS_ENDPOINT_URL, e.g. http://minio:9000).
    #[monoio::test(enable_timer = true)]
    async fn put_get_roundtrip() {
        let Some(endpoint) = endpoint_from_env("AWS_ENDPOINT_URL", false) else {
            eprintln!("skipping: no AWS_ENDPOINT_URL");
            return;
        };
        roundtrip(endpoint).await;
    }

    // HTTPS round-trip (set RS5CMD_TLS_ENDPOINT to an https S3 endpoint;
    // certificate verification is skipped for self-signed test servers).
    #[monoio::test(enable_timer = true)]
    async fn put_get_roundtrip_tls() {
        let Some(endpoint) = endpoint_from_env("RS5CMD_TLS_ENDPOINT", true) else {
            eprintln!("skipping: no RS5CMD_TLS_ENDPOINT");
            return;
        };
        roundtrip(endpoint).await;
    }
}
