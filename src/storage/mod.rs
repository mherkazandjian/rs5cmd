//! Storage abstraction over local filesystem and S3.

pub mod fs;
pub mod s3;
pub mod url;

use std::fmt;

use async_trait::async_trait;
use serde::Serialize;
use tokio::sync::mpsc;

use self::url::Url;

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
    /// Proxy URL (`socks5://`, `socks5h://`, `http://`, `https://`) for the
    /// default SDK transport. `None` falls back to the standard `ALL_PROXY` /
    /// `HTTPS_PROXY` / `HTTP_PROXY` environment variables.
    pub proxy: Option<String>,
    /// Force S3 addressing style: `Some("path")` or `Some("virtual")`. When
    /// `None`, path-style is used for custom endpoints (MinIO etc.) and the SDK
    /// default (virtual-host) for real AWS — matching prior behavior.
    pub addressing_style: Option<String>,
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
            proxy: None,
            addressing_style: None,
            use_dualstack_endpoint: false,
            use_fips_endpoint: false,
            part_size: DEFAULT_PART_SIZE,
            concurrency: DEFAULT_CONCURRENCY,
            preserve_timestamps: false,
            client_copy: false,
            remove_empty_dirs: false,
        }
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
}
