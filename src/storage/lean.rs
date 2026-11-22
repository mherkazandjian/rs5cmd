//! The lean data path (D4): rs5cmd sends S3 object requests itself instead of through the SDK's
//! orchestrator, which costs about 148 µs of CPU per small request against ~30 µs for a request
//! sent this way (E8). Opt-in with `--data-path lean` (or `RS5CMD_DATA_PATH=lean`); the default
//! stays the SDK.
//!
//! It keeps from the SDK what decides behaviour, so the two paths do not drift:
//! - the connection: the pooled hyper client the SDK uses (`build_http_client`: HTTP/1.1, the TLS
//!   settings, `--proxy`, the connect timeout and its error classes), so both paths share it;
//! - credentials: the SDK config's provider chain, cached as the SDK's identity cache caches them;
//! - the endpoint: the SDK's endpoint resolver, given the client's parameters (region, custom
//!   endpoint, path style, dual-stack, FIPS). A bucket whose auth scheme is not SigV4 (S3
//!   Express, multi-region access points) stays on the SDK;
//! - retries: the SDK's standard mode (attempts, full-jitter backoff, the retryable classes and
//!   the retry token bucket);
//! - errors: the SDK's own error values (`SdkError` around the operation's error), so messages
//!   and the checks made on them are the same on both paths;
//! - stalled downloads: the SDK's minimum-throughput body with the client's stalled-stream
//!   settings, so a download that stops sending fails after the same grace period. (The SDK's
//!   content-length check needs no copy: hyper already ends a short body with an error.)
//!
//! Requests are signed with SigV4 through `aws-sigv4` (as the SDK, with S3's settings: single
//! URI encoding, no path normalisation, `x-amz-content-sha256`).
//!
//! Phase 1: GetObject (whole and ranged) and HeadObject.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant, SystemTime};

use aws_credential_types::provider::error::CredentialsError;
use aws_credential_types::provider::{ProvideCredentials, SharedCredentialsProvider};
use aws_credential_types::Credentials;
use aws_sdk_s3::config::endpoint::{DefaultResolver, Params, ResolveEndpoint};
use aws_sdk_s3::operation::get_object::GetObjectError;
use aws_sdk_s3::operation::head_object::HeadObjectError;
use aws_smithy_async::rt::sleep::TokioSleep;
use aws_smithy_async::time::SystemTimeSource;
use aws_smithy_runtime::client::http::body::minimum_throughput::MinimumThroughputDownloadBody;
use aws_smithy_runtime_api::client::http::{HttpConnector, SharedHttpConnector};
use aws_smithy_runtime_api::client::orchestrator::{HttpRequest, HttpResponse};
use aws_smithy_runtime_api::client::result::{ConnectorError, SdkError};
use aws_smithy_runtime_api::client::stalled_stream_protection::StalledStreamProtectionConfig;
use aws_smithy_types::body::SdkBody;
use aws_smithy_types::byte_stream::ByteStream;
use aws_smithy_types::error::ErrorMetadata;
use aws_smithy_types::retry::ErrorKind;
use aws_smithy_types::Document;

use super::url::Url;

const USER_AGENT: &str = concat!("rs5cmd/", env!("CARGO_PKG_VERSION"));

// --- Retries: the SDK's standard mode (aws-smithy-runtime `StandardRetryStrategy`) ---

/// The SDK's retry quota, one token bucket for every S3 client in the process (its retry
/// partition is the service): 500 tokens. A retry takes 10 after a transient error (a 5xx status,
/// a timeout or a broken connection) and 5 after any other; when the request then succeeds the
/// last retry's tokens come back, the earlier ones are spent, and a request that succeeds at once
/// adds one. With the bucket empty a failed request is not retried.
static RETRY_QUOTA: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(RETRY_QUOTA_MAX);
const RETRY_QUOTA_MAX: usize = 500;
const RETRY_COST: u32 = 5;
const RETRY_TIMEOUT_COST: u32 = 10;
/// Full jitter: a random fraction of 1 s × 2^(attempt - 1), at most 20 s.
const INITIAL_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(20);
/// aws-runtime's `THROTTLING_ERRORS` and `TRANSIENT_ERRORS`.
const THROTTLING_CODES: &[&str] = &[
    "Throttling",
    "ThrottlingException",
    "ThrottledException",
    "RequestThrottledException",
    "TooManyRequestsException",
    "ProvisionedThroughputExceededException",
    "TransactionInProgressException",
    "RequestLimitExceeded",
    "BandwidthLimitExceeded",
    "LimitExceededException",
    "RequestThrottled",
    "SlowDown",
    "PriorRequestNotComplete",
    "EC2ThrottledException",
];
const TRANSIENT_CODES: &[&str] = &["RequestTimeout", "RequestTimeoutException"];
/// aws-smithy-runtime's `TRANSIENT_ERROR_STATUS_CODES`.
const TRANSIENT_STATUSES: &[u16] = &[500, 502, 503, 504];

/// Whether an error response is retried, and as what: the AWS error code first (throttling, then
/// transient codes), then the status.
fn classify_status(status: u16, code: Option<&str>) -> Option<ErrorKind> {
    match code {
        Some(c) if THROTTLING_CODES.contains(&c) => Some(ErrorKind::ThrottlingError),
        Some(c) if TRANSIENT_CODES.contains(&c) => Some(ErrorKind::TransientError),
        _ if TRANSIENT_STATUSES.contains(&status) => Some(ErrorKind::TransientError),
        _ => None,
    }
}

/// Whether a connection-level failure is retried: timeouts and I/O errors are transient, others
/// as the connector classified them (`connector_error` in s3.rs).
fn classify_connector(e: &ConnectorError) -> Option<ErrorKind> {
    if e.is_timeout() || e.is_io() {
        Some(ErrorKind::TransientError)
    } else {
        e.as_other()
    }
}

fn backoff(attempt: u32) -> Duration {
    let factor = 2u32.checked_pow(attempt.saturating_sub(1)).unwrap_or(u32::MAX);
    INITIAL_BACKOFF
        .mul_f64(fastrand::f64() * f64::from(factor))
        .min(MAX_BACKOFF)
}

/// A request that succeeded gives back its last retry's tokens (the permit is dropped) or, on the
/// first attempt, adds one token, up to the bucket's size.
fn reward_success(permit: Option<tokio::sync::SemaphorePermit<'static>>) {
    match permit {
        Some(p) => drop(p),
        None => {
            if RETRY_QUOTA.available_permits() < RETRY_QUOTA_MAX {
                RETRY_QUOTA.add_permits(1);
            }
        }
    }
}

// --- Credentials: the SDK's identity cache (`LazyCache` defaults) ---

/// Reloaded this long before they expire.
const CREDS_BUFFER: Duration = Duration::from_secs(10);
/// Reloaded after this when they carry no expiry (env, static profile keys).
const CREDS_DEFAULT_EXPIRATION: Duration = Duration::from_secs(15 * 60);

/// The credentials of one SDK configuration, shared by every lean client built from it (as the
/// SDK clients share the configuration's identity cache): a `run` file's lines do not each ask
/// the provider chain. No provider (`--no-sign-request`) means unsigned requests.
pub(crate) struct CredsCache {
    provider: Option<SharedCredentialsProvider>,
    cached: RwLock<Option<(Credentials, Instant)>>,
    loading: tokio::sync::Mutex<()>,
}

impl CredsCache {
    pub(crate) fn new(provider: Option<SharedCredentialsProvider>) -> CredsCache {
        CredsCache {
            provider,
            cached: RwLock::new(None),
            loading: tokio::sync::Mutex::const_new(()),
        }
    }

    fn fresh(&self) -> Option<Credentials> {
        let cached = self.cached.read().unwrap_or_else(|e| e.into_inner());
        cached
            .as_ref()
            .filter(|(_, until)| Instant::now() < *until)
            .map(|(c, _)| c.clone())
    }

    async fn get(&self) -> Result<Option<Credentials>, CredentialsError> {
        let Some(provider) = &self.provider else {
            return Ok(None);
        };
        if let Some(c) = self.fresh() {
            return Ok(Some(c));
        }
        // One load at a time; the others wait for it and take its result.
        let _loading = self.loading.lock().await;
        if let Some(c) = self.fresh() {
            return Ok(Some(c));
        }
        let creds = provider.provide_credentials().await?;
        let lifetime = match creds.expiry() {
            Some(at) => at
                .duration_since(SystemTime::now())
                .unwrap_or_default()
                .saturating_sub(CREDS_BUFFER),
            None => CREDS_DEFAULT_EXPIRATION,
        };
        let until = Instant::now() + lifetime;
        *self.cached.write().unwrap_or_else(|e| e.into_inner()) = Some((creds.clone(), until));
        Ok(Some(creds))
    }
}

// --- Endpoints: the SDK's resolver ---

/// Where a bucket's requests go and how they are signed, from the SDK's endpoint resolver.
struct Resolved {
    /// The URL before the key: `https://bucket.s3.region.amazonaws.com` or, path style,
    /// `http://host:9000/bucket`.
    base: String,
    signing_region: String,
    signing_name: String,
}


// --- The client ---

/// The S3 settings of one lean client: what its SDK client was built with.
pub(crate) struct LeanConfig {
    pub connector: SharedHttpConnector,
    pub credentials: Arc<CredsCache>,
    pub region: String,
    pub endpoint: Option<String>,
    pub force_path_style: Option<bool>,
    pub use_dual_stack: bool,
    pub use_fips: bool,
    /// `--retry-count` + 1.
    pub max_attempts: u32,
    pub request_payer: Option<String>,
    /// The SDK client's stalled-stream protection (its default: on, 5 s grace period).
    pub stalled_stream: StalledStreamProtectionConfig,
}

pub(crate) struct Lean {
    c: LeanConfig,
    /// Resolved endpoints by bucket; `None` for a bucket whose auth scheme is not plain SigV4,
    /// or whose resolution failed (the SDK then answers, with its own error).
    endpoints: RwLock<HashMap<String, Option<Arc<Resolved>>>>,
}

/// What a request that did not succeed ended with, after its retries.
enum Failure {
    /// The service answered with an error status; its body has been read.
    Status(HttpResponse, bytes::Bytes),
    Connector(ConnectorError),
    Credentials(CredentialsError),
    /// The request could not be built or signed.
    Build(anyhow::Error),
}

impl Lean {
    pub(crate) fn new(c: LeanConfig) -> Lean {
        Lean {
            c,
            endpoints: RwLock::default(),
        }
    }

    /// GetObject, whole or with a `Range` header value. `None` when the bucket is not on the lean
    /// path: the caller sends the request through the SDK.
    pub(crate) async fn get_object(
        &self,
        src: &Url,
        range: Option<&str>,
    ) -> Option<anyhow::Result<HttpResponse>> {
        let r = self.resolve(&src.bucket).await?;
        let headers: Vec<(&'static str, String)> =
            range.map(|v| ("range", v.to_string())).into_iter().collect();
        Some(
            self.send("GET", &r, src, &headers)
                .await
                .map(|resp| self.guard_download(resp))
                .map_err(|f| into_error(f, get_object_error)),
        )
    }

    /// Wraps a download's body as the SDK's stalled-stream interceptor does: it fails when the
    /// server sends too little for longer than the grace period.
    fn guard_download(&self, mut resp: HttpResponse) -> HttpResponse {
        if !self.c.stalled_stream.download_enabled() {
            return resp;
        }
        let config = self.c.stalled_stream.clone();
        let body = std::mem::replace(resp.body_mut(), SdkBody::taken());
        *resp.body_mut() = body.map_preserve_contents(move |body| {
            SdkBody::from_body_1_x(MinimumThroughputDownloadBody::new(
                SystemTimeSource::default(),
                TokioSleep::default(),
                body,
                config.clone().into(),
            ))
        });
        resp
    }

    /// HeadObject. `None` as for `get_object`.
    pub(crate) async fn head_object(&self, src: &Url) -> Option<anyhow::Result<HttpResponse>> {
        let r = self.resolve(&src.bucket).await?;
        Some(
            self.send("HEAD", &r, src, &[])
                .await
                .map_err(|f| into_error(f, head_object_error)),
        )
    }

    async fn resolve(&self, bucket: &str) -> Option<Arc<Resolved>> {
        // An access point or Outposts ARN takes settings the lean client does not carry
        // (`use_arn_region`, multi-region access points): the SDK sends those.
        if bucket.starts_with("arn:") {
            return None;
        }
        let cached = self.endpoints.read().unwrap_or_else(|e| e.into_inner()).get(bucket).cloned();
        if let Some(r) = cached {
            return r;
        }
        let r = self.resolve_uncached(bucket).await.map(Arc::new);
        self.endpoints
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(bucket.to_string(), r.clone());
        r
    }

    async fn resolve_uncached(&self, bucket: &str) -> Option<Resolved> {
        let c = &self.c;
        let params = Params::builder()
            .bucket(bucket)
            .region(c.region.clone())
            .use_fips(c.use_fips)
            .use_dual_stack(c.use_dual_stack)
            .set_endpoint(c.endpoint.clone())
            .set_force_path_style(c.force_path_style)
            .build()
            .ok()?;
        let ep = DefaultResolver::new().resolve_endpoint(&params).await.ok()?;
        let (signing_region, signing_name) = match ep.properties().get("authSchemes") {
            // No auth schemes listed: the operation's default, SigV4 for "s3" in the region.
            None => (c.region.clone(), "s3".to_string()),
            Some(Document::Array(schemes)) => sigv4_scheme(schemes.first()?, &c.region)?,
            Some(_) => return None,
        };
        Some(Resolved {
            base: ep.url().trim_end_matches('/').to_string(),
            signing_region,
            signing_name,
        })
    }

    /// Sends the request (built anew for each attempt: a new date and signature) with the SDK's
    /// standard retries, and returns the response if its status is a success.
    async fn send(
        &self,
        method: &str,
        r: &Resolved,
        src: &Url,
        headers: &[(&'static str, String)],
    ) -> Result<HttpResponse, Failure> {
        let mut attempt = 1u32;
        let mut permit: Option<tokio::sync::SemaphorePermit<'static>> = None;
        loop {
            let creds = self.c.credentials.get().await.map_err(Failure::Credentials)?;
            let req = self
                .request(method, r, src, headers, creds.as_ref())
                .map_err(Failure::Build)?;
            let (kind, retry_after, failure) = match self.c.connector.call(req).await {
                Ok(resp) if resp.status().is_success() => {
                    reward_success(permit);
                    return Ok(resp);
                }
                Ok(mut resp) => match read_body(&mut resp).await {
                    Ok(body) => {
                        let code = parse_error(&body).ok().and_then(|p| p.code);
                        let kind = classify_status(resp.status().as_u16(), code.as_deref());
                        // x-amz-retry-after (milliseconds) counts only with a code-based class.
                        let retry_after = code
                            .as_deref()
                            .filter(|c| THROTTLING_CODES.contains(c) || TRANSIENT_CODES.contains(c))
                            .and_then(|_| resp.headers().get("x-amz-retry-after"))
                            .and_then(|v| v.parse::<u64>().ok())
                            .map(Duration::from_millis);
                        (kind, retry_after, Failure::Status(resp, body))
                    }
                    Err(e) => (classify_connector(&e), None, Failure::Connector(e)),
                },
                Err(e) => (classify_connector(&e), None, Failure::Connector(e)),
            };
            let Some(kind) = kind else {
                return Err(failure);
            };
            if attempt >= self.c.max_attempts {
                return Err(failure);
            }
            let delay = retry_after.map_or_else(|| backoff(attempt), |d| d.min(MAX_BACKOFF));
            let cost = if kind == ErrorKind::TransientError {
                RETRY_TIMEOUT_COST
            } else {
                RETRY_COST
            };
            match RETRY_QUOTA.try_acquire_many(cost) {
                // The retry before this one, if any, keeps its tokens: they are spent.
                Ok(p) => {
                    if let Some(old) = permit.replace(p) {
                        old.forget();
                    }
                }
                Err(_) => return Err(failure),
            }
            tokio::time::sleep(delay).await;
            attempt += 1;
        }
    }

    fn request(
        &self,
        method: &str,
        r: &Resolved,
        src: &Url,
        headers: &[(&'static str, String)],
        creds: Option<&Credentials>,
    ) -> anyhow::Result<HttpRequest> {
        let mut uri = format!("{}/{}", r.base, encode(&src.path, true));
        if !src.version_id.is_empty() {
            uri.push_str("?versionId=");
            uri.push_str(&encode(&src.version_id, false));
        }
        let uri: http::Uri = uri.parse()?;
        let host = uri
            .authority()
            .map(|a| a.as_str().to_string())
            .ok_or_else(|| anyhow::anyhow!("endpoint without a host: {uri}"))?;
        let mut b = http::Request::builder()
            .method(method)
            .uri(uri)
            .header("host", host)
            .header("user-agent", USER_AGENT);
        for (k, v) in headers {
            b = b.header(*k, v);
        }
        if let Some(p) = &self.c.request_payer {
            b = b.header("x-amz-request-payer", p);
        }
        let mut req = b.body(SdkBody::empty())?;
        if let Some(creds) = creds {
            sign(&mut req, creds, r)?;
        }
        Ok(HttpRequest::try_from(req)?)
    }
}

/// The first auth scheme, when it is SigV4: its signing region (the client's when absent) and
/// name.
fn sigv4_scheme(scheme: &Document, region: &str) -> Option<(String, String)> {
    let Document::Object(m) = scheme else {
        return None;
    };
    let text = |k: &str| match m.get(k) {
        Some(Document::String(s)) => Some(s.clone()),
        _ => None,
    };
    if text("name")?.as_str() != "sigv4" {
        return None;
    }
    let region = text("signingRegion").unwrap_or_else(|| region.to_string());
    let name = text("signingName").unwrap_or_else(|| "s3".to_string());
    Some((region, name))
}

/// Percent-encodes a key (keeping `/`, as the SDK's greedy label) or a query value: everything
/// but the unreserved characters.
fn encode(s: &str, keep_slash: bool) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char)
            }
            b'/' if keep_slash => out.push('/'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// SigV4 for S3, as the SDK signs: single URI encoding, no path normalisation, the payload hash
/// in `x-amz-content-sha256` (of the empty body: the requests here have none), the session
/// token when there is one.
fn sign(req: &mut http::Request<SdkBody>, creds: &Credentials, r: &Resolved) -> anyhow::Result<()> {
    use aws_sigv4::http_request::{
        sign, PayloadChecksumKind, PercentEncodingMode, SignableBody, SignableRequest,
        SigningSettings, UriPathNormalizationMode,
    };
    use aws_sigv4::sign::v4;

    let mut settings = SigningSettings::default();
    settings.percent_encoding_mode = PercentEncodingMode::Single;
    settings.uri_path_normalization_mode = UriPathNormalizationMode::Disabled;
    settings.payload_checksum_kind = PayloadChecksumKind::XAmzSha256;
    let identity: aws_smithy_runtime_api::client::identity::Identity = creds.clone().into();
    let params: aws_sigv4::http_request::SigningParams<'_> = v4::SigningParams::builder()
        .identity(&identity)
        .region(&r.signing_region)
        .name(&r.signing_name)
        .time(SystemTime::now())
        .settings(settings)
        .build()?
        .into();
    let headers: Vec<(String, String)> = req
        .headers()
        .iter()
        // Values are UTF-8 (they are built from strings); `to_str` would refuse non-ASCII ones.
        .map(|(k, v)| (k.as_str().to_string(), String::from_utf8_lossy(v.as_bytes()).into_owned()))
        .collect();
    let signable = SignableRequest::new(
        req.method().as_str(),
        req.uri().to_string(),
        headers.iter().map(|(k, v)| (k.as_str(), v.as_str())),
        SignableBody::Bytes(&[]),
    )?;
    let (instructions, _signature) = sign(signable, &params)?.into_parts();
    instructions.apply_to_request_http1x(req);
    Ok(())
}

/// Reads an error response's body, leaving a copy in the response (the error keeps it).
async fn read_body(resp: &mut HttpResponse) -> Result<bytes::Bytes, ConnectorError> {
    let body = std::mem::replace(resp.body_mut(), SdkBody::taken());
    let bytes = ByteStream::new(body)
        .collect()
        .await
        .map_err(|e| ConnectorError::io(e.into()))?
        .into_bytes();
    *resp.body_mut() = SdkBody::from(bytes.clone());
    Ok(bytes)
}

// --- Errors: the values the SDK's deserializers build ---

/// What an S3 error body says (`rest_xml_unwrapped_errors`: an `<Error>` root).
#[derive(Debug, Default, PartialEq)]
struct Parsed {
    code: Option<String>,
    message: Option<String>,
    storage_class: Option<String>,
    access_tier: Option<String>,
}

fn parse_error(body: &[u8]) -> Result<Parsed, aws_smithy_xml::decode::XmlDecodeError> {
    use aws_smithy_xml::decode::{try_data, Document as Xml, XmlDecodeError};
    let mut doc = Xml::try_from(body)?;
    let mut root = doc.root_element()?;
    if root.start_el().local() != "Error" {
        return Err(XmlDecodeError::custom("expected error as root tag"));
    }
    let mut p = Parsed::default();
    while let Some(mut tag) = root.next_tag() {
        let field = match tag.start_el().local() {
            "Code" => &mut p.code,
            "Message" => &mut p.message,
            "StorageClass" => &mut p.storage_class,
            "AccessTier" => &mut p.access_tier,
            _ => continue,
        };
        *field = Some(try_data(&mut tag)?.to_string());
    }
    Ok(p)
}

/// The error metadata the SDK builds (`parse_http_error_metadata` and the request-id headers): a
/// bodiless 404 is `NotFound`.
fn error_metadata(resp: &HttpResponse, body: &[u8], parsed: &Parsed) -> ErrorMetadata {
    let mut b = ErrorMetadata::builder();
    let code = parsed
        .code
        .clone()
        .or_else(|| (body.is_empty() && resp.status().as_u16() == 404).then(|| "NotFound".into()));
    if let Some(c) = code {
        b = b.code(c);
    }
    if let Some(m) = &parsed.message {
        b = b.message(m.clone());
    }
    let headers = resp.headers();
    if let Some(v) = headers.get("x-amz-id-2") {
        b = b.custom("s3_extended_request_id", v.to_string());
    }
    if let Some(v) = headers.get("x-amzn-requestid").or(headers.get("x-amz-request-id")) {
        b = b.custom("aws_request_id", v.to_string());
    }
    b.build()
}

/// The parsed body, or what the SDK makes of one it cannot parse (an `unhandled` error with the
/// XML error as its source). An empty body parses to nothing.
fn parsed_or<E>(
    body: &[u8],
    unhandled: impl FnOnce(aws_smithy_xml::decode::XmlDecodeError) -> E,
) -> Result<Parsed, E> {
    if body.is_empty() {
        return Ok(Parsed::default());
    }
    parse_error(body).map_err(unhandled)
}

fn get_object_error(resp: &HttpResponse, body: &[u8]) -> GetObjectError {
    use aws_sdk_s3::types::error::{InvalidObjectState, NoSuchKey};
    use aws_sdk_s3::types::{IntelligentTieringAccessTier, StorageClass};
    let parsed = match parsed_or(body, |e| GetObjectError::unhandled(e)) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let meta = error_metadata(resp, body, &parsed);
    let message = parsed.message.clone();
    match meta.code().map(str::to_owned).as_deref() {
        None => GetObjectError::unhandled(meta),
        Some("NoSuchKey") => GetObjectError::NoSuchKey(
            NoSuchKey::builder().set_message(message).meta(meta).build(),
        ),
        Some("InvalidObjectState") => GetObjectError::InvalidObjectState(
            InvalidObjectState::builder()
                .set_message(message)
                .set_storage_class(parsed.storage_class.as_deref().map(StorageClass::from))
                .set_access_tier(
                    parsed
                        .access_tier
                        .as_deref()
                        .map(IntelligentTieringAccessTier::from),
                )
                .meta(meta)
                .build(),
        ),
        Some(_) => GetObjectError::generic(meta),
    }
}

fn head_object_error(resp: &HttpResponse, body: &[u8]) -> HeadObjectError {
    use aws_sdk_s3::types::error::NotFound;
    let parsed = match parsed_or(body, |e| HeadObjectError::unhandled(e)) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let meta = error_metadata(resp, body, &parsed);
    let message = parsed.message.clone();
    match meta.code().map(str::to_owned).as_deref() {
        None => HeadObjectError::unhandled(meta),
        Some("NotFound") => {
            HeadObjectError::NotFound(NotFound::builder().set_message(message).meta(meta).build())
        }
        Some(_) => HeadObjectError::generic(meta),
    }
}

/// The error the SDK returns for the same outcome.
fn into_error<E>(f: Failure, op_error: impl FnOnce(&HttpResponse, &[u8]) -> E) -> anyhow::Error
where
    E: std::error::Error + Send + Sync + 'static,
{
    match f {
        Failure::Status(resp, body) => {
            let e = op_error(&resp, &body);
            SdkError::<E, HttpResponse>::service_error(e, resp).into()
        }
        Failure::Connector(e) => SdkError::<E, HttpResponse>::dispatch_failure(e).into(),
        // The SDK reports a credentials failure as a dispatch failure of kind "other".
        Failure::Credentials(e) => {
            SdkError::<E, HttpResponse>::dispatch_failure(ConnectorError::other(e.into(), None))
                .into()
        }
        Failure::Build(e) => SdkError::<E, HttpResponse>::construction_failure(e).into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aws_smithy_runtime_api::client::http::HttpConnectorFuture;

    #[test]
    fn keys_are_encoded_as_the_sdk_encodes_labels() {
        assert_eq!(encode("a b/c+d=é~_.-", true), "a%20b/c%2Bd%3D%C3%A9~_.-");
        assert_eq!(encode("v/1+2", false), "v%2F1%2B2");
    }

    #[test]
    fn statuses_and_codes_are_classified_as_the_sdk_does() {
        assert_eq!(classify_status(503, Some("SlowDown")), Some(ErrorKind::ThrottlingError));
        assert_eq!(classify_status(400, Some("RequestTimeout")), Some(ErrorKind::TransientError));
        assert_eq!(classify_status(500, None), Some(ErrorKind::TransientError));
        let transient = Some(ErrorKind::TransientError);
        assert_eq!(classify_status(503, Some("ServiceUnavailable")), transient);
        assert_eq!(classify_status(404, Some("NoSuchKey")), None);
        assert_eq!(classify_status(403, Some("AccessDenied")), None);
        assert_eq!(classify_status(429, None), None);
    }

    #[test]
    fn backoff_is_full_jitter_capped_at_20_s() {
        for attempt in 1..12 {
            let d = backoff(attempt);
            let cap = INITIAL_BACKOFF.mul_f64(f64::from(1u32 << (attempt - 1))).min(MAX_BACKOFF);
            assert!(d <= cap, "attempt {attempt}: {d:?} > {cap:?}");
        }
        assert!(backoff(40) <= MAX_BACKOFF);
    }

    #[test]
    fn error_bodies_parse_as_s3_writes_them() {
        let body = b"<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<Error><Code>NoSuchKey</Code>\
            <Message>The specified key does not exist.</Message><Key>a&amp;b</Key>\
            <RequestId>r</RequestId></Error>";
        let p = parse_error(body).unwrap();
        assert_eq!(p.code.as_deref(), Some("NoSuchKey"));
        assert_eq!(p.message.as_deref(), Some("The specified key does not exist."));
        assert!(parse_error(b"<html>busy</html>").is_err());
    }

    fn response(status: u16, headers: &[(&str, &str)], body: &'static [u8]) -> HttpResponse {
        let mut b = http::Response::builder().status(status);
        for (k, v) in headers {
            b = b.header(*k, *v);
        }
        HttpResponse::try_from(b.body(SdkBody::from(body)).unwrap()).unwrap()
    }

    #[test]
    fn errors_are_the_sdks_modeled_errors() {
        let body: &[u8] = b"<Error><Code>NoSuchKey</Code><Message>gone</Message></Error>";
        let resp = response(404, &[("x-amz-request-id", "req")], body);
        let e = get_object_error(&resp, body);
        assert!(e.is_no_such_key(), "{e:?}");
        assert_eq!(aws_sdk_s3::error::ProvideErrorMetadata::message(&e), Some("gone"));
        assert_eq!(aws_sdk_s3::operation::RequestId::request_id(&e), Some("req"));

        let body: &[u8] = b"<Error><Code>AccessDenied</Code><Message>no</Message></Error>";
        let e = get_object_error(&response(403, &[], body), body);
        assert_eq!(aws_sdk_s3::error::ProvideErrorMetadata::code(&e), Some("AccessDenied"));
        assert!(format!("{e}").contains("AccessDenied"), "{e}");

        let e = head_object_error(&response(404, &[], b""), b"");
        assert!(e.is_not_found(), "{e:?}");
        let e = head_object_error(&response(503, &[], b""), b"");
        assert_eq!(aws_sdk_s3::error::ProvideErrorMetadata::code(&e), None);
    }

    fn resolver(endpoint: Option<&str>, path_style: Option<bool>) -> Lean {
        Lean::new(LeanConfig {
            connector: SharedHttpConnector::new(NoConnector),
            credentials: Arc::new(CredsCache::new(None)),
            region: "eu-west-1".to_string(),
            endpoint: endpoint.map(str::to_string),
            force_path_style: path_style,
            use_dual_stack: false,
            use_fips: false,
            max_attempts: 3,
            request_payer: None,
            stalled_stream: StalledStreamProtectionConfig::disabled(),
        })
    }

    #[derive(Debug)]
    struct NoConnector;
    impl HttpConnector for NoConnector {
        fn call(&self, _: HttpRequest) -> HttpConnectorFuture {
            unreachable!("resolution sends nothing")
        }
    }

    #[tokio::test]
    async fn endpoints_come_from_the_sdks_resolver() {
        let r = resolver(Some("http://127.0.0.1:9000"), Some(true)).resolve("b1").await.unwrap();
        assert_eq!(r.base, "http://127.0.0.1:9000/b1");
        assert_eq!((r.signing_region.as_str(), r.signing_name.as_str()), ("eu-west-1", "s3"));
        // A name the SDK can put in the host (3 to 63 characters, DNS-safe) goes there.
        let r = resolver(None, None).resolve("bkt2").await.unwrap();
        assert_eq!(r.base, "https://bkt2.s3.eu-west-1.amazonaws.com");
        // A shorter one cannot, so it goes in the path, as on the SDK.
        let r = resolver(None, None).resolve("b2").await.unwrap();
        assert_eq!(r.base, "https://s3.eu-west-1.amazonaws.com/b2");
        // An S3 Express directory bucket is not signed with plain SigV4: it stays on the SDK.
        assert!(resolver(None, None).resolve("b3--euw1-az1--x-s3").await.is_none());
    }
}
