//! `cp` / `mv` — copy objects between local fs and S3, in any direction.

use std::path::PathBuf;
use std::sync::Arc;

use clap::Args;

use super::filters::{filter_key, patterns_with_files, Filters};
use super::GlobalOpts;
use crate::storage::s3::S3;
use crate::storage::url::Url;
use crate::storage::{new_client, Metadata, Options};

#[derive(Args, Debug)]
pub struct CpArgs {
    /// One or more sources (local paths or s3:// URLs), may contain wildcards,
    /// followed by a single destination as the final positional. With more than
    /// one source the destination must be a directory (local) or prefix (s3).
    #[arg(required = true, num_args = 1.., value_name = "PATH")]
    pub paths: Vec<String>,

    /// Follow symbolic links when walking local directories.
    #[arg(long, default_value_t = true)]
    pub follow_symlinks: bool,

    /// Storage class for the destination object.
    #[arg(long)]
    pub storage_class: Option<String>,

    /// Canned ACL for the destination object.
    #[arg(long)]
    pub acl: Option<String>,

    /// Content-Type for the destination object.
    #[arg(long)]
    pub content_type: Option<String>,

    /// Use the io_uring fast path (Linux, requires the `fast` build feature and
    /// an explicit --endpoint-url). Optimized for many small objects.
    #[arg(long)]
    pub fast: bool,

    /// Multipart part size in MiB for large objects (min 5).
    #[arg(long, default_value_t = 8)]
    pub part_size: u64,

    /// Concurrent parts per large object (multipart upload / ranged download).
    #[arg(long, default_value_t = 8)]
    pub concurrency: usize,

    /// Copy the given source object version id (single-object remote source).
    #[arg(long)]
    pub version_id: Option<String>,

    /// Copy every version of a single remote source key (routes the source
    /// through the ListObjectVersions path, like `ls`/`cat`). Each version is
    /// written to a distinct destination by appending its version id, so the
    /// versions do not collide (#762).
    #[arg(long)]
    pub all_versions: bool,

    /// Force transfer of glacier objects whether they are restored or not.
    /// `cp` transfers every listed object regardless of storage class, so this
    /// flag is accepted for compatibility (and parity with `sync`'s
    /// `--force-glacier-transfer`); it does not change `cp`'s behavior (#812).
    #[arg(long)]
    pub force_glacier_transfer: bool,

    /// Preserve file modification time: store the local mtime as object
    /// metadata on upload, and restore it onto the file on download.
    #[arg(long)]
    pub preserve_timestamps: bool,

    /// For remote→remote copies, stream through the client (download then
    /// upload) instead of a server-side CopyObject. Useful when server-side
    /// copy is unavailable or disallowed.
    #[arg(long)]
    pub client_copy: bool,

    /// On `mv` of local files to remote, after removing each moved source file
    /// also remove any source directories it emptied, walking up toward (but
    /// never past) the move source root. Non-empty or otherwise un-removable
    /// directories are skipped silently. No effect on `cp`.
    #[arg(long)]
    pub remove_empty_dirs: bool,

    /// Conditional write: only write the destination if it does not already
    /// exist (S3 `If-None-Match: "*"`). An existing destination object is left
    /// untouched and reported as skipped instead of overwritten (#752).
    #[arg(long = "if-none-match")]
    pub if_none_match: bool,

    /// rclone-style symlink round-trip (unix). Instead of following a local
    /// symlink on upload, store its target as a small placeholder object whose
    /// key gets a recognizable suffix (`.s5cmdlink`). On download, a key ending
    /// in that suffix is recreated as a symlink pointing at the stored target,
    /// with the suffix stripped from the local name (#785).
    #[arg(long)]
    pub links: bool,

    /// Exclude objects whose relative path matches the given glob (repeatable).
    /// Only applied when a source expands via wildcard/prefix/directory listing;
    /// a single concrete source is never filtered.
    #[arg(long)]
    pub exclude: Vec<String>,

    /// Only include objects whose relative path matches the given glob
    /// (repeatable). Only applied when a source expands via
    /// wildcard/prefix/directory listing; a single concrete source is never
    /// filtered.
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
}

/// Suffix appended to an object key when a local symlink is stored as a
/// placeholder object via `--links`. The object body holds the link target.
pub const LINK_SUFFIX: &str = ".s5cmdlink";

impl CpArgs {
    fn metadata(&self) -> Metadata {
        Metadata {
            storage_class: self.storage_class.clone(),
            acl: self.acl.clone(),
            content_type: self.content_type.clone(),
            if_none_match: self.if_none_match,
            ..Default::default()
        }
    }
}

pub async fn run(global: &GlobalOpts, args: CpArgs, is_move: bool) -> anyhow::Result<()> {
    let mut opts = global.storage_options();
    opts.part_size = args.part_size.max(5).saturating_mul(1024 * 1024);
    opts.concurrency = args.concurrency.max(1);
    opts.preserve_timestamps = args.preserve_timestamps;
    opts.client_copy = args.client_copy;
    opts.remove_empty_dirs = args.remove_empty_dirs;
    let op = if is_move { "mv" } else { "cp" };

    // The final positional is the destination; everything before it is a source.
    // `clap` guarantees at least one positional (`required = true`), but a lone
    // positional means a source was given with no destination.
    if args.paths.len() < 2 {
        anyhow::bail!("{op} requires at least one source and a destination");
    }
    let dst_raw = args.paths.last().expect("paths is non-empty");
    let src_raws = &args.paths[..args.paths.len() - 1];

    let dst = Url::parse(dst_raw).map_err(|e| anyhow::anyhow!(e))?;
    let metadata = args.metadata();

    // With more than one source, the destination must be a directory (local) or
    // a prefix/bucket (s3) — a single named target cannot receive many objects.
    let multi_source = src_raws.len() > 1;
    if multi_source && !dest_is_dir_like(&dst) {
        anyhow::bail!(
            "{op}: destination {dst_raw} must be a directory or s3 prefix when copying multiple sources"
        );
    }

    // Per-source version-id / all-versions only make sense for a single source.
    if src_raws.len() > 1 && (args.version_id.is_some() || args.all_versions) {
        anyhow::bail!("--version-id/--all-versions require exactly one source");
    }

    // Compile include/exclude filters into regexes once. Inline patterns are
    // combined with any read from `--include-from`/`--exclude-from` files. The
    // filters only ever apply to objects discovered by expanding a wildcard /
    // prefix / directory source — a single concrete source is never filtered
    // (matching rm's documented semantics).
    let includes = patterns_with_files(&args.include, &args.include_from)?;
    let excludes = patterns_with_files(&args.exclude, &args.exclude_from)?;
    let filters = Filters::new(&includes, &excludes)?;

    // Expand every source through the existing single-source logic and collect
    // all (src, dst) pairs so they share one client and one worker pool.
    let mut pairs: Vec<(Url, Url)> = Vec::new();
    for raw in src_raws {
        let src = Url::new(
            raw,
            crate::storage::url::UrlOptions {
                version_id: args.version_id.clone(),
                all_versions: args.all_versions,
                ..Default::default()
            },
        )
        .map_err(|e| anyhow::anyhow!(e))?;
        let expanded = expand_sources(&src, &dst, &opts, args.follow_symlinks, &filters).await?;
        pairs.extend(expanded);
    }

    // Any remote side present means we need an S3 client. Use the first source
    // (or the destination) that is remote as the config anchor.
    let any_src_remote = pairs.iter().any(|(s, _)| s.is_remote());
    let dst_remote = dst.is_remote();

    // io_uring fast path: dispatch small-object upload/download/server-side-copy
    // sets (any mix as long as each pair touches a remote) to the per-core
    // monoio engine. Falls back to the default path for local->local.
    if args.fast {
        #[cfg(feature = "fast")]
        {
            if let Some(r) = try_fast_path(global, &pairs, is_move, op).await? {
                return r;
            }
        }
        #[cfg(not(feature = "fast"))]
        eprintln!("warning: --fast ignored (built without the `fast` feature); using default path");
    }

    // Build the S3 client(s) ONCE and share across all transfers. (A single cp
    // invocation has one direction, so one client — from whichever side is
    // remote — serves every object. Re-creating it per object would reload the
    // whole AWS config + credential chain each time.)
    //
    // Per-side region/endpoint support (#858/#816/#514/#702/#700/#671): when the
    // source and destination resolve to different regions/endpoints we build TWO
    // clients — one anchored on the source side, one on the destination side —
    // so an s3->s3 copy can bridge them via a download+upload streaming copy.
    // When the sides are identical (the common case, and always so when no
    // per-side flags are given) we keep the single-client fast path so behavior
    // and cost are unchanged.
    let sides_differ = opts.sides_differ();
    let (s3, s3_dst): (Option<Arc<S3>>, Option<Arc<S3>>) = if any_src_remote || dst_remote {
        if sides_differ {
            // Source-anchored client: prefer a remote source pair, else dst.
            let src_anchor = pairs
                .iter()
                .find(|(s, _)| s.is_remote())
                .map(|(s, _)| s)
                .unwrap_or(&dst);
            let src_opts = opts.for_side(crate::storage::Side::Source);
            let dst_opts = opts.for_side(crate::storage::Side::Destination);
            let src_client = Arc::new(S3::new(src_anchor, &src_opts).await?);
            let dst_client = Arc::new(S3::new(&dst, &dst_opts).await?);
            (Some(src_client), Some(dst_client))
        } else {
            // Single shared client (fast path). Anchor on whichever side is
            // remote: prefer a remote source pair, otherwise the destination.
            let anchor = pairs
                .iter()
                .find(|(s, _)| s.is_remote())
                .map(|(s, _)| s)
                .unwrap_or(&dst);
            (Some(Arc::new(S3::new(anchor, &opts).await?)), None)
        }
    } else {
        (None, None)
    };

    let opts = Arc::new(opts);
    let metadata = Arc::new(metadata);
    let workers = global.numworkers.max(1);

    // Spawn each transfer as a tokio task so the multi-threaded runtime spreads
    // the per-request CPU (request build, SigV4 signing, response parse) across
    // all cores. (`buffer_unordered` would poll every future inside one task on
    // one thread, serializing that CPU and leaving transfers latency-bound on a
    // single core.) The in-flight `JoinSet` is bounded to `workers`, which caps
    // both running tasks *and* buffered completed results — important when the
    // object count is huge.
    let mut had_error = false;
    // Count-of-transfers progress bar (no-op in JSON mode / non-TTY stderr).
    let pb = crate::progress::Progress::new(pairs.len() as u64, op);
    let mut set = tokio::task::JoinSet::new();
    for (s, d) in pairs.into_iter() {
        while set.len() >= workers {
            let (s, d, r) = set.join_next().await.unwrap().expect("transfer task panicked");
            report(op, &s, &d, r, &mut had_error);
            pb.inc(1);
        }
        let opts = Arc::clone(&opts);
        let metadata = Arc::clone(&metadata);
        let s3 = s3.clone();
        let s3_dst = s3_dst.clone();
        let links = args.links;
        set.spawn(async move {
            let r = copy_one(
                &s,
                &d,
                s3.as_deref(),
                s3_dst.as_deref(),
                &opts,
                &metadata,
                is_move,
                links,
            )
            .await;
            (s, d, r)
        });
    }
    while let Some(joined) = set.join_next().await {
        let (s, d, r) = joined.expect("transfer task panicked");
        report(op, &s, &d, r, &mut had_error);
        pb.inc(1);
    }
    pb.finish();

    if had_error {
        anyhow::bail!("one or more {op} operations failed");
    }
    Ok(())
}

/// Prints the per-transfer result line, tracking whether any failed.
fn report(op: &str, s: &Url, d: &Url, r: anyhow::Result<()>, had_error: &mut bool) {
    match r {
        Ok(()) => crate::output::op_success(op, &s.to_string(), Some(&d.to_string())),
        // A failed `--if-none-match` conditional write means the destination
        // already exists: this is a skip, not a failure, so it must NOT set
        // had_error (the overall command still succeeds) (#752).
        Err(ref e) if e.downcast_ref::<crate::storage::PreconditionFailedError>().is_some() => {
            eprintln!("{op} {s} {d}: object already exists, skipped");
        }
        Err(e) => {
            *had_error = true;
            // Annotate raw EMFILE with the RLIMIT_NOFILE hint (#390).
            let msg = crate::error::format_error(&e);
            crate::output::op_error(op, &s.to_string(), Some(&d.to_string()), &msg);
        }
    }
}

/// Determines whether the source expands to multiple objects, and builds the
/// list of (source, destination) URL pairs to copy.
async fn expand_sources(
    src: &Url,
    dst: &Url,
    opts: &Options,
    follow_symlinks: bool,
    filters: &Filters,
) -> anyhow::Result<Vec<(Url, Url)>> {
    let client = new_client(src, opts).await?;

    // `--all-versions` against a single remote key: route the source through the
    // ListObjectVersions path (the same listing `ls`/`cat` use when versioned)
    // and copy every version, giving each its own destination so they do not
    // collide (#762). `src.all_versions` makes `list()` dispatch to the version
    // lister, where every emitted object carries its `version_id`.
    if src.is_remote() && src.all_versions && !src.is_wildcard() {
        return expand_all_versions(client.as_ref(), src, dst, follow_symlinks).await;
    }

    let is_multi = if src.is_wildcard() {
        true
    } else if src.is_remote() {
        src.is_bucket() || src.is_prefix()
    } else {
        // Local: a directory expands.
        std::fs::metadata(src.absolute())
            .map(|m| m.is_dir())
            .unwrap_or(false)
    };

    if !is_multi {
        let dst_url = resolve_single_dest(src, dst);
        return Ok(vec![(src.clone(), dst_url)]);
    }

    // Expand by listing.
    let mut rx = client.list(src, follow_symlinks);
    let mut pairs = Vec::new();
    while let Some(obj) = rx.recv().await {
        if let Some(err) = obj.err {
            // A per-entry listing error (e.g. a broken symlink) must not abort
            // the whole transfer; warn and keep copying the good files (#749).
            eprintln!("{err:#}");
            continue;
        }
        let Some(obj_url) = obj.url else { continue };
        if obj.typ.is_dir() {
            continue;
        }
        // Skip objects rejected by the include/exclude filters. Only reached on
        // the expansion path, so a single concrete source is never filtered.
        if filters.should_skip(&filter_key(&obj_url)) {
            continue;
        }
        // Destination keeps the source's relative layout under dst as a prefix.
        let rel = obj_url.relative();
        let dst_url = dst.join(&rel);
        pairs.push((obj_url, dst_url));
    }
    Ok(pairs)
}

/// Expands a single remote `--all-versions` source into one (source, dest) pair
/// per object version. Each source URL carries its own version id (so the GET
/// fetches that exact version), and each destination has the version id appended
/// to its base name so the versions are written side by side instead of
/// overwriting one another (#762).
///
/// Delete markers (versioned tombstones) and common prefixes are skipped — they
/// have no body to copy. A per-entry listing error is reported and skipped so a
/// single bad entry does not abort the whole copy (mirrors the default path).
async fn expand_all_versions(
    client: &dyn crate::storage::Storage,
    src: &Url,
    dst: &Url,
    follow_symlinks: bool,
) -> anyhow::Result<Vec<(Url, Url)>> {
    // Is the destination a directory (a place to drop multiple files) rather
    // than a single named target? When directory-like, versions land under it as
    // `<base>_<versionid>`; otherwise the single dst path itself is suffixed.
    let dst_is_dir = dest_is_dir_like(dst);

    let mut rx = client.list(src, follow_symlinks);
    let mut pairs = Vec::new();
    while let Some(obj) = rx.recv().await {
        if let Some(err) = obj.err {
            eprintln!("{err:#}");
            continue;
        }
        // Delete markers carry a version id but no body; skip them.
        if obj.is_delete_marker || obj.typ.is_dir() {
            continue;
        }
        let Some(obj_url) = obj.url else { continue };
        // A version with no id cannot be disambiguated; skip defensively.
        if obj_url.version_id.is_empty() {
            continue;
        }
        let dst_url = versioned_dest(&obj_url, dst, dst_is_dir);
        pairs.push((obj_url, dst_url));
    }
    Ok(pairs)
}

/// Builds the per-version destination for `--all-versions`, appending the source
/// object's version id to the base name so distinct versions never collide. When
/// `dst_is_dir` the file lands under `dst` as `<base>_<versionid>`; otherwise the
/// version id is appended directly to the single named destination
/// (`<dst>_<versionid>`).
fn versioned_dest(src: &Url, dst: &Url, dst_is_dir: bool) -> Url {
    let vid = &src.version_id;
    if dst_is_dir {
        dst.join(&format!("{}_{}", src.base(), vid))
    } else {
        // Append directly to the destination path (NOT a path-join, which would
        // insert a separator) so the suffix stays part of the file name.
        let mut out = dst.clone();
        out.path = format!("{}_{}", out.path, vid);
        out
    }
}

/// Whether `dst` is a place that can receive multiple objects: an s3 bucket or
/// prefix (trailing `/`), or a local directory (existing dir or trailing `/`).
fn dest_is_dir_like(dst: &Url) -> bool {
    if dst.is_remote() {
        dst.is_bucket() || dst.absolute().ends_with('/')
    } else {
        dst.absolute().ends_with('/')
            || std::fs::metadata(dst.absolute())
                .map(|m| m.is_dir())
                .unwrap_or(false)
    }
}

/// Resolves the destination for a single-object copy. If dst is directory-like,
/// the source base name is appended.
fn resolve_single_dest(src: &Url, dst: &Url) -> Url {
    if dest_is_dir_like(dst) {
        dst.join(&src.base())
    } else {
        dst.clone()
    }
}

/// Copies a single source object to a single destination, picking the transfer
/// direction from the remote/local kinds. For `mv`, deletes the source after a
/// successful copy.
async fn copy_one(
    src: &Url,
    dst: &Url,
    s3: Option<&S3>,
    s3_dst: Option<&S3>,
    opts: &Options,
    metadata: &Metadata,
    is_move: bool,
    links: bool,
) -> anyhow::Result<()> {
    // rclone-style `--links` symlink round-trip (unix only). On upload we store
    // a symlink's target as a placeholder object (suffixed key); on download we
    // recreate the symlink from such a placeholder. Both short-circuit the
    // normal file transfer below.
    #[cfg(unix)]
    if links {
        // local -> remote: source is a symlink => store placeholder.
        if !src.is_remote() && dst.is_remote() {
            if is_symlink_path(&src.absolute()) {
                let s3 = s3_dst.or(s3).expect("upload requires an S3 client");
                upload_symlink(s3, src, dst, opts).await?;
                if is_move && !opts.dry_run {
                    std::fs::remove_file(src.absolute())?;
                }
                return Ok(());
            }
        }
        // remote -> local: placeholder object => recreate symlink.
        if src.is_remote() && !dst.is_remote() && src.path.ends_with(LINK_SUFFIX) {
            let s3 = s3.expect("download requires an S3 client");
            download_symlink(s3, src, dst, opts).await?;
            if is_move {
                s3.delete(src).await?;
            }
            return Ok(());
        }
    }

    match (src.is_remote(), dst.is_remote()) {
        // remote -> remote: server-side copy, or client-side (download+upload)
        // streaming when --client-copy is set or when the two sides resolve to
        // different regions/endpoints (a single CopyObject cannot bridge two
        // endpoints, so we download from the source client and upload to the
        // destination client) (#858/#816/#514/#702/#700/#671).
        (true, true) => {
            let s3 = s3.expect("remote copy requires an S3 client");
            if let Some(dst_s3) = s3_dst {
                // Per-side clients: cross-client streaming copy.
                s3.client_copy_to(dst_s3, src, dst, metadata).await?;
            } else if opts.client_copy {
                s3.client_copy(src, dst, metadata).await?;
            } else {
                s3.copy(src, dst, metadata).await?;
            }
            if is_move {
                s3.delete(src).await?;
            }
        }
        // remote -> local: download.
        (true, false) => {
            let s3 = s3.expect("download requires an S3 client");
            s3.download(src, &PathBuf::from(dst.absolute())).await?;
            if is_move {
                s3.delete(src).await?;
            }
        }
        // local -> remote: upload. When per-side clients exist, the destination
        // client is the one anchored on the destination's region/endpoint.
        (false, true) => {
            let s3 = s3_dst
                .or(s3)
                .expect("upload requires an S3 client");
            s3.upload(&PathBuf::from(src.absolute()), dst, metadata)
                .await?;
            if is_move && !opts.dry_run {
                std::fs::remove_file(src.absolute())?;
                // Opt-in: prune source directories the move just emptied,
                // walking up but never at/above the move source root (#846).
                if opts.remove_empty_dirs {
                    prune_empty_dirs(src);
                }
            }
        }
        // local -> local: filesystem copy.
        (false, false) => {
            let fs = new_client(src, opts).await?;
            fs.copy(src, dst, metadata).await?;
            if is_move && !opts.dry_run {
                std::fs::remove_file(src.absolute())?;
            }
        }
    }
    Ok(())
}

/// Whether the given local path is itself a symlink (does not follow it).
#[cfg(unix)]
fn is_symlink_path(path: &str) -> bool {
    std::fs::symlink_metadata(path)
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false)
}

/// Stores a local symlink as a placeholder object (`--links`). The object key
/// is the resolved destination key with [`LINK_SUFFIX`] appended, and the body
/// is the link target string read via `std::fs::read_link` (the link is NOT
/// followed). (#785)
#[cfg(unix)]
async fn upload_symlink(s3: &S3, src: &Url, dst: &Url, opts: &Options) -> anyhow::Result<()> {
    let target = std::fs::read_link(src.absolute())
        .map_err(|e| anyhow::anyhow!("reading symlink {}: {e}", src.absolute()))?;
    let target_str = target.to_string_lossy().to_string();

    // Append the suffix to the destination key (NOT a path-join, which would
    // insert a separator) so it stays part of the object name.
    let mut link_dst = dst.clone();
    link_dst.path = format!("{}{}", link_dst.path, LINK_SUFFIX);

    if opts.dry_run {
        return Ok(());
    }
    s3.put_object_bytes(&link_dst, target_str.into_bytes()).await
}

/// Recreates a symlink from a placeholder object (`--links`). The object body
/// is the link target; the symlink is created at the destination path with
/// [`LINK_SUFFIX`] stripped from the file name. (#785)
#[cfg(unix)]
async fn download_symlink(s3: &S3, src: &Url, dst: &Url, opts: &Options) -> anyhow::Result<()> {
    let body = s3.get_object_bytes(src).await?;
    let target = String::from_utf8_lossy(&body).to_string();

    // Strip the suffix from the local destination path.
    let dst_abs = dst.absolute();
    let link_path = match dst_abs.strip_suffix(LINK_SUFFIX) {
        Some(stripped) => PathBuf::from(stripped),
        None => PathBuf::from(&dst_abs),
    };

    if opts.dry_run {
        return Ok(());
    }
    if let Some(parent) = link_path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    // Remove any existing entry so symlink creation does not fail with EEXIST.
    let _ = tokio::fs::remove_file(&link_path).await;
    std::os::unix::fs::symlink(&target, &link_path)
        .map_err(|e| anyhow::anyhow!("creating symlink {} -> {target}: {e}", link_path.display()))?;
    Ok(())
}

/// After a local→remote `mv` removed the source file `src`, attempt to remove
/// the source directories it just emptied, walking up from the file's parent
/// toward — but never reaching or passing — the move source root.
///
/// The move source root is derived from the file's relative path within the
/// move: `src.relative()` is the path of the file relative to the move base
/// (e.g. `sub/inner/c.txt` for `mv root/* …`, or `data/sub/3.txt` for
/// `mv data …`), so stripping that many trailing components off the absolute
/// file path yields the directory the relative layout is anchored at. We never
/// remove that anchor directory or anything at/above it.
///
/// `std::fs::remove_dir` only succeeds on an empty directory, so a non-empty
/// parent (or any other error) simply stops the climb. Pruning is best-effort
/// and never fatal: a non-empty/unremovable dir is silently skipped.
fn prune_empty_dirs(src: &Url) {
    let abs = src.absolute();
    let file_path = std::path::Path::new(&abs);

    // Number of path components in the relative path (the file plus any dirs the
    // move created beneath the anchor). With only one component there are no
    // intermediate move-created dirs to prune (e.g. a bare single-file move), so
    // there is nothing to do — and importantly nothing above the file's own
    // parent may be touched.
    let rel = src.relative();
    let rel_components = std::path::Path::new(&rel)
        .components()
        .filter(|c| matches!(c, std::path::Component::Normal(_)))
        .count();
    if rel_components <= 1 {
        return;
    }

    // The anchor (move source root) is the absolute path with `rel_components`
    // trailing components removed. Everything strictly below it may be pruned;
    // the anchor itself never is.
    let mut anchor = file_path.to_path_buf();
    for _ in 0..rel_components {
        match anchor.parent() {
            Some(p) => anchor = p.to_path_buf(),
            None => return,
        }
    }

    let mut cur = file_path.parent().map(|p| p.to_path_buf());
    while let Some(dir) = cur {
        // Stop at/above the anchor or anything outside it.
        if dir == anchor || !dir.starts_with(&anchor) {
            break;
        }
        // remove_dir fails on a non-empty directory; treat that (and any other
        // error) as a non-fatal stop.
        if std::fs::remove_dir(&dir).is_err() {
            break;
        }
        cur = dir.parent().map(|p| p.to_path_buf());
    }
}

use crate::storage::Storage as _;

/// Resolves AWS credentials + region via the default provider chain (env →
/// profile → SSO → IMDS), honoring `--region`/`--profile`. Returns
/// (access_key, secret_key, session_token, region).
#[cfg(feature = "fast")]
async fn resolve_credentials(
    global: &GlobalOpts,
) -> anyhow::Result<(String, String, Option<String>, String)> {
    use aws_config::BehaviorVersion;
    use aws_credential_types::provider::ProvideCredentials;

    let mut loader = aws_config::defaults(BehaviorVersion::latest());
    if let Some(r) = &global.region {
        loader = loader.region(aws_sdk_s3::config::Region::new(r.clone()));
    }
    if let Some(p) = &global.profile {
        loader = loader.profile_name(p);
    }
    let conf = loader.load().await;

    let provider = conf
        .credentials_provider()
        .ok_or_else(|| anyhow::anyhow!("no AWS credentials found (env/profile/SSO/IMDS)"))?;
    let creds = provider
        .provide_credentials()
        .await
        .map_err(|e| anyhow::anyhow!("resolving AWS credentials: {e}"))?;

    let region = conf
        .region()
        .map(|r| r.to_string())
        .or_else(|| global.region.clone())
        .unwrap_or_else(|| "us-east-1".to_string());

    Ok((
        creds.access_key_id().to_string(),
        creds.secret_access_key().to_string(),
        creds.session_token().map(|s| s.to_string()),
        region,
    ))
}

/// Attempts to run the (src,dst) pairs on the io_uring fast path. Each pair is
/// mapped independently to upload / download / server-side copy, so mixed sets
/// are supported. Returns `Ok(Some(result))` if handled, or `Ok(None)` if the
/// set contains a local->local pair (caller falls back to the default path).
#[cfg(feature = "fast")]
async fn try_fast_path(
    global: &GlobalOpts,
    pairs: &[(Url, Url)],
    is_move: bool,
    op: &str,
) -> anyhow::Result<Option<anyhow::Result<()>>> {
    use crate::fastpath::{run_transfers, Endpoint, FastConfig, Transfer};

    if pairs.is_empty() {
        return Ok(None);
    }

    // The fast path supports any pair whose source or destination is remote:
    // upload (local->remote), download (remote->local), and server-side copy
    // (remote->remote). A local->local pair cannot use it, so if ANY pair is
    // local->local we fall back entirely to the default path.
    if pairs.iter().any(|(s, d)| !s.is_remote() && !d.is_remote()) {
        return Ok(None);
    }

    // Endpoint: the fast path needs an explicit path-style endpoint for now.
    let Some(ep_raw) = global.endpoint_url.clone() else {
        anyhow::bail!("--fast currently requires --endpoint-url (e.g. http://host:9000)");
    };
    let ep_uri: http::Uri = ep_raw.parse()?;
    let scheme = ep_uri.scheme_str().unwrap_or("http").to_string();
    if scheme != "http" && scheme != "https" {
        anyhow::bail!("--fast endpoint must be http or https");
    }
    let default_port = if scheme == "https" { 443 } else { 80 };
    let endpoint = Endpoint {
        host: ep_uri
            .host()
            .ok_or_else(|| anyhow::anyhow!("invalid --endpoint-url host"))?
            .to_string(),
        port: ep_uri.port_u16().unwrap_or(default_port),
        scheme,
        no_verify: global.no_verify_ssl,
    };

    // Resolve credentials via the standard AWS provider chain (env, profile,
    // SSO, IMDS, ...) — the same resolution the SDK path uses — then hand the
    // concrete keys to the per-core signer.
    let (access_key, secret_key, session_token, region) = resolve_credentials(global).await?;

    // Core / concurrency sizing.
    let avail = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    let cores = std::env::var("RS5CMD_FAST_CORES")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or_else(|| avail.min(16))
        .max(1);
    let per_core_concurrency = (global.numworkers / cores).max(1);

    // Map each pair independently to the right transfer variant based on which
    // side(s) are remote. Mixed-direction sets are supported.
    let transfers: Vec<Transfer> = pairs
        .iter()
        .map(|(s, d)| match (s.is_remote(), d.is_remote()) {
            (false, true) => Transfer::Upload {
                local: std::path::PathBuf::from(s.absolute()),
                bucket: d.bucket.clone(),
                key: d.path.clone(),
            },
            (true, false) => Transfer::Download {
                bucket: s.bucket.clone(),
                key: s.path.clone(),
                local: std::path::PathBuf::from(d.absolute()),
            },
            (true, true) => Transfer::Copy {
                src_bucket: s.bucket.clone(),
                src_key: s.path.clone(),
                dst_bucket: d.bucket.clone(),
                dst_key: d.path.clone(),
            },
            // local->local was already excluded above.
            (false, false) => unreachable!("local->local excluded from fast path"),
        })
        .collect();

    let cfg = FastConfig {
        endpoint,
        access_key,
        secret_key,
        session_token,
        region,
        cores,
        per_core_concurrency,
        max_retries: global.retry_count,
        dry_run: global.dry_run,
        is_move,
    };

    // run_transfers blocks (spawns OS threads); keep it off the tokio worker.
    let outcomes =
        tokio::task::block_in_place(|| run_transfers(transfers, cfg));

    let mut had_error = false;
    for o in &outcomes {
        match &o.result {
            Ok(()) => crate::output::op_success(op, &o.source, Some(&o.destination)),
            Err(e) => {
                had_error = true;
                crate::output::op_error(op, &o.source, Some(&o.destination), &format!("{e:#}"));
            }
        }
    }
    if had_error {
        return Ok(Some(Err(anyhow::anyhow!("one or more fast-path transfers failed"))));
    }
    Ok(Some(Ok(())))
}
