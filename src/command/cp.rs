//! `cp` / `mv` — copy objects between local fs and S3, in any direction.

use std::path::PathBuf;
use std::sync::Arc;

use clap::Args;

use super::GlobalOpts;
use crate::storage::s3::S3;
use crate::storage::url::Url;
use crate::storage::{new_client, Metadata, Options};

#[derive(Args, Debug)]
pub struct CpArgs {
    /// Source (local path or s3:// URL), may contain wildcards.
    pub src: String,
    /// Destination (local path or s3:// URL).
    pub dst: String,

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
}

impl CpArgs {
    fn metadata(&self) -> Metadata {
        Metadata {
            storage_class: self.storage_class.clone(),
            acl: self.acl.clone(),
            content_type: self.content_type.clone(),
            ..Default::default()
        }
    }
}

pub async fn run(global: &GlobalOpts, args: CpArgs, is_move: bool) -> anyhow::Result<()> {
    let mut opts = global.storage_options();
    opts.part_size = args.part_size.max(5).saturating_mul(1024 * 1024);
    opts.concurrency = args.concurrency.max(1);
    let src = Url::new(
        &args.src,
        crate::storage::url::UrlOptions {
            version_id: args.version_id.clone(),
            ..Default::default()
        },
    )
    .map_err(|e| anyhow::anyhow!(e))?;
    let dst = Url::parse(&args.dst).map_err(|e| anyhow::anyhow!(e))?;
    let metadata = args.metadata();

    let pairs = expand_sources(&src, &dst, &opts, args.follow_symlinks).await?;
    let op = if is_move { "mv" } else { "cp" };

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

    // Build the S3 client ONCE and share it across all transfers. (A single cp
    // invocation has one direction, so one client — from whichever side is
    // remote — serves every object. Re-creating it per object would reload the
    // whole AWS config + credential chain each time.)
    let s3: Option<Arc<S3>> = if src.is_remote() || dst.is_remote() {
        let anchor = if src.is_remote() { &src } else { &dst };
        Some(Arc::new(S3::new(anchor, &opts).await?))
    } else {
        None
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
        set.spawn(async move {
            let r = copy_one(&s, &d, s3.as_deref(), &opts, &metadata, is_move).await;
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
        Err(e) => {
            *had_error = true;
            crate::output::op_error(op, &s.to_string(), Some(&d.to_string()), &format!("{e:#}"));
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
) -> anyhow::Result<Vec<(Url, Url)>> {
    let client = new_client(src, opts).await?;

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
            return Err(err);
        }
        let Some(obj_url) = obj.url else { continue };
        if obj.typ.is_dir() {
            continue;
        }
        // Destination keeps the source's relative layout under dst as a prefix.
        let rel = obj_url.relative();
        let dst_url = dst.join(&rel);
        pairs.push((obj_url, dst_url));
    }
    Ok(pairs)
}

/// Resolves the destination for a single-object copy. If dst is directory-like,
/// the source base name is appended.
fn resolve_single_dest(src: &Url, dst: &Url) -> Url {
    let dir_like = if dst.is_remote() {
        dst.is_bucket() || dst.absolute().ends_with('/')
    } else {
        dst.absolute().ends_with('/')
            || std::fs::metadata(dst.absolute())
                .map(|m| m.is_dir())
                .unwrap_or(false)
    };

    if dir_like {
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
    opts: &Options,
    metadata: &Metadata,
    is_move: bool,
) -> anyhow::Result<()> {
    match (src.is_remote(), dst.is_remote()) {
        // remote -> remote: server-side copy.
        (true, true) => {
            let s3 = s3.expect("remote copy requires an S3 client");
            s3.copy(src, dst, metadata).await?;
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
        // local -> remote: upload.
        (false, true) => {
            let s3 = s3.expect("upload requires an S3 client");
            s3.upload(&PathBuf::from(src.absolute()), dst, metadata)
                .await?;
            if is_move && !opts.dry_run {
                std::fs::remove_file(src.absolute())?;
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
