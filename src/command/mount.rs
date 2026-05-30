//! `mount` — FUSE-based local mount of a remote S3 path (rclone-style).
//!
//! This file holds only the CLI surface (`MountArgs`); the FUSE filesystem,
//! VFS core, caches, chunked reader and write-back cache live under
//! `crate::mount`. Gated behind the `mount` Cargo feature.

use clap::Args;

use super::GlobalOpts;

#[derive(Args, Debug)]
pub struct MountArgs {
    /// Remote S3 URL to mount, e.g. `s3://bucket` or `s3://bucket/prefix`.
    pub source: String,

    /// Local directory to mount onto (must exist and be empty).
    pub mountpoint: String,

    /// Mount read-only; reject all writes at the FUSE layer.
    #[arg(long)]
    pub read_only: bool,

    /// Directory for the on-disk write-back cache. Defaults to
    /// `$XDG_CACHE_HOME/rs5cmd/mount` (or `~/.cache/rs5cmd/mount`).
    #[arg(long)]
    pub cache_dir: Option<String>,

    /// Read chunk size (bytes) for the chunked read-ahead reader.
    #[arg(long, default_value_t = 4 * 1024 * 1024)]
    pub vfs_read_chunk_size: u64,

    /// Per-open-file read-ahead buffer budget (bytes).
    #[arg(long, default_value_t = 16 * 1024 * 1024)]
    pub buffer_size: u64,

    /// How long a directory listing stays cached.
    #[arg(long, default_value = "5m", value_parser = humantime_secs)]
    pub dir_cache_time: std::time::Duration,

    /// How long file/dir attributes stay cached (also handed to the kernel).
    #[arg(long, default_value = "1s", value_parser = humantime_secs)]
    pub attr_timeout: std::time::Duration,

    /// Multipart part size in MiB for large uploads (min 5).
    #[arg(long, default_value_t = 8)]
    pub part_size: u64,

    /// Concurrent parts per object (chunked read prefetch / multipart upload).
    #[arg(long, default_value_t = 8)]
    pub concurrency: usize,

    /// Allow other users to access the mount (needs `user_allow_other` in
    /// /etc/fuse.conf for unprivileged mounts).
    #[arg(long)]
    pub allow_other: bool,

    /// Override the uid reported for all entries (defaults to the caller's).
    #[arg(long)]
    pub uid: Option<u32>,

    /// Override the gid reported for all entries (defaults to the caller's).
    #[arg(long)]
    pub gid: Option<u32>,
}

/// Parses a simple duration like `500ms`, `1s`, `5m`, `2h`. A bare number is
/// seconds. Kept self-contained (this module is mount-only) so the default
/// build pulls in no extra dependency.
fn humantime_secs(s: &str) -> Result<std::time::Duration, String> {
    let s = s.trim();
    if s.is_empty() {
        return Err("empty duration".into());
    }
    let split = s.find(|c: char| c.is_alphabetic()).unwrap_or(s.len());
    let (num, unit) = s.split_at(split);
    let value: f64 = num
        .trim()
        .parse()
        .map_err(|_| format!("invalid duration: {s}"))?;
    let secs = match unit {
        "" | "s" => value,
        "ms" => value / 1_000.0,
        "us" | "µs" => value / 1_000_000.0,
        "m" => value * 60.0,
        "h" => value * 3_600.0,
        other => return Err(format!("unknown duration unit: {other}")),
    };
    // `Duration::from_secs_f64` panics on negative / non-finite values, so
    // reject them here with a clean parse error instead.
    if !secs.is_finite() || secs < 0.0 {
        return Err(format!("invalid duration: {s}"));
    }
    Ok(std::time::Duration::from_secs_f64(secs))
}

pub async fn run(global: &GlobalOpts, args: MountArgs) -> anyhow::Result<()> {
    crate::mount::run(global, args).await
}
