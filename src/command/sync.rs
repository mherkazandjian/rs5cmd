//! `sync` — synchronize a source (local or S3) to a destination (local or S3).
//!
//! Ported from s5cmd's `command/sync.go` and `command/sync_strategy.go`.
//!
//! The algorithm mirrors the Go implementation:
//!   1. Build the set of source objects, keyed by their relative path.
//!   2. List the destination (recursively) and build the set of dest objects,
//!      keyed by their relative path.
//!   3. Partition into three groups:
//!        - only in source            -> copy
//!        - in both (common)          -> copy iff the sync strategy says so
//!        - only in destination       -> delete iff `--delete`
//!   4. Execute copies through the same direction logic as `cp`, and deletes
//!      through the storage `delete`, using a concurrency-limited worker pool.

// `sync_strategy.rs` is a sibling file. Because we may not edit `command/mod.rs`
// to declare it, we attach it here via a `#[path]` module.
#[path = "sync_strategy.rs"]
mod sync_strategy;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use clap::Args;

use self::sync_strategy::SyncStrategy;
use super::filters::Filters;
use super::GlobalOpts;
use crate::storage::s3::S3;
use crate::storage::url::Url;
use crate::storage::{new_client, Metadata, Object, Options};

#[derive(Args, Debug)]
pub struct SyncArgs {
    /// Source (local path or s3:// URL), may contain wildcards or be a dir/prefix.
    pub src: String,
    /// Destination (local path or s3:// URL).
    pub dst: String,

    /// Delete objects in the destination that are not present in the source.
    #[arg(long)]
    pub delete: bool,

    /// Use object size as the only criterion when deciding to sync.
    #[arg(long)]
    pub size_only: bool,

    /// Compare content checksums (S3 ETag / MD5) instead of size+modtime. For a
    /// local source the file's MD5 is computed; objects whose ETag is a
    /// multipart composite (contains `-`) can't be compared and are re-copied.
    #[arg(long)]
    pub checksum: bool,

    /// (Accepted for compatibility) compare exact timestamps. The default
    /// strategy already compares modification times, so this is a no-op here.
    #[arg(long)]
    pub exact_timestamps: bool,

    /// Follow symbolic links when walking local directories.
    #[arg(long, default_value_t = true)]
    pub follow_symlinks: bool,

    /// Exclude objects whose relative path matches the given glob (repeatable).
    #[arg(long)]
    pub exclude: Vec<String>,

    /// Only include objects whose relative path matches the given glob (repeatable).
    #[arg(long)]
    pub include: Vec<String>,

    /// Read additional `--exclude` globs from a file (one per line; blank lines
    /// and `#` comments ignored). Repeatable.
    #[arg(long)]
    pub exclude_from: Vec<String>,

    /// Read additional `--include` globs from a file (one per line; blank lines
    /// and `#` comments ignored). Repeatable.
    #[arg(long)]
    pub include_from: Vec<String>,

    /// Storage class for copied destination objects.
    #[arg(long)]
    pub storage_class: Option<String>,

    /// Canned ACL for copied destination objects.
    #[arg(long)]
    pub acl: Option<String>,

    /// Content-Type for copied destination objects.
    #[arg(long)]
    pub content_type: Option<String>,

    /// Stop the sync as soon as any copy or delete fails. When set, no further
    /// work is spawned after the first failure, in-flight tasks are drained, and
    /// the run returns an error immediately (a failed copy phase will not proceed
    /// to deletes). When unset (default), every operation is attempted and the
    /// run fails at the end if any operation failed.
    #[arg(long)]
    pub exit_on_error: bool,

    /// Transfer GLACIER / DEEP_ARCHIVE objects instead of silently skipping
    /// them. By default sync skips glacier-tier source objects (they cannot be
    /// read until restored); with this flag they are queued for transfer like
    /// any other object, matching `cp`'s `--force-glacier-transfer` (#812).
    #[arg(long)]
    pub force_glacier_transfer: bool,

    /// Safety cap on `--delete`: abort the whole sync (before copying or
    /// deleting anything) if more than N destination objects would be deleted.
    /// Guards against a misconfigured source silently wiping the destination
    /// (rsync's `--max-delete`). Ignored unless `--delete` is also set.
    #[arg(long)]
    pub max_delete: Option<usize>,

    /// Preserve file modification time across transfers (store local mtime as
    /// object metadata on upload; restore it on download).
    #[arg(long)]
    pub preserve_timestamps: bool,
}

/// Returns true if an error is a fatal AWS error that should abort the whole
/// sync immediately rather than being aggregated. We treat `AccessDenied` and
/// `NoSuchBucket` (case-insensitive, matched against the full Display chain) as
/// fatal: when one transfer hits these, every other transfer to the same target
/// will fail identically, so there is no value in continuing.
fn is_fatal(e: &anyhow::Error) -> bool {
    let msg = format!("{e:#}").to_ascii_lowercase();
    msg.contains("accessdenied") || msg.contains("nosuchbucket")
}

/// Whether a source object should be skipped because it is on a glacier tier.
/// Sync skips glacier-tier objects by default (they cannot be read until
/// restored), but `--force-glacier-transfer` overrides that so they are
/// transferred like any other object (#812). When the flag is set, nothing is
/// skipped on storage-class grounds.
fn skip_glacier(sc: &crate::storage::StorageClass, force_glacier_transfer: bool) -> bool {
    !force_glacier_transfer && sc.is_glacier()
}

impl SyncArgs {
    fn metadata(&self) -> Metadata {
        Metadata {
            storage_class: self.storage_class.clone(),
            acl: self.acl.clone(),
            content_type: self.content_type.clone(),
            ..Default::default()
        }
    }
}

pub async fn run(global: &GlobalOpts, args: SyncArgs) -> anyhow::Result<()> {
    let mut opts = global.storage_options();
    opts.preserve_timestamps = args.preserve_timestamps;
    let src = Url::parse(&args.src).map_err(|e| anyhow::anyhow!(e))?;
    let dst = Url::parse(&args.dst).map_err(|e| anyhow::anyhow!(e))?;
    let metadata = args.metadata();
    let strategy = SyncStrategy::new(args.size_only, args.checksum);

    // Compile include/exclude filters into regexes once. Inline patterns are
    // combined with any read from `--include-from`/`--exclude-from` files.
    let includes = super::filters::patterns_with_files(&args.include, &args.include_from)?;
    let excludes = super::filters::patterns_with_files(&args.exclude, &args.exclude_from)?;
    let filters = Filters::new(&includes, &excludes)?;

    // Per-side options for listing: each side lists against its own resolved
    // region/endpoint (#858/#816/#514/#702/#700/#671). With no per-side flags
    // these are identical to `opts`, so behavior is unchanged.
    let src_side_opts = opts.for_side(crate::storage::Side::Source);
    let dst_side_opts = opts.for_side(crate::storage::Side::Destination);

    // Determine whether the source expands to multiple objects ("batch"), which
    // governs how relative keys are derived (mirrors Go's `isBatch`).
    let is_batch = is_source_batch(&src, &src_side_opts).await?;

    // Build the keyed source and destination maps.
    let source_objects =
        collect_source_objects(
            &src,
            &src_side_opts,
            args.follow_symlinks,
            is_batch,
            &filters,
            args.checksum,
            args.force_glacier_transfer,
        )
        .await?;
    let dest_objects = collect_dest_objects(&dst, &dst_side_opts, &filters).await?;

    // Partition into copy / common / delete groups.
    let mut to_copy: Vec<(Url, Url)> = Vec::new();
    let mut to_delete: Vec<Url> = Vec::new();

    let mut dest_objects = dest_objects;
    for (key, src_obj) in &source_objects {
        let Some(src_url) = src_obj.url.clone() else {
            continue;
        };
        match dest_objects.remove(key) {
            // Common object: copy only if the strategy says it changed.
            Some(dst_obj) => {
                if strategy.should_sync(src_obj, &dst_obj) {
                    let dst_url = generate_destination_url(&src_url, &dst, is_batch);
                    to_copy.push((src_url, dst_url));
                }
            }
            // Only in source: always copy.
            None => {
                let dst_url = generate_destination_url(&src_url, &dst, is_batch);
                to_copy.push((src_url, dst_url));
            }
        }
    }

    // Whatever is left in `dest_objects` is only in the destination.
    if args.delete {
        for (_, dst_obj) in dest_objects {
            if let Some(u) = dst_obj.url {
                to_delete.push(u);
            }
        }
    }

    // `--max-delete` safety cap: if the delete set is larger than the allowed
    // maximum, abort *before* performing any copy or delete. A source that
    // expanded to far fewer objects than expected (wrong path, failed mount,
    // etc.) would otherwise delete the bulk of the destination; failing fast
    // here makes that mistake recoverable rather than destructive.
    if let Some(max) = args.max_delete {
        if to_delete.len() > max {
            anyhow::bail!(
                "aborting sync: --delete would remove {} objects, exceeding --max-delete {} \
                 (nothing was copied or deleted)",
                to_delete.len(),
                max
            );
        }
    }

    // Build the S3 client(s); share across all transfers/deletes. Per-side
    // region/endpoint support (#858/#816/#514/#702/#700/#671): when the source
    // and destination resolve to different regions/endpoints, build TWO clients
    // (one per side) so an s3->s3 sync bridges them via a download+upload copy.
    // Otherwise keep the single-client fast path (always so without per-side
    // flags). The source client serves listing/reads/deletes; the destination
    // client serves uploads and destination deletes.
    let sides_differ = opts.sides_differ();
    let (s3, s3_dst): (Option<Arc<S3>>, Option<Arc<S3>>) =
        if src.is_remote() || dst.is_remote() {
            if sides_differ {
                let src_anchor = if src.is_remote() { &src } else { &dst };
                let src_opts = opts.for_side(crate::storage::Side::Source);
                let dst_opts = opts.for_side(crate::storage::Side::Destination);
                let src_client = Arc::new(S3::new(src_anchor, &src_opts).await?);
                let dst_client = Arc::new(S3::new(&dst, &dst_opts).await?);
                (Some(src_client), Some(dst_client))
            } else {
                let anchor = if src.is_remote() { &src } else { &dst };
                (Some(Arc::new(S3::new(anchor, &opts).await?)), None)
            }
        } else {
            (None, None)
        };

    let opts = Arc::new(opts);
    let metadata = Arc::new(metadata);
    let workers = global.numworkers.max(1);
    let mut had_error = false;
    // Counts of successful operations across both phases, used for the final
    // run summary. Under `--dry-run` these reflect the operations that *would*
    // have been performed (the storage layer no-ops but still reports success).
    let mut copied: u64 = 0;
    let mut deleted: u64 = 0;
    let exit_on_error = args.exit_on_error;

    // Execute copies via tokio::spawn (per-request CPU spreads across all cores),
    // with the in-flight JoinSet bounded to `workers` to cap running tasks and
    // buffered completed results.
    // Count-of-copies progress bar (no-op in JSON mode / non-TTY stderr).
    let pb = crate::progress::Progress::new(to_copy.len() as u64, "sync");
    let mut set = tokio::task::JoinSet::new();
    // Set once a fatal AWS error (AccessDenied / NoSuchBucket) is observed. A
    // fatal error triggers the same fast-fail path as `--exit-on-error`
    // regardless of the flag: stop spawning, drain in-flight tasks, then bail.
    let mut fatal_seen = false;
    for (s, d) in to_copy.into_iter() {
        while set.len() >= workers {
            let (s, d, r) = set.join_next().await.unwrap().expect("sync copy task panicked");
            fatal_seen |= report("cp", &s, &d, r, &mut had_error, &mut copied);
            pb.inc(1);
            // With --exit-on-error (or on any fatal error), stop pulling new
            // work the moment a copy fails. We `break` out of the spawn loop
            // (without spawning the remaining copies); the in-flight tasks are
            // drained below.
            if (exit_on_error && had_error) || fatal_seen {
                break;
            }
        }
        // Don't spawn further work once a failure has been seen in exit-on-error
        // mode, or once any fatal error has been seen (covers failures observed
        // during draining).
        if (exit_on_error && had_error) || fatal_seen {
            break;
        }
        let opts = Arc::clone(&opts);
        let metadata = Arc::clone(&metadata);
        let s3 = s3.clone();
        let s3_dst = s3_dst.clone();
        set.spawn(async move {
            let r = copy_one(&s, &d, s3.as_deref(), s3_dst.as_deref(), &opts, &metadata).await;
            (s, d, r)
        });
    }
    // Drain all in-flight copy tasks so none are leaked (dropping the JoinSet
    // would abort them; we instead let them finish and report each result).
    while let Some(joined) = set.join_next().await {
        let (s, d, r) = joined.expect("sync copy task panicked");
        fatal_seen |= report("cp", &s, &d, r, &mut had_error, &mut copied);
        pb.inc(1);
    }
    pb.finish();

    // A fatal error aborts the whole sync immediately, with a clear message.
    if fatal_seen {
        anyhow::bail!("aborting sync: fatal error (access denied or missing bucket); see errors above");
    }

    // In exit-on-error mode a failed copy phase must not proceed to deletes:
    // bail before the delete phase starts.
    if exit_on_error && had_error {
        anyhow::bail!("one or more sync operations failed");
    }

    // Execute deletes the same way, with their own count progress bar.
    let dpb = crate::progress::Progress::new(to_delete.len() as u64, "sync-rm");
    let mut dset = tokio::task::JoinSet::new();
    for u in to_delete.into_iter() {
        while dset.len() >= workers {
            let (u, r) = dset.join_next().await.unwrap().expect("sync delete task panicked");
            fatal_seen |= report_rm(&u, r, &mut had_error, &mut deleted);
            dpb.inc(1);
            // Same fast-fail as the copy phase: stop on --exit-on-error or on
            // any fatal error, draining the remaining in-flight deletes.
            if (exit_on_error && had_error) || fatal_seen {
                break;
            }
        }
        if (exit_on_error && had_error) || fatal_seen {
            break;
        }
        let opts = Arc::clone(&opts);
        // Sync deletes remove DESTINATION objects, so use the destination-side
        // client when per-side clients exist; otherwise the shared client.
        let s3 = s3_dst.clone().or_else(|| s3.clone());
        dset.spawn(async move {
            let r = delete_one(&u, s3.as_deref(), &opts).await;
            (u, r)
        });
    }
    // Drain all in-flight delete tasks so none are leaked.
    while let Some(joined) = dset.join_next().await {
        let (u, r) = joined.expect("sync delete task panicked");
        fatal_seen |= report_rm(&u, r, &mut had_error, &mut deleted);
        dpb.inc(1);
    }
    dpb.finish();

    // A fatal error during the delete phase aborts immediately as well.
    if fatal_seen {
        anyhow::bail!("aborting sync: fatal error (access denied or missing bucket); see errors above");
    }

    // One-line run summary of successful operations, emitted to stderr so it
    // never interleaves with the per-object result lines on stdout. Suppressed
    // in JSON mode to keep machine-readable output limited to the per-op objects.
    // When the source and destination already matched, say so explicitly rather
    // than printing a silent "0 objects" line (upstream s5cmd #796).
    if !crate::output::is_json() {
        if copied == 0 && deleted == 0 {
            eprintln!("# nothing to sync; source and destination are already in sync");
        } else {
            eprintln!("# synced {copied} objects, deleted {deleted}");
        }
    }

    if had_error {
        anyhow::bail!("one or more sync operations failed");
    }
    Ok(())
}

/// Reports a copy result, bumping `had_error`/`ok` accordingly. `ok` counts
/// successful copies for the final run summary (a dry-run "success" is the
/// operation the storage layer would have performed). Returns true if the
/// result was a fatal AWS error (see `is_fatal`), so the caller can fast-fail.
fn report(
    op: &str,
    s: &Url,
    d: &Url,
    r: anyhow::Result<()>,
    had_error: &mut bool,
    ok: &mut u64,
) -> bool {
    match r {
        Ok(()) => {
            *ok += 1;
            crate::output::op_success(op, &s.to_string(), Some(&d.to_string()));
            false
        }
        Err(e) => {
            *had_error = true;
            let fatal = is_fatal(&e);
            crate::output::op_error(op, &s.to_string(), Some(&d.to_string()), &crate::error::format_error(&e));
            fatal
        }
    }
}

/// Reports a delete result, bumping `had_error`/`ok` accordingly. `ok` counts
/// successful deletes for the final run summary. Returns true if the result was
/// a fatal AWS error (see `is_fatal`), so the caller can fast-fail.
fn report_rm(u: &Url, r: anyhow::Result<()>, had_error: &mut bool, ok: &mut u64) -> bool {
    match r {
        Ok(()) => {
            *ok += 1;
            crate::output::op_success("rm", &u.to_string(), None);
            false
        }
        Err(e) => {
            *had_error = true;
            let fatal = is_fatal(&e);
            crate::output::op_error("rm", &u.to_string(), None, &crate::error::format_error(&e));
            fatal
        }
    }
}

/// Determines whether the source expands to multiple objects. Mirrors the Go
/// `isBatch` computation: wildcards and remote prefixes/buckets are batch; a
/// local directory is batch; a single file is not.
async fn is_source_batch(src: &Url, opts: &Options) -> anyhow::Result<bool> {
    if src.is_wildcard() {
        return Ok(true);
    }
    if src.is_remote() {
        return Ok(src.is_bucket() || src.is_prefix());
    }
    // Local: a directory expands.
    let client = new_client(src, opts).await?;
    match client.stat(src).await {
        Ok(obj) => Ok(obj.typ.is_dir()),
        Err(_) => Ok(false),
    }
}

/// Lists the source and returns its objects keyed by relative path. For a
/// non-batch source the key is the object's base name (matching Go).
async fn collect_source_objects(
    src: &Url,
    opts: &Options,
    follow_symlinks: bool,
    is_batch: bool,
    filters: &Filters,
    checksum: bool,
    force_glacier_transfer: bool,
) -> anyhow::Result<HashMap<String, Object>> {
    let client = new_client(src, opts).await?;
    let mut map: HashMap<String, Object> = HashMap::new();

    if !is_batch {
        // Single object: stat it directly.
        let obj = client.stat(src).await?;
        if !obj.typ.is_dir() {
            let key = src.base();
            if !filters.should_skip(&key) {
                let mut obj = obj;
                if obj.url.is_none() {
                    obj.url = Some(src.clone());
                }
                // For checksum mode, a local source has no ETag from stat; fill
                // it with the file's MD5 so the strategy can compare.
                if checksum && !src.is_remote() {
                    if let Some(h) = local_md5_hex(&src.absolute()) {
                        obj.etag = h;
                    }
                }
                map.insert(key, obj);
            }
        }
        return Ok(map);
    }

    let mut rx = client.list(src, follow_symlinks);
    while let Some(obj) = rx.recv().await {
        let mut obj = obj;
        if let Some(err) = obj.err {
            // A fatal listing error (AccessDenied / NoSuchBucket) aborts the
            // sync; annotate it with the source for a clearer message.
            if is_fatal(&err) {
                return Err(anyhow::anyhow!("cannot list source {src}: {err:#}"));
            }
            return Err(err);
        }
        if obj.typ.is_dir() {
            continue;
        }
        if skip_glacier(&obj.storage_class, force_glacier_transfer) {
            continue;
        }
        let Some(obj_url) = obj.url.clone() else { continue };
        let key = to_slash(&obj_url.relative());
        if filters.should_skip(&key) {
            continue;
        }
        // For checksum mode, compute the MD5 of each local source file (remote
        // sources already carry an ETag from the listing).
        if checksum && !obj_url.is_remote() {
            if let Some(h) = local_md5_hex(&obj_url.absolute()) {
                obj.etag = h;
            }
        }
        map.insert(key, obj);
    }
    Ok(map)
}

/// Computes the lowercase hex MD5 of a local file, streaming it in chunks so
/// large files don't have to be held in memory. Returns `None` on any IO error
/// (the caller then treats the checksum as missing and re-copies to be safe).
fn local_md5_hex(path: &str) -> Option<String> {
    use md5::{Digest, Md5};
    use std::io::Read;

    let mut file = std::fs::File::open(path).ok()?;
    let mut hasher = Md5::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf).ok()?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Some(format!("{:x}", hasher.finalize()))
}

/// Lists the destination recursively and returns its objects keyed by relative
/// path. Mirrors Go's approach of appending `/*` to force a recursive listing.
async fn collect_dest_objects(
    dst: &Url,
    opts: &Options,
    filters: &Filters,
) -> anyhow::Result<HashMap<String, Object>> {
    // Build the recursive listing URL: dst (with a trailing slash) + "*".
    let dst_abs = dst.absolute();
    let listing_path = if dst_abs.ends_with('/') {
        format!("{dst_abs}*")
    } else {
        format!("{dst_abs}/*")
    };

    let listing_url = match Url::parse(&listing_path) {
        Ok(u) => u,
        // If the destination cannot be turned into a listing URL (e.g. it does
        // not exist yet), treat it as empty.
        Err(_) => return Ok(HashMap::new()),
    };

    let client = match new_client(&listing_url, opts).await {
        Ok(c) => c,
        Err(_) => return Ok(HashMap::new()),
    };

    let mut map: HashMap<String, Object> = HashMap::new();
    let mut rx = client.list(&listing_url, false);
    while let Some(obj) = rx.recv().await {
        if let Some(err) = &obj.err {
            // A non-existent destination simply has no objects to compare.
            let msg = err.to_string();
            if msg.contains("no object found") || msg.contains("not found") {
                continue;
            }
            // Propagate other listing errors. Fatal AWS errors (AccessDenied /
            // NoSuchBucket) get a clearer message naming the destination.
            let lower = msg.to_ascii_lowercase();
            if lower.contains("accessdenied") || lower.contains("nosuchbucket") {
                return Err(anyhow::anyhow!("cannot list destination {dst}: {msg}"));
            }
            return Err(anyhow::anyhow!(msg));
        }
        if obj.typ.is_dir() {
            continue;
        }
        let Some(obj_url) = &obj.url else { continue };
        let key = to_slash(&obj_url.relative());
        if filters.should_skip(&key) {
            continue;
        }
        map.insert(key, obj);
    }
    Ok(map)
}

/// Generates the destination URL for a given source object, as if it lived in
/// the destination. Mirrors Go's `generateDestinationURL`.
fn generate_destination_url(src_url: &Url, dst: &Url, is_batch: bool) -> Url {
    let objname = if is_batch {
        src_url.relative()
    } else {
        src_url.base()
    };

    if dst.is_remote() {
        if dst.is_prefix() || dst.is_bucket() {
            return dst.join(&objname);
        }
        return dst.clone();
    }
    dst.join(&objname)
}

/// Copies a single source object to a single destination, choosing the transfer
/// direction from the remote/local kinds (duplicated from `cp.rs`, since that
/// file must not be edited).
async fn copy_one(
    src: &Url,
    dst: &Url,
    s3: Option<&S3>,
    s3_dst: Option<&S3>,
    opts: &Options,
    metadata: &Metadata,
) -> anyhow::Result<()> {
    match (src.is_remote(), dst.is_remote()) {
        // remote -> remote: server-side copy, or a cross-client download+upload
        // when the two sides resolve to different regions/endpoints
        // (#858/#816/#514/#702/#700/#671).
        (true, true) => {
            let s3 = s3.expect("remote copy requires S3 client");
            if let Some(dst_s3) = s3_dst {
                s3.client_copy_to(dst_s3, src, dst, metadata).await?;
            } else {
                s3.copy(src, dst, metadata).await?;
            }
        }
        // remote -> local: download (source-side client).
        (true, false) => {
            s3.expect("download requires S3 client")
                .download(src, &PathBuf::from(dst.absolute()))
                .await?;
        }
        // local -> remote: upload (destination-side client when present).
        (false, true) => {
            s3_dst
                .or(s3)
                .expect("upload requires S3 client")
                .upload(&PathBuf::from(src.absolute()), dst, metadata)
                .await?;
        }
        // local -> local: filesystem copy.
        (false, false) => {
            let fs = new_client(src, opts).await?;
            fs.copy(src, dst, metadata).await?;
        }
    }
    Ok(())
}

/// Deletes a single object (local or remote), using the shared S3 client when remote.
async fn delete_one(u: &Url, s3: Option<&S3>, opts: &Options) -> anyhow::Result<()> {
    if u.is_remote() {
        s3.expect("remote delete requires S3 client").delete(u).await
    } else {
        new_client(u, opts).await?.delete(u).await
    }
}

/// Converts OS path separators to forward slashes for stable map keys, matching
/// Go's `filepath.ToSlash`.
fn to_slash(s: &str) -> String {
    if std::path::MAIN_SEPARATOR == '/' {
        s.to_string()
    } else {
        s.replace(std::path::MAIN_SEPARATOR, "/")
    }
}

// Bring the Storage trait into scope so trait methods can be called on the
// concrete `S3` type within `copy_one`.
use crate::storage::Storage as _;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dest_url_batch_remote_prefix() {
        let src = Url::parse("/tmp/data/sub/a.txt").unwrap();
        let mut src = src;
        // Simulate a relative path as produced by listing a directory source.
        let base = Url::parse("/tmp/data").unwrap();
        src.set_relative(&base);
        let dst = Url::parse("s3://bucket/prefix/").unwrap();
        let out = generate_destination_url(&src, &dst, true);
        assert_eq!(out.absolute(), "s3://bucket/prefix/data/sub/a.txt");
    }

    #[test]
    fn dest_url_non_batch_uses_base() {
        let src = Url::parse("/tmp/data/a.txt").unwrap();
        let dst = Url::parse("s3://bucket/prefix/").unwrap();
        let out = generate_destination_url(&src, &dst, false);
        assert_eq!(out.absolute(), "s3://bucket/prefix/a.txt");
    }

    #[test]
    fn dest_url_remote_exact_object_clones() {
        let src = Url::parse("s3://b/x/a.txt").unwrap();
        let dst = Url::parse("s3://bucket/dir/file.txt").unwrap();
        // dst is neither prefix nor bucket -> exact destination clone.
        let out = generate_destination_url(&src, &dst, false);
        assert_eq!(out.absolute(), "s3://bucket/dir/file.txt");
    }

    #[test]
    fn is_fatal_matches_known_aws_errors() {
        // Matched case-insensitively against the full Display chain.
        assert!(is_fatal(&anyhow::anyhow!("AccessDenied: forbidden")));
        assert!(is_fatal(&anyhow::anyhow!("accessdenied")));
        assert!(is_fatal(&anyhow::anyhow!("NoSuchBucket: missing")));
        assert!(is_fatal(&anyhow::anyhow!("nosuchbucket")));
        // Fatal cause surfaced through a wrapping context.
        let wrapped = anyhow::anyhow!("AccessDenied").context("uploading object");
        assert!(is_fatal(&wrapped));

        // Unrelated errors are not fatal.
        assert!(!is_fatal(&anyhow::anyhow!("connection reset")));
        assert!(!is_fatal(&anyhow::anyhow!("NoSuchKey: not found")));
    }

    #[test]
    fn glacier_skipped_by_default() {
        // Without --force-glacier-transfer, a GLACIER object is skipped.
        let sc = crate::storage::StorageClass("GLACIER".to_string());
        assert!(skip_glacier(&sc, false));
    }

    #[test]
    fn glacier_not_skipped_when_forced() {
        // #812: --force-glacier-transfer overrides the skip so glacier objects
        // are transferred, mirroring cp's behavior.
        let sc = crate::storage::StorageClass("GLACIER".to_string());
        assert!(!skip_glacier(&sc, true));
    }

    #[test]
    fn non_glacier_never_skipped() {
        // STANDARD (and the empty default) are never skipped, with or without
        // the flag.
        let std = crate::storage::StorageClass("STANDARD".to_string());
        assert!(!skip_glacier(&std, false));
        assert!(!skip_glacier(&std, true));
        let empty = crate::storage::StorageClass::default();
        assert!(!skip_glacier(&empty, false));
        assert!(!skip_glacier(&empty, true));
    }

    #[test]
    fn filters_exclude_and_include() {
        let f = Filters::new(&[], &["*.txt".to_string()]).unwrap();
        assert!(f.should_skip("a.txt"));
        assert!(!f.should_skip("a.csv"));

        let f = Filters::new(&["*.csv".to_string()], &[]).unwrap();
        assert!(!f.should_skip("a.csv"));
        assert!(f.should_skip("a.txt"));
    }
}
