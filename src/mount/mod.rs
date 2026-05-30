//! FUSE mount of a remote S3 path (rclone-style). Behind the `mount` feature.
//!
//! Layering: the [`fs`] module is a thin fuse3 adapter over the
//! binding-agnostic [`vfs::Vfs`] core, which in turn drives the reused
//! `storage::s3::S3` backend. The [`inode`] module holds the inode↔key map.
//! Later phases add a chunked read-ahead reader and a write-back cache; the
//! binding-agnostic core keeps a `fuser` shim possible for macOS.

mod fs;
mod inode;
mod reader;
mod vfs;
mod writer;

use std::sync::Arc;

use fuse3::raw::Session;
use fuse3::MountOptions;

use crate::command::mount::MountArgs;
use crate::command::GlobalOpts;
use crate::storage::s3::S3;
use crate::storage::url::Url;

use fs::S3Fuse;
use vfs::{Vfs, VfsConfig};

/// Entry point for `rs5cmd mount`. Mounts the filesystem and blocks until it is
/// unmounted (externally via `fusermount3 -u`, or by Ctrl-C).
pub async fn run(global: &GlobalOpts, args: MountArgs) -> anyhow::Result<()> {
    let uid = args.uid.unwrap_or_else(|| unsafe { libc::getuid() });
    let gid = args.gid.unwrap_or_else(|| unsafe { libc::getgid() });

    let mountpoint = std::path::Path::new(&args.mountpoint);
    if !mountpoint.is_dir() {
        anyhow::bail!("mountpoint {} is not a directory", args.mountpoint);
    }

    // Parse the source `s3://bucket[/prefix]`.
    let src = Url::parse(&args.source).map_err(|e| anyhow::anyhow!(e))?;
    if !src.is_remote() {
        anyhow::bail!("mount source must be an s3:// URL, got {}", args.source);
    }
    let bucket = src.bucket.clone();
    // Normalize the root prefix to end with '/' (empty means the whole bucket).
    let mut root_prefix = src.path.clone();
    if !root_prefix.is_empty() && !root_prefix.ends_with('/') {
        root_prefix.push('/');
    }

    // Build the S3 client from the global options, layering on the mount's
    // part-size / concurrency knobs (reused by the read path).
    let mut opts = global.storage_options();
    opts.part_size = args.part_size.max(5) * 1024 * 1024;
    opts.concurrency = args.concurrency.max(1);
    let s3 = S3::new(&src, &opts).await?;

    let cfg = VfsConfig {
        attr_ttl: args.attr_timeout,
        dir_ttl: args.dir_cache_time,
        uid,
        gid,
        chunk_size: args.vfs_read_chunk_size,
        buffer_size: args.buffer_size,
        concurrency: args.concurrency.max(1),
    };
    // Per-mount write-back cache directory for the write path.
    let cache_base = args
        .cache_dir
        .clone()
        .map(std::path::PathBuf::from)
        .unwrap_or_else(default_cache_dir);
    let cache_dir = cache_base.join(format!("mount-{}", std::process::id()));
    std::fs::create_dir_all(&cache_dir)?;

    let vfs = Arc::new(Vfs::new(s3, bucket, root_prefix, cache_dir.clone(), cfg));
    let fs = S3Fuse::new(vfs, args.attr_timeout, args.read_only);

    let mut mount_options = MountOptions::default();
    mount_options
        .fs_name("rs5cmd")
        .uid(uid)
        .gid(gid)
        .read_only(args.read_only)
        .allow_other(args.allow_other);

    tracing::info!(
        source = %args.source,
        mountpoint = %args.mountpoint,
        "mounting S3 filesystem"
    );

    let mount_handle = Session::new(mount_options)
        .mount_with_unprivileged(fs, mountpoint)
        .await?;

    tokio::select! {
        res = mount_handle => { res?; }
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("received Ctrl-C, unmounting");
        }
    }

    // Clean up the write-back cache directory.
    let _ = std::fs::remove_dir_all(&cache_dir);
    Ok(())
}

/// Default base directory for the write-back cache.
fn default_cache_dir() -> std::path::PathBuf {
    if let Ok(x) = std::env::var("XDG_CACHE_HOME") {
        if !x.is_empty() {
            return std::path::PathBuf::from(x).join("rs5cmd").join("mount");
        }
    }
    if let Ok(home) = std::env::var("HOME") {
        if !home.is_empty() {
            return std::path::PathBuf::from(home)
                .join(".cache")
                .join("rs5cmd")
                .join("mount");
        }
    }
    std::env::temp_dir().join("rs5cmd-mount")
}
