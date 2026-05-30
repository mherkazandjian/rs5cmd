//! `pipe` — stream stdin to a remote (s3://) object. Ported from s5cmd's
//! `command/pipe.go`.
//!
//! Reads stdin and uploads it to the destination object, honoring the
//! metadata flags (content-type, storage-class, acl, ...). The destination
//! must be a remote, non-bucket, non-prefix, non-wildcard object URL.
//!
//! Rather than buffering all of stdin in memory and issuing a single
//! `PutObject`, stdin is read in fixed-size parts. Small inputs (those that
//! fit in a single part) take a single `PutObject`. Larger inputs use a
//! concurrent multipart upload so that arbitrarily large streams do not
//! exhaust memory: at most `concurrency` parts of `part-size` bytes are held
//! in memory at once. Mirrors the `--concurrency`/`--part-size` flags of the
//! Go implementation.

use aws_sdk_s3::primitives::ByteStream;
use clap::Args;
use tokio::io::AsyncReadExt;

use super::GlobalOpts;
use crate::storage::s3::S3;
use crate::storage::url::Url;
use crate::storage::Metadata;

/// Minimum part size allowed by S3 for multipart uploads, in MiB. S3 requires
/// every part except the last to be at least 5 MiB.
const MIN_PART_SIZE_MIB: usize = 5;

/// Default part size, in MiB, matching the Go implementation's default.
const DEFAULT_PART_SIZE_MIB: usize = 8;

/// Default upload concurrency, matching the Go implementation's default.
const DEFAULT_CONCURRENCY: usize = 8;

/// Uploads stdin to a remote object. Added here because `pipe` streams from
/// stdin rather than a file (the existing `S3::upload` only accepts a path).
impl S3 {
    /// Uploads an in-memory body to a remote object with a single `PutObject`.
    pub async fn put_reader(
        &self,
        body: ByteStream,
        dst: &Url,
        metadata: &Metadata,
    ) -> anyhow::Result<()> {
        if self.dry_run {
            return Ok(());
        }

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
        if let Some(sse) = &metadata.encryption_method {
            if !sse.is_empty() {
                req = req.server_side_encryption(
                    aws_sdk_s3::types::ServerSideEncryption::from(sse.as_str()),
                );
            }
        }
        if let Some(key) = &metadata.encryption_key_id {
            if !key.is_empty() {
                req = req.ssekms_key_id(key);
            }
        }
        for (k, v) in &metadata.user_defined {
            req = req.metadata(k, v);
        }

        req.send().await?;
        Ok(())
    }

    /// Streams `reader` to `dst`, using a single `PutObject` for small inputs
    /// and a concurrent multipart upload for larger ones.
    ///
    /// stdin is read sequentially into `part_size`-byte buffers; the part
    /// uploads run concurrently, bounded by `concurrency`, so no more than
    /// `concurrency` parts are buffered in memory simultaneously. `part_size`
    /// is in bytes.
    pub async fn put_multipart_reader<R: tokio::io::AsyncRead + Unpin>(
        &self,
        reader: &mut R,
        dst: &Url,
        metadata: &Metadata,
        part_size: usize,
        concurrency: usize,
    ) -> anyhow::Result<()> {
        let concurrency = concurrency.max(1);
        let part_size = part_size.max(1);

        // Read the first part. If we hit end-of-input within a single part the
        // input is small enough for a single PutObject, so we avoid multipart
        // entirely (S3 multipart requires >= 2 parts).
        let first = read_part(reader, part_size).await?;
        if first.len() < part_size {
            return self
                .put_reader(ByteStream::from(first), dst, metadata)
                .await;
        }

        if self.dry_run {
            // Drain the rest of stdin so upstream pipes don't block, but skip
            // all network calls.
            let mut sink = tokio::io::sink();
            tokio::io::copy(reader, &mut sink).await?;
            return Ok(());
        }

        self.multipart_upload(reader, first, dst, metadata, part_size, concurrency)
            .await
    }

    /// Performs a concurrent multipart upload, feeding parts from `reader`.
    ///
    /// `first` is the already-read first part (exactly `part_size` bytes).
    async fn multipart_upload<R: tokio::io::AsyncRead + Unpin>(
        &self,
        reader: &mut R,
        first: Vec<u8>,
        dst: &Url,
        metadata: &Metadata,
        part_size: usize,
        concurrency: usize,
    ) -> anyhow::Result<()> {
        use aws_sdk_s3::types::CompletedMultipartUpload;

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
        if let Some(cc) = &metadata.cache_control {
            if !cc.is_empty() {
                create = create.cache_control(cc);
            }
        }
        if let Some(ce) = &metadata.content_encoding {
            if !ce.is_empty() {
                create = create.content_encoding(ce);
            }
        }
        if let Some(cd) = &metadata.content_disposition {
            if !cd.is_empty() {
                create = create.content_disposition(cd);
            }
        }
        if let Some(sse) = &metadata.encryption_method {
            if !sse.is_empty() {
                create = create.server_side_encryption(
                    aws_sdk_s3::types::ServerSideEncryption::from(sse.as_str()),
                );
            }
        }
        if let Some(key) = &metadata.encryption_key_id {
            if !key.is_empty() {
                create = create.ssekms_key_id(key);
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

        match self
            .upload_parts_from_reader(reader, first, dst, &upload_id, part_size, concurrency)
            .await
        {
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

    /// Reads remaining parts from `reader` (starting with the supplied `first`
    /// part), uploading them concurrently, and returns the completed parts
    /// sorted by part number.
    ///
    /// Reading is sequential (stdin is sequential), but uploads run
    /// concurrently bounded by a semaphore so at most `concurrency` parts are
    /// in flight — and thus buffered in memory — at once.
    async fn upload_parts_from_reader<R: tokio::io::AsyncRead + Unpin>(
        &self,
        reader: &mut R,
        first: Vec<u8>,
        dst: &Url,
        upload_id: &str,
        part_size: usize,
        concurrency: usize,
    ) -> anyhow::Result<Vec<aws_sdk_s3::types::CompletedPart>> {
        use aws_sdk_s3::types::CompletedPart;
        use tokio::task::JoinSet;

        let sem = std::sync::Arc::new(tokio::sync::Semaphore::new(concurrency));
        let mut join_set: JoinSet<anyhow::Result<CompletedPart>> = JoinSet::new();

        let mut part_number: i32 = 1;
        let mut next = Some(first);

        while let Some(buf) = next.take() {
            // Acquire a permit before spawning so the producer (this loop)
            // never reads more than `concurrency` parts ahead of completed
            // uploads, bounding memory use.
            let permit = sem
                .clone()
                .acquire_owned()
                .await
                .expect("semaphore not closed");
            let client = self.client.clone();
            let bucket = dst.bucket.clone();
            let key = dst.path.clone();
            let upload_id = upload_id.to_string();
            let pn = part_number;
            let rp = self.request_payer();
            join_set.spawn(async move {
                let _permit = permit;
                let resp = client
                    .upload_part()
                    .bucket(bucket)
                    .key(key)
                    .upload_id(upload_id)
                    .part_number(pn)
                    .body(ByteStream::from(buf))
                    .set_request_payer(rp)
                    .send()
                    .await?;
                anyhow::Ok(
                    CompletedPart::builder()
                        .part_number(pn)
                        .set_e_tag(resp.e_tag().map(|s| s.to_string()))
                        .build(),
                )
            });

            part_number += 1;

            // Read the next part sequentially. A short read signals EOF: if it
            // is empty there are no more parts, otherwise it is the final part.
            let buf = read_part(reader, part_size).await?;
            if !buf.is_empty() {
                next = Some(buf);
            }
        }

        let mut parts = Vec::new();
        while let Some(res) = join_set.join_next().await {
            parts.push(res??);
        }
        parts.sort_by_key(|p| p.part_number().unwrap_or(0));
        Ok(parts)
    }
}

/// Reads up to `cap` bytes from `reader` into a fresh buffer.
///
/// Returns the buffer once it holds `cap` bytes or end-of-input is reached. A
/// returned buffer shorter than `cap` indicates end-of-input was hit.
async fn read_part<R: tokio::io::AsyncRead + Unpin>(
    reader: &mut R,
    cap: usize,
) -> anyhow::Result<Vec<u8>> {
    let mut buf = vec![0u8; cap];
    let mut filled = 0;
    while filled < cap {
        let n = reader.read(&mut buf[filled..]).await?;
        if n == 0 {
            break;
        }
        filled += n;
    }
    buf.truncate(filled);
    Ok(buf)
}

#[derive(Args, Debug)]
pub struct PipeArgs {
    /// Destination object (s3:// URL).
    pub dst: String,

    /// Storage class for the destination object.
    #[arg(long)]
    pub storage_class: Option<String>,

    /// Canned ACL for the destination object.
    #[arg(long)]
    pub acl: Option<String>,

    /// Content-Type for the destination object.
    #[arg(long)]
    pub content_type: Option<String>,

    /// Content-Encoding for the destination object.
    #[arg(long)]
    pub content_encoding: Option<String>,

    /// Content-Disposition for the destination object.
    #[arg(long)]
    pub content_disposition: Option<String>,

    /// Cache-Control for the destination object.
    #[arg(long)]
    pub cache_control: Option<String>,

    /// Server-side encryption method for the destination object
    /// (e.g. "AES256" or "aws:kms").
    #[arg(long)]
    pub sse: Option<String>,

    /// KMS key id to use when the server-side encryption method is "aws:kms".
    #[arg(long)]
    pub sse_kms_key_id: Option<String>,

    /// Size of each part transferred to the remote server, in MiB. Values
    /// below the S3 minimum of 5 MiB are clamped up to 5 MiB.
    #[arg(long, short = 'p', default_value_t = DEFAULT_PART_SIZE_MIB)]
    pub part_size: usize,

    /// Number of concurrent parts transferred to the remote server.
    #[arg(long, short = 'c', default_value_t = DEFAULT_CONCURRENCY)]
    pub concurrency: usize,
}

impl PipeArgs {
    fn metadata(&self, dst: &Url) -> Metadata {
        let content_type = self
            .content_type
            .clone()
            .filter(|s| !s.is_empty())
            .or_else(|| Some(guess_content_type(dst)));

        Metadata {
            storage_class: self.storage_class.clone(),
            acl: self.acl.clone(),
            content_type,
            content_encoding: self.content_encoding.clone(),
            content_disposition: self.content_disposition.clone(),
            cache_control: self.cache_control.clone(),
            encryption_method: self.sse.clone(),
            encryption_key_id: self.sse_kms_key_id.clone(),
            ..Default::default()
        }
    }
}

/// Resolves the effective part size in bytes from a user-supplied value in MiB.
///
/// S3 rejects parts smaller than 5 MiB (except the final part), so any value
/// below that minimum is clamped up. A value of zero falls back to the default.
fn effective_part_size_bytes(part_size_mib: usize) -> usize {
    let mib = if part_size_mib == 0 {
        DEFAULT_PART_SIZE_MIB
    } else {
        part_size_mib.max(MIN_PART_SIZE_MIB)
    };
    mib * 1024 * 1024
}

/// Resolves the effective concurrency, ensuring it is at least 1.
fn effective_concurrency(concurrency: usize) -> usize {
    concurrency.max(1)
}

pub async fn run(global: &GlobalOpts, args: PipeArgs) -> anyhow::Result<()> {
    let opts = global.storage_options();
    let dst = Url::parse(&args.dst).map_err(|e| anyhow::anyhow!(e))?;

    if !dst.is_remote() {
        anyhow::bail!("destination must be a bucket");
    }
    if dst.is_bucket() || dst.is_prefix() {
        anyhow::bail!("target {dst:?} must be an object");
    }
    if dst.is_wildcard() {
        anyhow::bail!("target {:?} can not contain glob characters", args.dst);
    }

    let metadata = args.metadata(&dst);
    let part_size = effective_part_size_bytes(args.part_size);
    let concurrency = effective_concurrency(args.concurrency);

    let s3 = S3::new(&dst, &opts).await?;

    // Stream stdin in parts: a single PUT for small inputs, otherwise a
    // concurrent multipart upload that bounds memory regardless of input size.
    let mut stdin = tokio::io::stdin();
    s3.put_multipart_reader(&mut stdin, &dst, &metadata, part_size, concurrency)
        .await?;

    println!("pipe {dst}");
    Ok(())
}

/// Guesses a content type from the destination's file extension, falling back
/// to `application/octet-stream`. (No `mime` crate is available, so a small
/// built-in table covers the common cases.)
fn guess_content_type(dst: &Url) -> String {
    let abs = dst.absolute();
    let ext = abs.rsplit('.').next().filter(|e| !e.contains('/'));
    let ct = match ext.map(|e| e.to_ascii_lowercase()).as_deref() {
        Some("txt") => "text/plain",
        Some("html") | Some("htm") => "text/html",
        Some("css") => "text/css",
        Some("csv") => "text/csv",
        Some("json") => "application/json",
        Some("xml") => "application/xml",
        Some("js") => "application/javascript",
        Some("pdf") => "application/pdf",
        Some("zip") => "application/zip",
        Some("gz") => "application/gzip",
        Some("tar") => "application/x-tar",
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("svg") => "image/svg+xml",
        Some("webp") => "image/webp",
        Some("mp4") => "video/mp4",
        Some("mp3") => "audio/mpeg",
        _ => "application/octet-stream",
    };
    ct.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guesses_known_extension() {
        let u = Url::parse("s3://bucket/prefix/object.gz").unwrap();
        assert_eq!(guess_content_type(&u), "application/gzip");
    }

    #[test]
    fn guesses_html() {
        let u = Url::parse("s3://bucket/s5cmd.html").unwrap();
        assert_eq!(guess_content_type(&u), "text/html");
    }

    #[test]
    fn falls_back_to_octet_stream() {
        let u = Url::parse("s3://bucket/object").unwrap();
        assert_eq!(guess_content_type(&u), "application/octet-stream");
    }

    #[test]
    fn metadata_uses_explicit_content_type() {
        let u = Url::parse("s3://bucket/object.txt").unwrap();
        let args = PipeArgs {
            dst: "s3://bucket/object.txt".to_string(),
            storage_class: None,
            acl: None,
            content_type: Some("application/custom".to_string()),
            content_encoding: None,
            content_disposition: None,
            cache_control: None,
            sse: None,
            sse_kms_key_id: None,
            part_size: DEFAULT_PART_SIZE_MIB,
            concurrency: DEFAULT_CONCURRENCY,
        };
        assert_eq!(
            args.metadata(&u).content_type.as_deref(),
            Some("application/custom")
        );
    }

    #[test]
    fn metadata_guesses_content_type_when_absent() {
        let u = Url::parse("s3://bucket/object.json").unwrap();
        let args = PipeArgs {
            dst: "s3://bucket/object.json".to_string(),
            storage_class: None,
            acl: None,
            content_type: None,
            content_encoding: None,
            content_disposition: None,
            cache_control: None,
            sse: None,
            sse_kms_key_id: None,
            part_size: DEFAULT_PART_SIZE_MIB,
            concurrency: DEFAULT_CONCURRENCY,
        };
        assert_eq!(
            args.metadata(&u).content_type.as_deref(),
            Some("application/json")
        );
    }

    #[test]
    fn metadata_maps_sse_flags() {
        let u = Url::parse("s3://bucket/object.txt").unwrap();
        let args = PipeArgs {
            dst: "s3://bucket/object.txt".to_string(),
            storage_class: None,
            acl: None,
            content_type: None,
            content_encoding: None,
            content_disposition: None,
            cache_control: None,
            sse: Some("aws:kms".to_string()),
            sse_kms_key_id: Some("my-key-id".to_string()),
            part_size: DEFAULT_PART_SIZE_MIB,
            concurrency: DEFAULT_CONCURRENCY,
        };
        let md = args.metadata(&u);
        assert_eq!(md.encryption_method.as_deref(), Some("aws:kms"));
        assert_eq!(md.encryption_key_id.as_deref(), Some("my-key-id"));
    }

    #[test]
    fn part_size_clamped_to_minimum() {
        // Values below the S3 minimum of 5 MiB are clamped up.
        assert_eq!(effective_part_size_bytes(1), MIN_PART_SIZE_MIB * 1024 * 1024);
        assert_eq!(effective_part_size_bytes(5), 5 * 1024 * 1024);
        assert_eq!(effective_part_size_bytes(8), 8 * 1024 * 1024);
        // Zero falls back to the default.
        assert_eq!(
            effective_part_size_bytes(0),
            DEFAULT_PART_SIZE_MIB * 1024 * 1024
        );
    }

    #[test]
    fn concurrency_at_least_one() {
        assert_eq!(effective_concurrency(0), 1);
        assert_eq!(effective_concurrency(4), 4);
    }

    #[tokio::test]
    async fn read_part_short_read_signals_small_input() {
        // A reader with fewer bytes than the cap yields a short buffer, which
        // is how put_multipart_reader decides to do a single PUT.
        let mut cursor = std::io::Cursor::new(b"hello".to_vec());
        let buf = read_part(&mut cursor, 8).await.unwrap();
        assert_eq!(buf, b"hello");
        assert!(buf.len() < 8);
    }

    #[tokio::test]
    async fn read_part_fills_to_cap_then_drains() {
        let mut cursor = std::io::Cursor::new(vec![7u8; 20]);
        // Full parts are exactly `cap` bytes.
        assert_eq!(read_part(&mut cursor, 8).await.unwrap().len(), 8);
        assert_eq!(read_part(&mut cursor, 8).await.unwrap().len(), 8);
        // Final short part.
        assert_eq!(read_part(&mut cursor, 8).await.unwrap().len(), 4);
        // Clean EOF yields an empty buffer.
        assert!(read_part(&mut cursor, 8).await.unwrap().is_empty());
    }
}
