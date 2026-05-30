//! S3 `Storage` implementation backed by the official `aws-sdk-s3` crate.
//! Ported from s5cmd's `storage/s3.go` (core operations).

use std::path::Path;

use async_trait::async_trait;
use aws_sdk_s3::config::{BehaviorVersion, Region};
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::types::{CompletedMultipartUpload, CompletedPart, Delete, ObjectIdentifier};
use aws_sdk_s3::Client;
use aws_smithy_types::byte_stream::Length;
use futures::stream::{self, StreamExt};
use tokio::sync::mpsc;

use super::url::Url;
use super::{
    Bucket, Metadata, NoObjectFound, Object, ObjectNotFound, ObjectType, Options, StorageClass,
    Storage,
};

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

/// Parses the total object size from a `Content-Range: bytes START-END/TOTAL`
/// header value (the part after the final `/`).
fn parse_content_range_total(cr: Option<&str>) -> Option<u64> {
    cr?.rsplit('/').next()?.trim().parse::<u64>().ok()
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

/// Builds an HTTP client for the AWS SDK whose TLS layer skips certificate
/// verification (and hostname checking) — used only when `--no-verify-ssl`
/// is set, for self-signed HTTPS endpoints.
///
/// The bundled `aws-smithy-http-client` exposes no "insecure"/no-verify hook
/// (its `TlsContext` only lets you ADD trust, and its `Connector` cannot be
/// built from a custom rustls `ClientConfig` through any public API), so we
/// assemble our own hyper + hyper-rustls connector with a dangerous
/// `ServerCertVerifier` and adapt it to the smithy `HttpClient` trait that
/// `aws_sdk_s3::config::Builder::http_client` accepts.
fn build_no_verify_http_client() -> NoVerifyHttpClient {
    use std::sync::Arc;

    // Process-wide default crypto provider (idempotent; the fast path may also
    // install one). Ignore the error if it's already installed.
    let _ = rustls::crypto::ring::default_provider().install_default();
    let provider = Arc::new(rustls::crypto::ring::default_provider());

    let tls_config = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .expect("rustls safe default protocol versions")
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(danger::NoVerify(provider)))
        .with_no_client_auth();

    let https = hyper_rustls::HttpsConnectorBuilder::new()
        .with_tls_config(tls_config)
        .https_or_http()
        .enable_http1()
        .enable_http2()
        .build();

    let client: HyperLegacyClient =
        hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new())
            .build(https);

    NoVerifyHttpClient {
        connector: NoVerifyConnector(std::sync::Arc::new(client)),
    }
}

type HyperLegacyClient = hyper_util::client::legacy::Client<
    hyper_rustls::HttpsConnector<hyper_util::client::legacy::connect::HttpConnector>,
    aws_smithy_types::body::SdkBody,
>;

/// A smithy `HttpClient` that hands out a single shared no-verify connector.
#[derive(Debug, Clone)]
pub(crate) struct NoVerifyHttpClient {
    connector: NoVerifyConnector,
}

impl aws_smithy_runtime_api::client::http::HttpClient for NoVerifyHttpClient {
    fn http_connector(
        &self,
        _settings: &aws_smithy_runtime_api::client::http::HttpConnectorSettings,
        _components: &aws_smithy_runtime_api::client::runtime_components::RuntimeComponents,
    ) -> aws_smithy_runtime_api::client::http::SharedHttpConnector {
        aws_smithy_runtime_api::client::http::SharedHttpConnector::new(self.connector.clone())
    }
}

/// Adapts a hyper 1.x legacy `Client` to the smithy `HttpConnector` trait,
/// mirroring `aws-smithy-http-client`'s internal `Adapter`.
#[derive(Debug, Clone)]
struct NoVerifyConnector(std::sync::Arc<HyperLegacyClient>);

impl aws_smithy_runtime_api::client::http::HttpConnector for NoVerifyConnector {
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
        let client = self.0.clone();
        HttpConnectorFuture::new(async move {
            let response = client
                .request(request)
                .await
                .map_err(|e| ConnectorError::other(e.into(), None))?;
            let (parts, body) = response.into_parts();
            let body = SdkBody::from_body_1_x(body);
            HttpResponse::try_from(http::Response::from_parts(parts, body))
                .map_err(|err| ConnectorError::other(err.into(), None))
        })
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
}

impl S3 {
    /// Builds an S3 client. Honors a custom endpoint (e.g. MinIO) via
    /// `opts.endpoint` or the `AWS_ENDPOINT_URL`/`S3_ENDPOINT_URL` env vars,
    /// switching to path-style addressing in that case.
    pub async fn new(_u: &Url, opts: &Options) -> anyhow::Result<S3> {
        let mut loader = aws_config::defaults(BehaviorVersion::latest());
        if let Some(region) = &opts.region {
            loader = loader.region(Region::new(region.clone()));
        }
        if let Some(profile) = &opts.profile {
            loader = loader.profile_name(profile);
        }
        let shared = loader.load().await;

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
        // Default region fallback so requests against MinIO don't fail config.
        if shared.region().is_none() && opts.region.is_none() {
            builder = builder.region(Region::new("us-east-1"));
        }

        // `--no-verify-ssl`: swap in a custom HTTP client whose rustls config
        // skips certificate verification, for self-signed HTTPS endpoints. The
        // bundled `aws-smithy-http-client` TLS context can only ADD trust, not
        // disable verification, so we build our own hyper + hyper-rustls
        // connector with a dangerous `ServerCertVerifier` and hand it to the
        // SDK via `http_client`. NOTE: this also disables hostname checking —
        // that is the intended effect of the flag.
        if opts.no_verify_ssl {
            builder = builder.http_client(build_no_verify_http_client());
        }

        Ok(S3 {
            client: Client::from_conf(builder.build()),
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
        })
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
        let first = resp.body.collect().await?.into_bytes();

        let file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(dst)?;
        file.set_len(total)?;
        {
            use std::os::unix::fs::FileExt;
            file.write_all_at(&first, 0)?;
        }

        // Small object: the first part was the whole object.
        if total > part_size {
            // Large object: fetch parts 1..n concurrently into the file.
            self.download_remaining_parts(src, file, total, part_size).await?;
        }

        // Restore the preserved mtime, if any (best-effort: a malformed value or
        // an unsupported filesystem must not fail the otherwise-complete copy).
        if let Some(v) = mtime_value {
            if let Err(e) = set_file_mtime_from_value(dst, &v) {
                eprintln!("warning: could not restore mtime on {}: {e:#}", dst.display());
            }
        }
        Ok(())
    }

    /// Single streaming GET to a file. Used as the zero-byte / range-error
    /// fallback for `download`.
    async fn download_single(&self, src: &Url, dst: &Path) -> anyhow::Result<()> {
        let mut body = self.read(src).await?;
        let mut file = tokio::fs::File::create(dst).await?;
        use tokio::io::AsyncWriteExt;
        while let Some(chunk) = body.try_next().await? {
            file.write_all(&chunk).await?;
        }
        file.flush().await?;
        Ok(())
    }

    /// Fetches parts `1..n` (part 0 is already written) as concurrent ranged
    /// GETs written at their offsets into the preallocated file.
    async fn download_remaining_parts(
        &self,
        src: &Url,
        file: std::fs::File,
        total: u64,
        part_size: u64,
    ) -> anyhow::Result<()> {
        let file = std::sync::Arc::new(file);
        let n_parts = total.div_ceil(part_size);

        let tasks = (1..n_parts).map(|i| {
            let offset = i * part_size;
            let len = part_size.min(total - offset);
            let client = self.client.clone();
            let bucket = src.bucket.clone();
            let key = src.path.clone();
            let vid = src.version_id.clone();
            let rp = self.request_payer();
            let file = std::sync::Arc::clone(&file);
            async move {
                let range = format!("bytes={}-{}", offset, offset + len - 1);
                let mut req = client
                    .get_object()
                    .bucket(bucket)
                    .key(key)
                    .range(range)
                    .set_request_payer(rp);
                if !vid.is_empty() {
                    req = req.version_id(vid);
                }
                let resp = req.send().await?;
                let data = resp.body.collect().await?.into_bytes();
                tokio::task::spawn_blocking(move || {
                    use std::os::unix::fs::FileExt;
                    file.write_all_at(&data, offset)
                })
                .await??;
                anyhow::Ok(())
            }
        });

        stream::iter(tasks)
            .buffer_unordered(self.concurrency)
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .collect::<anyhow::Result<Vec<_>>>()?;
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

    /// Files up to this size are read fully into memory for the request body,
    /// avoiding per-request streaming-from-disk overhead (blocking-pool file
    /// reads) that throttles many small concurrent uploads.
    const INLINE_BODY_MAX: u64 = 1024 * 1024;

    async fn upload_single(&self, src: &Path, dst: &Url, metadata: &Metadata) -> anyhow::Result<()> {
        let size = std::fs::metadata(src).map(|m| m.len()).unwrap_or(u64::MAX);
        let body = if size <= Self::INLINE_BODY_MAX {
            ByteStream::from(tokio::fs::read(src).await?)
        } else {
            ByteStream::from_path(src).await?
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
        req.send().await?;
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
                // Best-effort abort so we don't leak an incomplete upload.
                let _ = self
                    .client
                    .abort_multipart_upload()
                    .bucket(&dst.bucket)
                    .key(&dst.path)
                    .upload_id(&upload_id)
                    .set_request_payer(self.request_payer())
                    .send()
                    .await;
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
            async move {
                let body = ByteStream::read_from()
                    .path(&src)
                    .offset(offset)
                    .length(Length::Exact(len))
                    .build()
                    .await?;
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
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .collect::<anyhow::Result<Vec<_>>>()?;
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
            let mut batch: Vec<Url> = Vec::with_capacity(DELETE_CHUNK_SIZE);
            while let Some(u) = urls.recv().await {
                batch.push(u);
                if batch.len() >= DELETE_CHUNK_SIZE {
                    this.delete_chunk(std::mem::take(&mut batch), &tx).await;
                }
            }
            if !batch.is_empty() {
                this.delete_chunk(batch, &tx).await;
            }
        });
        rx
    }

    async fn delete_chunk(&self, urls: Vec<Url>, tx: &mpsc::Sender<Object>) {
        if urls.is_empty() {
            return;
        }
        let bucket = urls[0].bucket.clone();

        if self.dry_run {
            for u in urls {
                let _ = tx.send(Object {
                    url: Some(u),
                    ..Default::default()
                });
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
                            if !src.matches(key) {
                                continue;
                            }
                            let mut newurl = src.clone();
                            newurl.path = key.to_string();
                            let typ = if key.ends_with('/') {
                                ObjectType::Dir
                            } else {
                                ObjectType::File
                            };
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
                    last_key = Some(key.to_string());
                    if !src.matches(key) {
                        continue;
                    }
                    let mut newurl = src.clone();
                    newurl.path = key.to_string();
                    let typ = if key.ends_with('/') {
                        ObjectType::Dir
                    } else {
                        ObjectType::File
                    };
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
                marker = page.next_marker().map(|s| s.to_string()).or(last_key);
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
                    let typ = if key.ends_with('/') { ObjectType::Dir } else { ObjectType::File };
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
                key_marker = page.next_key_marker().map(|s| s.to_string());
                version_marker = page.next_version_id_marker().map(|s| s.to_string());
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
                // Best-effort abort so we don't leak an incomplete upload.
                let _ = self
                    .client
                    .abort_multipart_upload()
                    .bucket(&dst.bucket)
                    .key(&dst.path)
                    .upload_id(&upload_id)
                    .set_request_payer(self.request_payer())
                    .send()
                    .await;
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
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .collect::<anyhow::Result<Vec<_>>>()?;
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
