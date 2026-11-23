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
//! Uploads carry a CRC32C (D6): a header for an in-memory body (files up to 256 KiB, `pipe`'s
//! parts), and, with `--checksum-trailers`, an aws-chunked trailer for a streamed one (larger
//! files and their parts); some S3-compatible servers refuse aws-chunked bodies, so it is opt-in.
//! The SDK path sends none. Payloads are signed as the SDK signs them: the hash of an in-memory
//! body, UNSIGNED-PAYLOAD for a streamed one.
//!
//! Not yet the SDK's: its upload stall check (`UploadThroughput`, crate-private), which fails an
//! upload whose server stops reading for longer than the grace period. A lean upload waits.
//!
//! Phase 1: GetObject (whole and ranged) and HeadObject. Phase 2: PutObject and multipart
//! uploads (Create, UploadPart, Complete, Abort). Phase 3: ListObjectsV2, ListObjectVersions,
//! DeleteObjects and DeleteObject; their responses become the SDK's output types, so the
//! listings read them unchanged. ListObjects (v1, `--use-list-objects-v1`) stays on the SDK.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant, SystemTime};

use aws_credential_types::provider::error::CredentialsError;
use aws_credential_types::provider::{ProvideCredentials, SharedCredentialsProvider};
use aws_credential_types::Credentials;
use aws_sdk_s3::config::endpoint::{DefaultResolver, Params, ResolveEndpoint};
use aws_sdk_s3::operation::abort_multipart_upload::AbortMultipartUploadError;
use aws_sdk_s3::operation::complete_multipart_upload::CompleteMultipartUploadError;
use aws_sdk_s3::operation::create_multipart_upload::CreateMultipartUploadError;
use aws_sdk_s3::operation::delete_object::DeleteObjectError;
use aws_sdk_s3::operation::delete_objects::{DeleteObjectsError, DeleteObjectsOutput};
use aws_sdk_s3::operation::get_object::GetObjectError;
use aws_sdk_s3::operation::head_object::HeadObjectError;
use aws_sdk_s3::operation::list_object_versions::{
    ListObjectVersionsError, ListObjectVersionsOutput,
};
use aws_sdk_s3::operation::list_objects_v2::{ListObjectsV2Error, ListObjectsV2Output};
use aws_sdk_s3::operation::put_object::PutObjectError;
use aws_sdk_s3::operation::upload_part::UploadPartError;
use aws_sdk_s3::types::CompletedPart;
use aws_smithy_async::rt::sleep::TokioSleep;
use aws_smithy_async::time::SystemTimeSource;
use aws_smithy_runtime::client::http::body::minimum_throughput::MinimumThroughputDownloadBody;
use aws_smithy_runtime_api::box_error::BoxError;
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
/// `429 Too Many Requests` is a throttling error whatever its code (#32): MinIO and others send
/// `TooManyRequests`, which the SDK's list lacks, so rs5cmd adds a classifier for the status on
/// the SDK path (`too_many_requests` in s3.rs) and the lean path matches it.
const TOO_MANY_REQUESTS: u16 = 429;

/// Whether an error response is retried, and as what: the AWS error code first (throttling, then
/// transient codes), then the status (429 throttling, the 5xx above transient).
fn classify_status(status: u16, code: Option<&str>) -> Option<ErrorKind> {
    match code {
        Some(c) if THROTTLING_CODES.contains(&c) => Some(ErrorKind::ThrottlingError),
        Some(c) if TRANSIENT_CODES.contains(&c) => Some(ErrorKind::TransientError),
        _ if status == TOO_MANY_REQUESTS => Some(ErrorKind::ThrottlingError),
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
    /// `--checksum-trailers`: streamed uploads carry a CRC32C too, as an aws-chunked trailer.
    pub trailers: bool,
}

pub(crate) struct Lean {
    c: LeanConfig,
    /// Resolved endpoints by bucket; `None` for a bucket whose auth scheme is not plain SigV4,
    /// or whose resolution failed (the SDK then answers, with its own error).
    endpoints: RwLock<HashMap<String, Option<Arc<Resolved>>>>,
}

/// What a request that did not succeed ended with, after its retries.
enum Failure {
    /// The service answered with an error: an error status, or a success carrying an `<Error>`
    /// document. Its body has been read.
    Status(HttpResponse, bytes::Bytes),
    Connector(ConnectorError),
    Credentials(CredentialsError),
    /// The request could not be built or signed.
    Build(anyhow::Error),
}

/// One lean request: what it sends besides the signature, the date and the fixed headers.
struct Request<'a> {
    method: &'static str,
    key: &'a str,
    /// In order; a name without a value is sent bare (`?uploads`), an empty value as `name=`,
    /// as the SDK sends them.
    query: Vec<(&'static str, Option<String>)>,
    headers: Vec<(String, String)>,
    /// Rebuilt for each attempt when it can be (`SdkBody::try_clone`). A body that cannot be is
    /// sent once, without retries, as the SDK does.
    body: SdkBody,
    /// Send the body's CRC32C as an aws-chunked trailer (a streamed body; an in-memory one gets
    /// it as a header, `checksum_header`).
    trailer: bool,
    /// Read the response whole: a success carrying an `<Error>` document is an error, as the SDK
    /// decides for the operations without a streamed response.
    buffered: bool,
    /// Send `x-amz-request-payer` (`--request-payer`), as every SDK request rs5cmd builds does
    /// but the versions listing's.
    payer: bool,
}

impl<'a> Request<'a> {
    fn new(method: &'static str, key: &'a str) -> Request<'a> {
        Request {
            method,
            key,
            query: Vec::new(),
            headers: Vec::new(),
            body: SdkBody::empty(),
            trailer: false,
            buffered: true,
            payer: true,
        }
    }

    fn version(mut self, src: &Url) -> Self {
        if !src.version_id.is_empty() {
            self.query.push(("versionId", Some(src.version_id.clone())));
        }
        self
    }

    /// The body, with its CRC32C: as a header when it is in memory (returned), else as a trailer
    /// when `trailers` (`--checksum-trailers`), else none.
    fn checksummed(mut self, body: SdkBody, trailers: bool) -> (Self, Option<String>) {
        let header = body.bytes().map(crc32c);
        match &header {
            Some(v) => self.headers.push((CHECKSUM_HEADER.to_string(), v.clone())),
            None => self.trailer = trailers,
        }
        self.body = body;
        (self, header)
    }
}

impl Lean {
    pub(crate) fn new(c: LeanConfig) -> Lean {
        Lean {
            c,
            endpoints: RwLock::default(),
        }
    }

    /// Whether this client sends `bucket`'s requests itself (see `resolve`). When it does not,
    /// the caller sends them through the SDK.
    pub(crate) async fn serves(&self, bucket: &str) -> bool {
        self.resolve(bucket).await.is_some()
    }

    /// Whether uploads that carry a checksum include the streamed ones (`--checksum-trailers`):
    /// a multipart upload declares the algorithm only when every part will carry one.
    pub(crate) fn trailers(&self) -> bool {
        self.c.trailers
    }

    /// GetObject, whole or with a `Range` header value. `None` when the bucket is not on the lean
    /// path: the caller sends the request through the SDK.
    pub(crate) async fn get_object(
        &self,
        src: &Url,
        range: Option<&str>,
    ) -> Option<anyhow::Result<HttpResponse>> {
        let r = self.resolve(&src.bucket).await?;
        let mut req = Request::new("GET", &src.path).version(src);
        req.buffered = false;
        if let Some(v) = range {
            req.headers.push(("range".to_string(), v.to_string()));
        }
        Some(
            self.send(req, &r)
                .await
                .map(|(resp, _)| self.guard_download(resp))
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
        let mut req = Request::new("HEAD", &src.path).version(src);
        req.buffered = false;
        Some(
            self.send(req, &r)
                .await
                .map(|(resp, _)| resp)
                .map_err(|f| into_error(f, head_object_error)),
        )
    }

    /// PutObject of `body` with the object's `headers` (content type, metadata, conditions), and
    /// its CRC32C (`Request::checksummed`).
    pub(crate) async fn put_object(
        &self,
        dst: &Url,
        body: SdkBody,
        headers: Vec<(String, String)>,
    ) -> anyhow::Result<()> {
        let mut req = Request::new("PUT", &dst.path);
        req.headers = headers;
        let (req, _) = req.checksummed(body, self.c.trailers);
        self.call(&dst.bucket, req, put_object_error).await.map(drop)
    }

    /// CreateMultipartUpload; the upload id, if the response has one. `checksummed_parts`: every
    /// part will carry a CRC32C, so the upload declares the algorithm (S3 then requires one with
    /// each part and in CompleteMultipartUpload).
    pub(crate) async fn create_multipart_upload(
        &self,
        dst: &Url,
        headers: Vec<(String, String)>,
        checksummed_parts: bool,
    ) -> anyhow::Result<Option<String>> {
        let mut req = Request::new("POST", &dst.path);
        req.query.push(("uploads", None));
        req.headers = headers;
        if checksummed_parts {
            req.headers
                .push(("x-amz-checksum-algorithm".to_string(), "CRC32C".to_string()));
        }
        let (_, body) = self.call(&dst.bucket, req, create_multipart_upload_error).await?;
        Ok(xml_text(&body, "UploadId"))
    }

    /// UploadPart; the part as CompleteMultipartUpload lists it (its ETag and, when it carried
    /// one, its CRC32C).
    pub(crate) async fn upload_part(
        &self,
        dst: &Url,
        upload_id: &str,
        part_number: i32,
        body: SdkBody,
    ) -> anyhow::Result<CompletedPart> {
        let mut req = Request::new("PUT", &dst.path);
        req.query.push(("partNumber", Some(part_number.to_string())));
        req.query.push(("uploadId", Some(upload_id.to_string())));
        let (req, sent) = req.checksummed(body, self.c.trailers);
        let checksummed = sent.is_some() || req.trailer;
        let (resp, _) = self.call(&dst.bucket, req, upload_part_error).await?;
        let h = resp.headers();
        // The stored part's checksum, read from the response as the SDK path reads it (for a
        // header it is the value sent); none for a part sent without one.
        let crc = h
            .get(CHECKSUM_HEADER)
            .map(str::to_owned)
            .or(sent)
            .filter(|_| checksummed);
        Ok(CompletedPart::builder()
            .part_number(part_number)
            .set_e_tag(h.get("etag").map(str::to_owned))
            .set_checksum_crc32_c(crc)
            .build())
    }

    /// CompleteMultipartUpload of `parts` (in part-number order), with `headers` (conditions).
    pub(crate) async fn complete_multipart_upload(
        &self,
        dst: &Url,
        upload_id: &str,
        parts: &[CompletedPart],
        headers: Vec<(String, String)>,
    ) -> anyhow::Result<()> {
        let mut req = Request::new("POST", &dst.path);
        req.query.push(("uploadId", Some(upload_id.to_string())));
        req.headers = headers;
        req.headers
            .push(("content-type".to_string(), "application/xml".to_string()));
        req.body = SdkBody::from(complete_body(parts));
        self.call(&dst.bucket, req, complete_multipart_upload_error)
            .await
            .map(drop)
    }

    /// AbortMultipartUpload.
    pub(crate) async fn abort_multipart_upload(
        &self,
        dst: &Url,
        upload_id: &str,
    ) -> anyhow::Result<()> {
        let mut req = Request::new("DELETE", &dst.path);
        req.query.push(("uploadId", Some(upload_id.to_string())));
        self.call(&dst.bucket, req, abort_multipart_upload_error)
            .await
            .map(drop)
    }

    /// One ListObjectsV2 page; keys come URL-encoded (`encoding-type=url`), as `list_v2` asks.
    pub(crate) async fn list_objects_v2(
        &self,
        bucket: &str,
        prefix: &str,
        delimiter: Option<&str>,
        start_after: Option<&str>,
        token: Option<&str>,
    ) -> anyhow::Result<ListObjectsV2Output> {
        let mut req = Request::new("GET", "");
        req.query.push(("list-type", Some("2".to_string())));
        if let Some(t) = token {
            req.query.push(("continuation-token", Some(t.to_string())));
        }
        if let Some(d) = delimiter {
            req.query.push(("delimiter", Some(d.to_string())));
        }
        req.query.push(("encoding-type", Some("url".to_string())));
        req.query.push(("prefix", Some(prefix.to_string())));
        if let Some(s) = start_after {
            req.query.push(("start-after", Some(s.to_string())));
        }
        let (resp, body) = self.call(bucket, req, list_objects_v2_error).await?;
        parse_list_v2(&body).map_err(|e| {
            SdkError::<_, HttpResponse>::service_error(ListObjectsV2Error::unhandled(e), resp)
                .into()
        })
    }

    /// One ListObjectVersions page, URL-encoded as `list_object_versions` asks; without
    /// `x-amz-request-payer`, as its SDK request.
    pub(crate) async fn list_object_versions(
        &self,
        bucket: &str,
        prefix: &str,
        delimiter: Option<&str>,
        key_marker: Option<&str>,
        version_marker: Option<&str>,
    ) -> anyhow::Result<ListObjectVersionsOutput> {
        let mut req = Request::new("GET", "");
        req.payer = false;
        req.query.push(("versions", None));
        if let Some(d) = delimiter {
            req.query.push(("delimiter", Some(d.to_string())));
        }
        req.query.push(("encoding-type", Some("url".to_string())));
        if let Some(k) = key_marker {
            req.query.push(("key-marker", Some(k.to_string())));
        }
        req.query.push(("prefix", Some(prefix.to_string())));
        if let Some(v) = version_marker {
            req.query.push(("version-id-marker", Some(v.to_string())));
        }
        let (resp, body) = self.call(bucket, req, list_object_versions_error).await?;
        parse_versions(&body).map_err(|e| {
            SdkError::<_, HttpResponse>::service_error(ListObjectVersionsError::unhandled(e), resp)
                .into()
        })
    }

    /// DeleteObjects of `keys`, with the CRC32 its SDK request carries (the operation requires a
    /// checksum and the SDK's default algorithm is CRC32).
    pub(crate) async fn delete_objects(
        &self,
        bucket: &str,
        keys: &[&str],
    ) -> anyhow::Result<DeleteObjectsOutput> {
        let mut req = Request::new("POST", "");
        req.query.push(("delete", None));
        let body = delete_body(keys);
        req.headers
            .push(("content-type".to_string(), "application/xml".to_string()));
        req.headers
            .push(("x-amz-checksum-crc32".to_string(), crc32(body.as_bytes())));
        // The request's ChecksumAlgorithm member, which the SDK sets to the checksum it sends.
        req.headers
            .push(("x-amz-sdk-checksum-algorithm".to_string(), "CRC32".to_string()));
        req.body = SdkBody::from(body);
        let (resp, body) = self.call(bucket, req, delete_objects_error).await?;
        parse_deleted(&body).map_err(|e| {
            SdkError::<_, HttpResponse>::service_error(DeleteObjectsError::unhandled(e), resp)
                .into()
        })
    }

    /// DeleteObject (of a version, when the URL names one).
    pub(crate) async fn delete_object(&self, src: &Url) -> anyhow::Result<()> {
        let req = Request::new("DELETE", &src.path).version(src);
        self.call(&src.bucket, req, delete_object_error)
            .await
            .map(drop)
    }

    /// Sends `req` to `bucket` (which `serves` confirmed) and returns the response and, for a
    /// buffered one, its body; or the error the SDK returns for the same outcome.
    async fn call<E>(
        &self,
        bucket: &str,
        req: Request<'_>,
        op_error: impl FnOnce(&HttpResponse, &[u8]) -> E,
    ) -> anyhow::Result<(HttpResponse, bytes::Bytes)>
    where
        E: std::error::Error + Send + Sync + 'static,
    {
        let Some(r) = self.resolve(bucket).await else {
            anyhow::bail!("s3://{bucket}: not on the lean data path");
        };
        self.send(req, &r).await.map_err(|f| into_error(f, op_error))
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
    /// standard retries, and returns the response, with its body when `buffered`, if it succeeded.
    async fn send(
        &self,
        mut req: Request<'_>,
        r: &Resolved,
    ) -> Result<(HttpResponse, bytes::Bytes), Failure> {
        let mut template = Some(std::mem::replace(&mut req.body, SdkBody::taken()));
        let mut attempt = 1u32;
        let mut permit: Option<tokio::sync::SemaphorePermit<'static>> = None;
        loop {
            let creds = self.c.credentials.get().await.map_err(Failure::Credentials)?;
            // A body that can be rebuilt stays the template for the next attempt; one that cannot
            // is sent as it is, once.
            let (body, retryable) = match template.as_ref().and_then(SdkBody::try_clone) {
                Some(b) => (b, true),
                None => (template.take().unwrap_or_else(SdkBody::taken), false),
            };
            let http_req = self
                .request(r, &req, body, creds.as_ref())
                .map_err(Failure::Build)?;
            let (kind, retry_after, failure) = match self.c.connector.call(http_req).await {
                Ok(resp) if resp.status().is_success() && !req.buffered => {
                    reward_success(permit);
                    return Ok((resp, bytes::Bytes::new()));
                }
                Ok(mut resp) => match read_body(&mut resp).await {
                    Ok(body) if resp.status().is_success() && !body_is_error(&body) => {
                        reward_success(permit);
                        return Ok((resp, body));
                    }
                    Ok(body) => judge(resp, body),
                    Err(e) => (classify_connector(&e), None, Failure::Connector(e)),
                },
                Err(e) => (classify_connector(&e), None, Failure::Connector(e)),
            };
            let Some(kind) = kind else {
                return Err(failure);
            };
            if attempt >= self.c.max_attempts || !retryable {
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
            log_retry(req.method, attempt, kind, delay, &failure);
            tokio::time::sleep(delay).await;
            attempt += 1;
        }
    }

    fn request(
        &self,
        r: &Resolved,
        req: &Request<'_>,
        body: SdkBody,
        creds: Option<&Credentials>,
    ) -> anyhow::Result<HttpRequest> {
        let mut uri = format!("{}/{}", r.base, encode(req.key, true));
        for (i, (k, v)) in req.query.iter().enumerate() {
            uri.push(if i == 0 { '?' } else { '&' });
            uri.push_str(k);
            if let Some(v) = v {
                uri.push('=');
                uri.push_str(&encode(v, false));
            }
        }
        let uri: http::Uri = uri.parse()?;
        let host = uri
            .authority()
            .map(|a| a.as_str().to_string())
            .ok_or_else(|| anyhow::anyhow!("endpoint without a host: {uri}"))?;
        let mut b = http::Request::builder()
            .method(req.method)
            .uri(uri)
            .header("host", host)
            .header("user-agent", USER_AGENT);
        let mut content_encoding = None;
        for (k, v) in &req.headers {
            if req.trailer && k == "content-encoding" {
                content_encoding = Some(v.as_str());
                continue;
            }
            b = b.header(k.as_str(), v.as_str());
        }
        if let Some(p) = self.c.request_payer.as_ref().filter(|_| req.payer) {
            b = b.header("x-amz-request-payer", p);
        }
        let body = if req.trailer {
            let len = body
                .content_length()
                .ok_or_else(|| anyhow::anyhow!("a streamed upload body without a length"))?;
            // An object's own content encoding comes after aws-chunked.
            let encoding = match content_encoding {
                Some(e) => format!("aws-chunked,{e}"),
                None => "aws-chunked".to_string(),
            };
            b = b
                .header("content-encoding", encoding)
                .header("x-amz-decoded-content-length", len)
                .header("x-amz-trailer", CHECKSUM_HEADER)
                .header("content-length", aws_chunked_len(len));
            if creds.is_none() {
                b = b.header("x-amz-content-sha256", STREAMING_TRAILER);
            }
            aws_chunked(body, len)
        } else {
            // A request with a payload says its length (the SDK's serializers); a GET or HEAD
            // says nothing.
            let len = body
                .content_length()
                .filter(|&n| n > 0 || req.method == "PUT" || req.method == "POST");
            if let Some(n) = len {
                b = b.header("content-length", n);
            }
            body
        };
        let mut http_req = b.body(body)?;
        if let Some(creds) = creds {
            sign(&mut http_req, creds, r, req.trailer)?;
        }
        Ok(HttpRequest::try_from(http_req)?)
    }
}

/// An error response, or a success carrying an `<Error>` document: whether it is retried, and
/// after how long if the service says (`x-amz-retry-after`, milliseconds, counted only with a
/// code-based class).
fn judge(resp: HttpResponse, body: bytes::Bytes) -> (Option<ErrorKind>, Option<Duration>, Failure) {
    let code = parse_error(&body).ok().and_then(|p| p.code);
    let kind = classify_status(resp.status().as_u16(), code.as_deref());
    let retry_after = code
        .as_deref()
        .filter(|c| THROTTLING_CODES.contains(c) || TRANSIENT_CODES.contains(c))
        .and_then(|_| resp.headers().get("x-amz-retry-after"))
        .and_then(|v| v.parse::<u64>().ok())
        .map(Duration::from_millis);
    (kind, retry_after, Failure::Status(resp, body))
}

/// One debug line per retry, worded as the SDK's standard strategy words its own (`attempt #N
/// failed with ...; retrying after ...`), so that one filter counts the retries of both data
/// paths (`RUST_LOG=debug`); the method, status and S3 error code are fields.
fn log_retry(method: &str, attempt: u32, kind: ErrorKind, delay: Duration, failure: &Failure) {
    let (status, code) = match failure {
        Failure::Status(resp, body) => {
            let code = parse_error(body).ok().and_then(|p| p.code).unwrap_or_default();
            (resp.status().as_u16().to_string(), code)
        }
        _ => ("none".to_string(), String::new()),
    };
    tracing::debug!(
        method = %method,
        status = %status,
        code = %code,
        "attempt #{attempt} failed with {kind:?}; retrying after {delay:?}"
    );
}

/// Whether a body is an S3 error document (`rest_xml_unwrapped_errors::body_is_error`).
fn body_is_error(body: &[u8]) -> bool {
    let Ok(mut doc) = aws_smithy_xml::decode::Document::try_from(body) else {
        return false;
    };
    doc.root_element()
        .is_ok_and(|root| root.start_el().matches("Error"))
}

/// The text of `root`'s first child element `name`.
fn xml_text(body: &[u8], name: &str) -> Option<String> {
    use aws_smithy_xml::decode::{try_data, Document as Xml};
    let mut doc = Xml::try_from(body).ok()?;
    let mut root = doc.root_element().ok()?;
    while let Some(mut tag) = root.next_tag() {
        if tag.start_el().local() == name {
            return try_data(&mut tag).ok().map(|d| d.to_string());
        }
    }
    None
}

/// CompleteMultipartUpload's body: the parts, each with its checksum, ETag and number.
fn complete_body(parts: &[CompletedPart]) -> String {
    let mut out =
        String::from("<CompleteMultipartUpload xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\">");
    for p in parts {
        out.push_str("<Part>");
        if let Some(c) = p.checksum_crc32_c() {
            xml_element(&mut out, "ChecksumCRC32C", c);
        }
        if let Some(e) = p.e_tag() {
            xml_element(&mut out, "ETag", e);
        }
        if let Some(n) = p.part_number() {
            xml_element(&mut out, "PartNumber", &n.to_string());
        }
        out.push_str("</Part>");
    }
    out.push_str("</CompleteMultipartUpload>");
    out
}

fn xml_element(out: &mut String, name: &str, text: &str) {
    out.push('<');
    out.push_str(name);
    out.push('>');
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            c => out.push(c),
        }
    }
    out.push_str("</");
    out.push_str(name);
    out.push('>');
}

// --- Listings and deletes: the SDK's output types, read from S3's XML ---

use aws_smithy_xml::decode::{try_data, Document as Xml, ScopedDecoder, XmlDecodeError};

fn xml_date(s: &str) -> Result<aws_smithy_types::DateTime, XmlDecodeError> {
    use aws_smithy_types::date_time::Format;
    aws_smithy_types::DateTime::from_str(s, Format::DateTimeWithOffset)
        .map_err(|_| XmlDecodeError::custom("invalid timestamp"))
}

fn xml_i64(s: &str) -> Result<i64, XmlDecodeError> {
    s.parse()
        .map_err(|_| XmlDecodeError::custom("invalid number"))
}

fn common_prefix(
    tag: &mut ScopedDecoder<'_, '_>,
) -> Result<aws_sdk_s3::types::CommonPrefix, XmlDecodeError> {
    let mut b = aws_sdk_s3::types::CommonPrefix::builder();
    while let Some(mut t) = tag.next_tag() {
        if t.start_el().local() == "Prefix" {
            b = b.prefix(try_data(&mut t)?);
        }
    }
    Ok(b.build())
}

/// A ListObjectsV2 result (`ListBucketResult`): what `list_v2` reads of it.
fn parse_list_v2(body: &[u8]) -> Result<ListObjectsV2Output, XmlDecodeError> {
    use aws_sdk_s3::types::{Object, ObjectStorageClass};
    let mut doc = Xml::try_from(body)?;
    let mut root = doc.root_element()?;
    let mut out = ListObjectsV2Output::builder();
    let (mut contents, mut prefixes) = (Vec::new(), Vec::new());
    while let Some(mut tag) = root.next_tag() {
        match tag.start_el().local() {
            "Contents" => {
                let mut o = Object::builder();
                while let Some(mut t) = tag.next_tag() {
                    match t.start_el().local() {
                        "Key" => o = o.key(try_data(&mut t)?),
                        "ETag" => o = o.e_tag(try_data(&mut t)?),
                        "LastModified" => o = o.last_modified(xml_date(&try_data(&mut t)?)?),
                        "Size" => o = o.size(xml_i64(&try_data(&mut t)?)?),
                        "StorageClass" => {
                            o = o.storage_class(ObjectStorageClass::from(&*try_data(&mut t)?))
                        }
                        _ => {}
                    }
                }
                contents.push(o.build());
            }
            "CommonPrefixes" => prefixes.push(common_prefix(&mut tag)?),
            "NextContinuationToken" => out = out.next_continuation_token(try_data(&mut tag)?),
            "IsTruncated" => out = out.is_truncated(&*try_data(&mut tag)? == "true"),
            _ => {}
        }
    }
    Ok(out
        .set_contents(Some(contents))
        .set_common_prefixes(Some(prefixes))
        .build())
}

/// A ListObjectVersions result (`ListVersionsResult`): what `list_object_versions` reads of it.
fn parse_versions(body: &[u8]) -> Result<ListObjectVersionsOutput, XmlDecodeError> {
    use aws_sdk_s3::types::{DeleteMarkerEntry, ObjectVersion, ObjectVersionStorageClass};
    let mut doc = Xml::try_from(body)?;
    let mut root = doc.root_element()?;
    let mut out = ListObjectVersionsOutput::builder();
    let (mut versions, mut markers, mut prefixes) = (Vec::new(), Vec::new(), Vec::new());
    while let Some(mut tag) = root.next_tag() {
        match tag.start_el().local() {
            "Version" => {
                let mut v = ObjectVersion::builder();
                while let Some(mut t) = tag.next_tag() {
                    match t.start_el().local() {
                        "Key" => v = v.key(try_data(&mut t)?),
                        "VersionId" => v = v.version_id(try_data(&mut t)?),
                        "IsLatest" => v = v.is_latest(&*try_data(&mut t)? == "true"),
                        "ETag" => v = v.e_tag(try_data(&mut t)?),
                        "LastModified" => v = v.last_modified(xml_date(&try_data(&mut t)?)?),
                        "Size" => v = v.size(xml_i64(&try_data(&mut t)?)?),
                        "StorageClass" => {
                            let class = ObjectVersionStorageClass::from(&*try_data(&mut t)?);
                            v = v.storage_class(class)
                        }
                        _ => {}
                    }
                }
                versions.push(v.build());
            }
            "DeleteMarker" => {
                let mut d = DeleteMarkerEntry::builder();
                while let Some(mut t) = tag.next_tag() {
                    match t.start_el().local() {
                        "Key" => d = d.key(try_data(&mut t)?),
                        "VersionId" => d = d.version_id(try_data(&mut t)?),
                        "IsLatest" => d = d.is_latest(&*try_data(&mut t)? == "true"),
                        "LastModified" => d = d.last_modified(xml_date(&try_data(&mut t)?)?),
                        _ => {}
                    }
                }
                markers.push(d.build());
            }
            "CommonPrefixes" => prefixes.push(common_prefix(&mut tag)?),
            "IsTruncated" => out = out.is_truncated(&*try_data(&mut tag)? == "true"),
            "NextKeyMarker" => out = out.next_key_marker(try_data(&mut tag)?),
            "NextVersionIdMarker" => out = out.next_version_id_marker(try_data(&mut tag)?),
            _ => {}
        }
    }
    Ok(out
        .set_versions(Some(versions))
        .set_delete_markers(Some(markers))
        .set_common_prefixes(Some(prefixes))
        .build())
}

/// A DeleteObjects result (`DeleteResult`): the keys deleted and the per-key errors.
fn parse_deleted(body: &[u8]) -> Result<DeleteObjectsOutput, XmlDecodeError> {
    use aws_sdk_s3::types::{DeletedObject, Error as KeyError};
    let mut doc = Xml::try_from(body)?;
    let mut root = doc.root_element()?;
    let (mut deleted, mut errors) = (Vec::new(), Vec::new());
    while let Some(mut tag) = root.next_tag() {
        match tag.start_el().local() {
            "Deleted" => {
                let mut d = DeletedObject::builder();
                while let Some(mut t) = tag.next_tag() {
                    match t.start_el().local() {
                        "Key" => d = d.key(try_data(&mut t)?),
                        "VersionId" => d = d.version_id(try_data(&mut t)?),
                        _ => {}
                    }
                }
                deleted.push(d.build());
            }
            "Error" => {
                let mut e = KeyError::builder();
                while let Some(mut t) = tag.next_tag() {
                    match t.start_el().local() {
                        "Key" => e = e.key(try_data(&mut t)?),
                        "VersionId" => e = e.version_id(try_data(&mut t)?),
                        "Code" => e = e.code(try_data(&mut t)?),
                        "Message" => e = e.message(try_data(&mut t)?),
                        _ => {}
                    }
                }
                errors.push(e.build());
            }
            _ => {}
        }
    }
    Ok(DeleteObjectsOutput::builder()
        .set_deleted(Some(deleted))
        .set_errors(Some(errors))
        .build())
}

/// DeleteObjects' body: the keys, as `delete_chunk` sends them (no version ids).
fn delete_body(keys: &[&str]) -> String {
    let mut out = String::from("<Delete xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\">");
    for k in keys {
        out.push_str("<Object>");
        xml_element(&mut out, "Key", k);
        out.push_str("</Object>");
    }
    out.push_str("</Delete>");
    out
}

// --- Checksums: CRC32C (D6), a header for in-memory bodies, an aws-chunked trailer for streamed
// ones ---

const CHECKSUM_HEADER: &str = "x-amz-checksum-crc32c";
const STREAMING_TRAILER: &str = "STREAMING-UNSIGNED-PAYLOAD-TRAILER";

/// The `x-amz-checksum-crc32c` value of `data`: its CRC32C, big-endian, in base64.
fn crc32c(data: &[u8]) -> String {
    crc32c_value(crc_fast::checksum(crc_fast::CrcAlgorithm::Crc32Iscsi, data) as u32)
}

/// The `x-amz-checksum-crc32` value of `data` (DeleteObjects).
fn crc32(data: &[u8]) -> String {
    crc32c_value(crc_fast::checksum(crc_fast::CrcAlgorithm::Crc32IsoHdlc, data) as u32)
}

/// A CRC, big-endian, in base64.
fn crc32c_value(crc: u32) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(crc.to_be_bytes())
}

/// The SDK's aws-chunked chunk size (aws-runtime's `DEFAULT_CHUNK_SIZE_BYTE`). Servers cap it:
/// MinIO refuses chunks over 16 MiB, so a body cannot go as one chunk.
const AWS_CHUNK: u64 = 64 * 1024;

/// The length of `len` bytes framed by `aws_chunked`, as the SDK counts it: each chunk's size line,
/// data and CRLF, the last (empty) chunk, the trailer (its value always 8 base64 characters) and
/// the closing CRLF.
fn aws_chunked_len(len: u64) -> u64 {
    let chunk = |n: u64| format!("{n:X}").len() as u64 + 2 + n + 2;
    let (full, rest) = (len / AWS_CHUNK, len % AWS_CHUNK);
    full * chunk(AWS_CHUNK)
        + if rest > 0 { chunk(rest) } else { 0 }
        + 3
        + (CHECKSUM_HEADER.len() as u64 + 1 + 8 + 2)
        + 2
}

/// `body` (`len` bytes) in aws-chunked framing with its CRC32C trailer, byte for byte as the SDK
/// frames it: chunks of 64 KiB and a shorter last one. The body's own frames pass through
/// uncopied (split where a chunk ends) between the size lines, the CRC is computed as they go.
fn aws_chunked(body: SdkBody, len: u64) -> SdkBody {
    SdkBody::from_body_1_x(AwsChunked {
        inner: body,
        digest: crc_fast::Digest::new(crc_fast::CrcAlgorithm::Crc32Iscsi),
        left: len,
        chunk_left: 0,
        pending: None,
        out: std::collections::VecDeque::with_capacity(3),
        len: aws_chunked_len(len),
        done: false,
    })
}

struct AwsChunked {
    inner: SdkBody,
    digest: crc_fast::Digest,
    /// Body bytes not yet passed on.
    left: u64,
    /// Bytes of the current chunk not yet passed on (0: between chunks).
    chunk_left: u64,
    /// The part of a body frame beyond the chunk that ended inside it.
    pending: Option<bytes::Bytes>,
    /// Frames ready to go: size lines, data, CRLFs, the trailer.
    out: std::collections::VecDeque<bytes::Bytes>,
    len: u64,
    done: bool,
}

impl http_body::Body for AwsChunked {
    type Data = bytes::Bytes;
    type Error = aws_smithy_types::body::Error;

    fn poll_frame(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<http_body::Frame<bytes::Bytes>, Self::Error>>> {
        use std::task::Poll;
        let this = self.get_mut();
        loop {
            if let Some(f) = this.out.pop_front() {
                return Poll::Ready(Some(Ok(http_body::Frame::data(f))));
            }
            if this.done {
                return Poll::Ready(None);
            }
            let mut data = match this.pending.take() {
                Some(d) => d,
                None => {
                    let next = http_body::Body::poll_frame(std::pin::Pin::new(&mut this.inner), cx);
                    match std::task::ready!(next) {
                        // A file body has no trailers of its own.
                        Some(Ok(frame)) => match frame.into_data() {
                            Ok(d) if !d.is_empty() => d,
                            _ => continue,
                        },
                        Some(Err(e)) => return Poll::Ready(Some(Err(e))),
                        None if this.left > 0 => {
                            let e = format!("body ended {} bytes short of its length", this.left);
                            return Poll::Ready(Some(Err(e.into())));
                        }
                        None => {
                            this.done = true;
                            let crc = crc32c_value(this.digest.finalize() as u32);
                            let tail = format!("0\r\n{CHECKSUM_HEADER}:{crc}\r\n\r\n");
                            this.out.push_back(tail.into());
                            continue;
                        }
                    }
                }
            };
            if this.chunk_left == 0 {
                if this.left == 0 {
                    return Poll::Ready(Some(Err("body longer than its length".into())));
                }
                this.chunk_left = this.left.min(AWS_CHUNK);
                this.out.push_back(format!("{:X}\r\n", this.chunk_left).into());
            }
            let take = (data.len() as u64).min(this.chunk_left) as usize;
            if data.len() > take {
                this.pending = Some(data.split_off(take));
            }
            this.digest.update(&data);
            this.chunk_left -= take as u64;
            this.left -= take as u64;
            this.out.push_back(data);
            if this.chunk_left == 0 {
                this.out.push_back(bytes::Bytes::from_static(b"\r\n"));
            }
        }
    }

    fn is_end_stream(&self) -> bool {
        self.done && self.out.is_empty()
    }

    fn size_hint(&self) -> http_body::SizeHint {
        http_body::SizeHint::with_exact(self.len)
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

/// SigV4 for S3, as the SDK signs: single URI encoding, no path normalisation, the payload in
/// `x-amz-content-sha256` (the hash of an in-memory body, UNSIGNED-PAYLOAD for a streamed one,
/// STREAMING-UNSIGNED-PAYLOAD-TRAILER with a trailer), the session token when there is one.
fn sign(
    req: &mut http::Request<SdkBody>,
    creds: &Credentials,
    r: &Resolved,
    trailer: bool,
) -> anyhow::Result<()> {
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
        match req.body().bytes() {
            _ if trailer => SignableBody::StreamingUnsignedPayloadTrailer,
            Some(bytes) => SignableBody::Bytes(bytes),
            None => SignableBody::UnsignedPayload,
        },
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

/// The operation's error for an error response, as its SDK deserializer builds it: a modeled
/// error for its code (`modeled`, which hands back the metadata otherwise), else a generic one;
/// unhandled when there is no code or the body is not an S3 error document.
fn op_error<E>(
    resp: &HttpResponse,
    body: &[u8],
    unhandled: impl FnOnce(BoxError) -> E,
    generic: impl FnOnce(ErrorMetadata) -> E,
    modeled: impl FnOnce(&str, Option<String>, &Parsed, ErrorMetadata) -> Result<E, ErrorMetadata>,
) -> E {
    let parsed = if body.is_empty() {
        Parsed::default()
    } else {
        match parse_error(body) {
            Ok(p) => p,
            Err(e) => return unhandled(e.into()),
        }
    };
    let meta = error_metadata(resp, body, &parsed);
    let Some(code) = meta.code().map(str::to_owned) else {
        return unhandled(meta.into());
    };
    let message = parsed.message.clone();
    match modeled(&code, message, &parsed, meta) {
        Ok(e) => e,
        Err(meta) => generic(meta),
    }
}

fn get_object_error(resp: &HttpResponse, body: &[u8]) -> GetObjectError {
    use aws_sdk_s3::types::error::{InvalidObjectState, NoSuchKey};
    use aws_sdk_s3::types::{IntelligentTieringAccessTier, StorageClass};
    op_error(
        resp,
        body,
        |e| GetObjectError::unhandled(e),
        GetObjectError::generic,
        |code, message, parsed, meta| match code {
            "NoSuchKey" => Ok(GetObjectError::NoSuchKey(
                NoSuchKey::builder().set_message(message).meta(meta).build(),
            )),
            "InvalidObjectState" => Ok(GetObjectError::InvalidObjectState(
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
            )),
            _ => Err(meta),
        },
    )
}

fn head_object_error(resp: &HttpResponse, body: &[u8]) -> HeadObjectError {
    use aws_sdk_s3::types::error::NotFound;
    op_error(
        resp,
        body,
        |e| HeadObjectError::unhandled(e),
        HeadObjectError::generic,
        |code, message, _, meta| match code {
            "NotFound" => Ok(HeadObjectError::NotFound(
                NotFound::builder().set_message(message).meta(meta).build(),
            )),
            _ => Err(meta),
        },
    )
}

fn put_object_error(resp: &HttpResponse, body: &[u8]) -> PutObjectError {
    use aws_sdk_s3::types::error::{
        EncryptionTypeMismatch, InvalidRequest, InvalidWriteOffset, TooManyParts,
    };
    op_error(
        resp,
        body,
        |e| PutObjectError::unhandled(e),
        PutObjectError::generic,
        |code, message, _, meta| match code {
            "EncryptionTypeMismatch" => Ok(PutObjectError::EncryptionTypeMismatch(
                EncryptionTypeMismatch::builder()
                    .set_message(message)
                    .meta(meta)
                    .build(),
            )),
            "InvalidRequest" => Ok(PutObjectError::InvalidRequest(
                InvalidRequest::builder().set_message(message).meta(meta).build(),
            )),
            "InvalidWriteOffset" => Ok(PutObjectError::InvalidWriteOffset(
                InvalidWriteOffset::builder()
                    .set_message(message)
                    .meta(meta)
                    .build(),
            )),
            "TooManyParts" => Ok(PutObjectError::TooManyParts(
                TooManyParts::builder().set_message(message).meta(meta).build(),
            )),
            _ => Err(meta),
        },
    )
}

fn create_multipart_upload_error(resp: &HttpResponse, body: &[u8]) -> CreateMultipartUploadError {
    op_error(
        resp,
        body,
        |e| CreateMultipartUploadError::unhandled(e),
        CreateMultipartUploadError::generic,
        |_, _, _, meta| Err(meta),
    )
}

fn upload_part_error(resp: &HttpResponse, body: &[u8]) -> UploadPartError {
    op_error(
        resp,
        body,
        |e| UploadPartError::unhandled(e),
        UploadPartError::generic,
        |_, _, _, meta| Err(meta),
    )
}

fn complete_multipart_upload_error(
    resp: &HttpResponse,
    body: &[u8],
) -> CompleteMultipartUploadError {
    op_error(
        resp,
        body,
        |e| CompleteMultipartUploadError::unhandled(e),
        CompleteMultipartUploadError::generic,
        |_, _, _, meta| Err(meta),
    )
}

fn list_objects_v2_error(resp: &HttpResponse, body: &[u8]) -> ListObjectsV2Error {
    use aws_sdk_s3::types::error::NoSuchBucket;
    op_error(
        resp,
        body,
        |e| ListObjectsV2Error::unhandled(e),
        ListObjectsV2Error::generic,
        |code, message, _, meta| match code {
            "NoSuchBucket" => Ok(ListObjectsV2Error::NoSuchBucket(
                NoSuchBucket::builder().set_message(message).meta(meta).build(),
            )),
            _ => Err(meta),
        },
    )
}

fn list_object_versions_error(resp: &HttpResponse, body: &[u8]) -> ListObjectVersionsError {
    op_error(
        resp,
        body,
        |e| ListObjectVersionsError::unhandled(e),
        ListObjectVersionsError::generic,
        |_, _, _, meta| Err(meta),
    )
}

fn delete_objects_error(resp: &HttpResponse, body: &[u8]) -> DeleteObjectsError {
    op_error(
        resp,
        body,
        |e| DeleteObjectsError::unhandled(e),
        DeleteObjectsError::generic,
        |_, _, _, meta| Err(meta),
    )
}

fn delete_object_error(resp: &HttpResponse, body: &[u8]) -> DeleteObjectError {
    op_error(
        resp,
        body,
        |e| DeleteObjectError::unhandled(e),
        DeleteObjectError::generic,
        |_, _, _, meta| Err(meta),
    )
}

fn abort_multipart_upload_error(resp: &HttpResponse, body: &[u8]) -> AbortMultipartUploadError {
    use aws_sdk_s3::types::error::NoSuchUpload;
    op_error(
        resp,
        body,
        |e| AbortMultipartUploadError::unhandled(e),
        AbortMultipartUploadError::generic,
        |code, message, _, meta| match code {
            "NoSuchUpload" => Ok(AbortMultipartUploadError::NoSuchUpload(
                NoSuchUpload::builder().set_message(message).meta(meta).build(),
            )),
            _ => Err(meta),
        },
    )
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
        // #32: 429 is throttling whatever its code; a transient code still wins, as on the SDK
        // path, where the error-code classifier runs after the status classifiers.
        let throttling = Some(ErrorKind::ThrottlingError);
        assert_eq!(classify_status(429, Some("TooManyRequests")), throttling);
        assert_eq!(classify_status(429, None), throttling);
        assert_eq!(classify_status(429, Some("RequestTimeout")), transient);
    }

    #[test]
    fn a_429_is_throttling_and_only_a_listed_code_brings_a_retry_after() {
        // The SDK reads x-amz-retry-after in its error-code classifier, so only for the codes
        // it lists; a 429 alone backs off as usual.
        let body: &'static [u8] = b"<Error><Code>TooManyRequests</Code><Message>slow</Message>\
            </Error>";
        let resp = response(429, &[("x-amz-retry-after", "1500")], body);
        let (kind, after, _) = judge(resp, bytes::Bytes::from_static(body));
        assert_eq!((kind, after), (Some(ErrorKind::ThrottlingError), None));
        let body: &'static [u8] = b"<Error><Code>SlowDown</Code><Message>slow</Message></Error>";
        let resp = response(503, &[("x-amz-retry-after", "1500")], body);
        let (kind, after, _) = judge(resp, bytes::Bytes::from_static(body));
        let wait = Some(Duration::from_millis(1500));
        assert_eq!((kind, after), (Some(ErrorKind::ThrottlingError), wait));
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

    #[test]
    fn upload_errors_are_the_sdks_modeled_errors() {
        let body: &[u8] = b"<Error><Code>InvalidRequest</Code><Message>bad</Message></Error>";
        let e = put_object_error(&response(400, &[], body), body);
        assert!(matches!(e, PutObjectError::InvalidRequest(_)), "{e:?}");
        let body: &[u8] = b"<Error><Code>NoSuchUpload</Code><Message>gone</Message></Error>";
        let e = abort_multipart_upload_error(&response(404, &[], body), body);
        assert!(e.is_no_such_upload(), "{e:?}");
        // CompleteMultipartUpload can answer 200 with an error document.
        let body: &[u8] = b"<Error><Code>InternalError</Code><Message>again</Message></Error>";
        assert!(body_is_error(body));
        let e = complete_multipart_upload_error(&response(200, &[], body), body);
        assert_eq!(aws_sdk_s3::error::ProvideErrorMetadata::code(&e), Some("InternalError"));
    }

    #[test]
    fn upload_ids_and_error_documents_are_read_from_the_body() {
        let body = b"<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<InitiateMultipartUploadResult \
            xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\"><Bucket>b</Bucket><Key>k</Key>\
            <UploadId>u-1</UploadId></InitiateMultipartUploadResult>";
        assert_eq!(xml_text(body, "UploadId").as_deref(), Some("u-1"));
        assert!(!body_is_error(body));
        assert!(!body_is_error(b""));
    }

    #[test]
    fn complete_lists_every_part_with_its_checksum() {
        let parts = [
            CompletedPart::builder()
                .part_number(1)
                .e_tag("\"a&b\"")
                .checksum_crc32_c("4waSgw==")
                .build(),
            CompletedPart::builder().part_number(2).e_tag("\"c\"").build(),
        ];
        assert_eq!(
            complete_body(&parts),
            "<CompleteMultipartUpload xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\">\
             <Part><ChecksumCRC32C>4waSgw==</ChecksumCRC32C><ETag>&quot;a&amp;b&quot;</ETag>\
             <PartNumber>1</PartNumber></Part><Part><ETag>&quot;c&quot;</ETag>\
             <PartNumber>2</PartNumber></Part></CompleteMultipartUpload>"
        );
    }

    #[test]
    fn a_crc32c_header_is_the_big_endian_value_in_base64() {
        // The CRC-32C check value: 0xE3069283 for "123456789".
        assert_eq!(crc32c(b"123456789"), "4waSgw==");
        assert_eq!(crc32c(b""), "AAAAAA==");
    }

    /// A body delivered in the given frames, as a file body delivers its reads.
    struct Frames(std::collections::VecDeque<bytes::Bytes>);
    impl http_body::Body for Frames {
        type Data = bytes::Bytes;
        type Error = aws_smithy_types::body::Error;
        fn poll_frame(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Option<Result<http_body::Frame<bytes::Bytes>, Self::Error>>> {
            let next = self.get_mut().0.pop_front();
            std::task::Poll::Ready(next.map(|b| Ok(http_body::Frame::data(b))))
        }
    }

    async fn framed(data: &[u8], frame: usize, len: u64) -> Result<Vec<u8>, String> {
        let frames = data.chunks(frame).map(bytes::Bytes::copy_from_slice).collect();
        let body = SdkBody::from_body_1_x(Frames(frames));
        let out = ByteStream::new(aws_chunked(body, len)).collect().await;
        out.map(|b| b.into_bytes().to_vec()).map_err(|e| format!("{e:?}"))
    }

    #[tokio::test]
    async fn a_streamed_body_goes_in_64_kib_aws_chunks_and_its_crc32c_trailer() {
        // 3 full chunks and 30,000 bytes more, read in frames that do not line up with them.
        let data: Vec<u8> = (0..3 * 65_536 + 30_000u32).map(|i| (i % 251) as u8).collect();
        let len = data.len() as u64;
        let mut expected = Vec::new();
        for c in data.chunks(65_536) {
            expected.extend_from_slice(format!("{:X}\r\n", c.len()).as_bytes());
            expected.extend_from_slice(c);
            expected.extend_from_slice(b"\r\n");
        }
        let tail = format!("0\r\nx-amz-checksum-crc32c:{}\r\n\r\n", crc32c(&data));
        expected.extend_from_slice(tail.as_bytes());
        for frame in [100_000, 65_536, 7, data.len()] {
            let got = framed(&data, frame, len).await.unwrap();
            assert!(got == expected, "framing differs with {frame}-byte frames");
        }
        assert_eq!(aws_chunked_len(len), expected.len() as u64);
        // Exactly one chunk, and the trailer alone, as the SDK counts them.
        assert_eq!(aws_chunked_len(65_536), 5 + 2 + 65_536 + 2 + 3 + 32 + 2);
        assert_eq!(aws_chunked_len(0), 3 + 32 + 2);
        // A body that does not match its length fails instead of sending a wrong one.
        assert!(framed(&data, 4096, len + 1).await.is_err());
        assert!(framed(&data, 4096, len - 1).await.is_err());
    }

    #[test]
    fn a_listing_page_becomes_the_sdks_output() {
        let body = b"<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
            <ListBucketResult xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\"><Name>b</Name>\
            <Prefix>d%2F</Prefix><KeyCount>2</KeyCount><MaxKeys>1000</MaxKeys>\
            <EncodingType>url</EncodingType><IsTruncated>true</IsTruncated>\
            <Contents><Key>d%2Fa%1B</Key><LastModified>2026-11-02T10:00:00.000Z</LastModified>\
            <ETag>&quot;e1&quot;</ETag><Size>3</Size><StorageClass>STANDARD_IA</StorageClass>\
            </Contents><CommonPrefixes><Prefix>d%2Fsub%2F</Prefix></CommonPrefixes>\
            <NextContinuationToken>t+2=</NextContinuationToken></ListBucketResult>";
        let page = parse_list_v2(body).unwrap();
        let o = &page.contents()[0];
        assert_eq!(o.key(), Some("d%2Fa%1B"));
        assert_eq!(o.e_tag(), Some("\"e1\""));
        assert_eq!(o.size(), Some(3));
        assert_eq!(o.storage_class().map(|c| c.as_str()), Some("STANDARD_IA"));
        assert_eq!(o.last_modified().map(|t| t.secs()), Some(1_793_613_600));
        assert_eq!(page.common_prefixes()[0].prefix(), Some("d%2Fsub%2F"));
        assert_eq!(page.next_continuation_token(), Some("t+2="));
        assert_eq!(page.is_truncated(), Some(true));
    }

    #[test]
    fn a_versions_page_and_a_delete_result_become_the_sdks_outputs() {
        let body = b"<ListVersionsResult><IsTruncated>true</IsTruncated>\
            <NextKeyMarker>k</NextKeyMarker><NextVersionIdMarker>v2</NextVersionIdMarker>\
            <Version><Key>k</Key><VersionId>v1</VersionId><IsLatest>true</IsLatest>\
            <Size>5</Size></Version><DeleteMarker><Key>k</Key><VersionId>v0</VersionId>\
            <IsLatest>false</IsLatest></DeleteMarker></ListVersionsResult>";
        let page = parse_versions(body).unwrap();
        assert_eq!(page.versions()[0].version_id(), Some("v1"));
        assert_eq!(page.versions()[0].size(), Some(5));
        assert_eq!(page.delete_markers()[0].version_id(), Some("v0"));
        assert_eq!(page.next_key_marker(), Some("k"));
        assert_eq!(page.next_version_id_marker(), Some("v2"));
        let body = b"<DeleteResult><Deleted><Key>a&amp;b</Key></Deleted><Error><Key>c</Key>\
            <Code>AccessDenied</Code><Message>Access Denied</Message></Error></DeleteResult>";
        let out = parse_deleted(body).unwrap();
        assert_eq!(out.deleted()[0].key(), Some("a&b"));
        assert_eq!(out.errors()[0].message(), Some("Access Denied"));
        assert!(!body_is_error(body), "an <Error> inside DeleteResult is not an error document");
    }

    #[test]
    fn delete_sends_the_keys_with_a_crc32() {
        assert_eq!(
            delete_body(&["a&b", "c"]),
            "<Delete xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\"><Object><Key>a&amp;b</Key>\
             </Object><Object><Key>c</Key></Object></Delete>"
        );
        // The CRC-32 check value: 0xCBF43926 for "123456789".
        assert_eq!(crc32(b"123456789"), "y/Q5Jg==");
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
            trailers: false,
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
