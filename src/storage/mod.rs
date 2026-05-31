//! Storage abstraction over local filesystem and S3.

pub mod fs;
pub mod s3;
pub mod url;

use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;
use serde::Serialize;
use tokio::sync::mpsc;

use self::url::Url;
use crate::ratelimit::RateLimiter;

/// Metadata error: a specified object was not found.
#[derive(Debug, thiserror::Error)]
#[error("given object {0} not found")]
pub struct ObjectNotFound(pub String);

/// Indicates there are no objects found from a given directory.
#[derive(Debug, thiserror::Error)]
#[error("no object found")]
pub struct NoObjectFound;

/// The kind of a filesystem object.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ObjectType {
    File,
    Dir,
    Symlink,
    #[default]
    Unknown,
}

impl ObjectType {
    pub fn is_dir(&self) -> bool {
        matches!(self, ObjectType::Dir)
    }
    pub fn is_symlink(&self) -> bool {
        matches!(self, ObjectType::Symlink)
    }
    pub fn is_regular(&self) -> bool {
        matches!(self, ObjectType::File)
    }
}

impl fmt::Display for ObjectType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            ObjectType::File => "file",
            ObjectType::Dir => "directory",
            ObjectType::Symlink => "symlink",
            ObjectType::Unknown => "",
        };
        write!(f, "{s}")
    }
}

impl Serialize for ObjectType {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_string())
    }
}

/// The storage class of a remote object.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StorageClass(pub String);

impl StorageClass {
    pub fn is_glacier(&self) -> bool {
        self.0 == "GLACIER"
    }
}

/// A generic storage item with its metadata.
#[derive(Debug, Default)]
pub struct Object {
    pub url: Option<Url>,
    pub etag: String,
    /// Last modification time as a unix timestamp (seconds), if known.
    pub mod_time: Option<std::time::SystemTime>,
    pub typ: ObjectType,
    pub size: i64,
    pub storage_class: StorageClass,
    /// True when this entry is an S3 delete marker (a versioned tombstone)
    /// rather than a real object. Defaults to false; only set by the version
    /// listing path. Such entries carry a `version_id` but no size/etag.
    pub is_delete_marker: bool,
    /// Per-object error, used when streaming results carry failures inline.
    pub err: Option<anyhow::Error>,
}

impl Object {
    pub fn with_error(err: anyhow::Error) -> Object {
        Object {
            err: Some(err),
            ..Default::default()
        }
    }
}

impl fmt::Display for Object {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.url {
            Some(u) => write!(f, "{u}"),
            None => write!(f, ""),
        }
    }
}

/// A storage container.
#[derive(Debug, Clone)]
pub struct Bucket {
    pub creation_date: Option<std::time::SystemTime>,
    pub name: String,
}

/// Metadata applied to objects on copy/put.
#[derive(Debug, Default, Clone)]
pub struct Metadata {
    pub acl: Option<String>,
    pub cache_control: Option<String>,
    pub expires: Option<String>,
    pub storage_class: Option<String>,
    pub content_type: Option<String>,
    pub content_encoding: Option<String>,
    pub content_disposition: Option<String>,
    pub encryption_method: Option<String>,
    pub encryption_key_id: Option<String>,
    pub user_defined: std::collections::HashMap<String, String>,
    /// COPY (default) or REPLACE.
    pub directive: Option<String>,
    /// Conditional write: when true, the destination write carries
    /// `If-None-Match: "*"`, so S3 fails with HTTP 412 (`PreconditionFailed`)
    /// if the destination object already exists. `cp` turns that 412 into an
    /// "object already exists, skipped" notice instead of a hard error (#752).
    pub if_none_match: bool,
}

/// Returned by S3 write operations when a conditional write (`If-None-Match:
/// "*"`) fails because the destination object already exists (HTTP 412). `cp`
/// downcasts an `anyhow::Error` to this type to render an "object already
/// exists, skipped" notice rather than treating it as a hard failure (#752).
#[derive(Debug, Clone, thiserror::Error)]
#[error("object already exists, skipped: {url}")]
pub struct PreconditionFailedError {
    pub url: String,
}

/// Configuration shared by storage backends.
#[derive(Debug, Clone)]
pub struct Options {
    pub max_retries: u32,
    pub endpoint: Option<String>,
    pub no_verify_ssl: bool,
    pub dry_run: bool,
    pub no_sign_request: bool,
    pub use_list_objects_v1: bool,
    pub request_payer: Option<String>,
    pub profile: Option<String>,
    pub region: Option<String>,
    /// Per-side region/endpoint overrides for two-sided operations (cp/mv/sync
    /// between two S3 locations). When set, the source side of a transfer uses
    /// `source_region`/`source_endpoint` and the destination side uses
    /// `destination_region`/`destination_endpoint`; each falls back to the
    /// shared `region`/`endpoint` (then the SDK defaults) when unset. This lets
    /// a single copy span two regions or two S3-compatible endpoints
    /// (upstream #858/#816/#514/#702/#700/#671). See [`Options::for_side`].
    pub source_region: Option<String>,
    pub destination_region: Option<String>,
    pub source_endpoint: Option<String>,
    pub destination_endpoint: Option<String>,
    /// Proxy URL (`socks5://`, `socks5h://`, `http://`, `https://`) for the
    /// default SDK transport. `None` falls back to the standard `ALL_PROXY` /
    /// `HTTPS_PROXY` / `HTTP_PROXY` environment variables.
    pub proxy: Option<String>,
    /// Force S3 addressing style: `Some("path")` or `Some("virtual")`. When
    /// `None`, path-style is used for custom endpoints (MinIO etc.) and the SDK
    /// default (virtual-host) for real AWS — matching prior behavior.
    pub addressing_style: Option<String>,
    /// Path to a PEM bundle of additional trusted CA certificates
    /// (`--ca-certs-file`, or s3cmd's `ca_certs_file`). When set, its certs are
    /// added to the TLS root store so private/self-signed CAs validate without
    /// disabling verification. Ignored when `no_verify_ssl` is set.
    pub ca_certs_file: Option<String>,
    /// Resolve S3 endpoints to their dual-stack (IPv4 + IPv6) variant so
    /// requests can travel over IPv6 (upstream #719). Applied via the SDK config
    /// builder's `use_dual_stack`. Ignored when a custom endpoint is set.
    pub use_dualstack_endpoint: bool,
    /// Resolve S3 endpoints to their FIPS-compliant variant. Applied via the SDK
    /// config builder's `use_fips`. Ignored when a custom endpoint is set.
    pub use_fips_endpoint: bool,
    /// Multipart part size in bytes. Objects larger than this are transferred
    /// in parallel parts; smaller ones use a single PUT/GET.
    pub part_size: u64,
    /// Number of parts transferred concurrently per object.
    pub concurrency: usize,
    /// Preserve file modification time across transfers: on upload the local
    /// mtime is stored as object metadata; on download it is restored onto the
    /// written file.
    pub preserve_timestamps: bool,
    /// Perform remote→remote copies by streaming through the client
    /// (download then upload) instead of a server-side `CopyObject`. Useful
    /// when server-side copy is unavailable or disallowed.
    pub client_copy: bool,
    /// On a local→remote `mv`, after removing each moved source file, also
    /// prune now-empty source directories walking up toward (but never past)
    /// the move source root. Non-empty/unremovable directories are skipped.
    pub remove_empty_dirs: bool,
    /// Aggregate upload bandwidth cap (`--limit-upload`), shared across all
    /// workers. `None` means no upload throttling.
    pub upload_limiter: Option<Arc<RateLimiter>>,
    /// Aggregate download bandwidth cap (`--limit-download`), shared across all
    /// workers. `None` means no download throttling.
    pub download_limiter: Option<Arc<RateLimiter>>,
}

/// Default multipart part size (bytes). Mirrors a common 8 MiB default and is
/// safely above S3's 5 MiB minimum part size.
pub const DEFAULT_PART_SIZE: u64 = 8 * 1024 * 1024;
/// Default per-object part concurrency.
pub const DEFAULT_CONCURRENCY: usize = 8;

impl Default for Options {
    fn default() -> Self {
        Options {
            max_retries: 0,
            endpoint: None,
            no_verify_ssl: false,
            dry_run: false,
            no_sign_request: false,
            use_list_objects_v1: false,
            request_payer: None,
            profile: None,
            region: None,
            source_region: None,
            destination_region: None,
            source_endpoint: None,
            destination_endpoint: None,
            proxy: None,
            addressing_style: None,
            ca_certs_file: None,
            use_dualstack_endpoint: false,
            use_fips_endpoint: false,
            part_size: DEFAULT_PART_SIZE,
            concurrency: DEFAULT_CONCURRENCY,
            preserve_timestamps: false,
            client_copy: false,
            remove_empty_dirs: false,
            upload_limiter: None,
            download_limiter: None,
        }
    }
}

/// Which side of a two-sided transfer (cp/mv/sync) a client is being built for,
/// selecting the per-side region/endpoint overrides (#858/#816/#514/#702/#700/#671).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Source,
    Destination,
}

impl Options {
    /// Returns the effective region for the given side: the per-side override if
    /// set, else the shared `region` fallback.
    pub fn region_for(&self, side: Side) -> Option<String> {
        let per_side = match side {
            Side::Source => &self.source_region,
            Side::Destination => &self.destination_region,
        };
        per_side
            .clone()
            .filter(|s| !s.is_empty())
            .or_else(|| self.region.clone())
    }

    /// Returns the effective endpoint for the given side: the per-side override
    /// if set, else the shared `endpoint` fallback.
    pub fn endpoint_for(&self, side: Side) -> Option<String> {
        let per_side = match side {
            Side::Source => &self.source_endpoint,
            Side::Destination => &self.destination_endpoint,
        };
        per_side
            .clone()
            .filter(|s| !s.is_empty())
            .or_else(|| self.endpoint.clone())
    }

    /// Clones these options with `region`/`endpoint` resolved to the given side's
    /// effective values, so [`s3::S3::new`] (which reads `region`/`endpoint`)
    /// builds a client anchored on that side. The per-side override fields are
    /// left intact but no longer consulted by `S3::new`.
    pub fn for_side(&self, side: Side) -> Options {
        Options {
            region: self.region_for(side),
            endpoint: self.endpoint_for(side),
            ..self.clone()
        }
    }

    /// True when the source and destination sides resolve to a different region
    /// or endpoint, so a single shared client / server-side `CopyObject` cannot
    /// serve both and a two-client download+upload copy is required. When all
    /// per-side overrides are unset this is always false (the fast single-client
    /// path is kept).
    pub fn sides_differ(&self) -> bool {
        self.region_for(Side::Source) != self.region_for(Side::Destination)
            || self.endpoint_for(Side::Source) != self.endpoint_for(Side::Destination)
    }
}

/// Object-metadata key (becomes `x-amz-meta-<key>`) used to carry the source
/// file's modification time when `--preserve-timestamps` is set. The value is
/// the mtime in `seconds.nanoseconds` since the Unix epoch.
pub const MTIME_METADATA_KEY: &str = "file-mtime";

/// Common interface for local filesystem and remote object storage. Listing
/// operations return a channel receiver mirroring the Go `<-chan *Object`
/// streaming model so results flow with backpressure.
#[async_trait]
pub trait Storage: Send + Sync {
    /// Returns the Object describing `src`, or `ObjectNotFound`.
    async fn stat(&self, src: &Url) -> anyhow::Result<Object>;

    /// Lists objects and prefixes under `src`.
    fn list(&self, src: &Url, follow_symlinks: bool) -> mpsc::Receiver<Object>;

    /// Deletes `src`.
    async fn delete(&self, src: &Url) -> anyhow::Result<()>;

    /// Copies `src` to `dst` (same storage type), optionally setting metadata.
    async fn copy(&self, src: &Url, dst: &Url, metadata: &Metadata) -> anyhow::Result<()>;
}

/// Constructs a storage client for the given URL.
pub async fn new_client(u: &Url, opts: &Options) -> anyhow::Result<Box<dyn Storage>> {
    if u.is_remote() {
        Ok(Box::new(s3::S3::new(u, opts).await?))
    } else {
        Ok(Box::new(fs::Filesystem::new(opts.dry_run)))
    }
}

#[cfg(test)]
mod dualstack_tests {
    use super::*;

    // The `--use-dualstack-endpoint` (#719) and `--use-fips-endpoint` flags
    // default off, and an `Options` literal can opt in to either. This covers
    // the storage-layer plumbing the flags feed into. NOTE: MinIO/custom
    // endpoints are used verbatim by the SDK, so real dual-stack (IPv6) DNS
    // resolution is NOT exercised by the test suite — only the wiring.
    #[test]
    fn options_default_dualstack_and_fips_off() {
        let o = Options::default();
        assert!(!o.use_dualstack_endpoint);
        assert!(!o.use_fips_endpoint);

        let o = Options {
            use_dualstack_endpoint: true,
            use_fips_endpoint: true,
            ..Default::default()
        };
        assert!(o.use_dualstack_endpoint);
        assert!(o.use_fips_endpoint);
    }

    // Per-side region/endpoint resolution (#858/#816/#514/#702/#700/#671).
    // With no overrides, both sides resolve to the shared region/endpoint and
    // `sides_differ()` is false (keeping the single-client fast path). A
    // per-side override is honored and makes the sides differ.
    #[test]
    fn options_per_side_defaults_to_shared() {
        let o = Options::default();
        assert!(o.source_region.is_none());
        assert!(o.destination_region.is_none());
        assert!(o.source_endpoint.is_none());
        assert!(o.destination_endpoint.is_none());

        let o = Options {
            region: Some("us-east-1".to_string()),
            endpoint: Some("http://minio:9000".to_string()),
            ..Default::default()
        };
        assert_eq!(o.region_for(Side::Source), Some("us-east-1".to_string()));
        assert_eq!(o.region_for(Side::Destination), Some("us-east-1".to_string()));
        assert_eq!(o.endpoint_for(Side::Source), o.endpoint_for(Side::Destination));
        assert!(!o.sides_differ(), "no overrides -> sides must not differ");
    }

    #[test]
    fn options_per_side_overrides_resolve_and_differ() {
        let o = Options {
            region: Some("us-east-1".to_string()),
            endpoint: Some("http://shared:9000".to_string()),
            destination_region: Some("eu-west-1".to_string()),
            destination_endpoint: Some("http://other:9000".to_string()),
            ..Default::default()
        };
        // Source falls back to the shared values; destination uses its overrides.
        assert_eq!(o.region_for(Side::Source), Some("us-east-1".to_string()));
        assert_eq!(o.region_for(Side::Destination), Some("eu-west-1".to_string()));
        assert_eq!(o.endpoint_for(Side::Source), Some("http://shared:9000".to_string()));
        assert_eq!(
            o.endpoint_for(Side::Destination),
            Some("http://other:9000".to_string())
        );
        assert!(o.sides_differ(), "differing region+endpoint -> sides differ");

        // `for_side` bakes the resolved values into region/endpoint so S3::new
        // (which only reads those) anchors on the right side.
        let dst_opts = o.for_side(Side::Destination);
        assert_eq!(dst_opts.region, Some("eu-west-1".to_string()));
        assert_eq!(dst_opts.endpoint, Some("http://other:9000".to_string()));
        let src_opts = o.for_side(Side::Source);
        assert_eq!(src_opts.region, Some("us-east-1".to_string()));
        assert_eq!(src_opts.endpoint, Some("http://shared:9000".to_string()));
    }

    #[test]
    fn options_per_side_only_region_differs() {
        // Only a destination region override (same/no endpoint) still makes the
        // sides differ, so the two-client copy path is selected.
        let o = Options {
            region: Some("us-east-1".to_string()),
            destination_region: Some("us-west-2".to_string()),
            ..Default::default()
        };
        assert!(o.sides_differ());
    }
}
