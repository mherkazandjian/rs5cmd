//! `cat` — print object contents to stdout.
//!
//! A single concrete object source (e.g. `s3://bucket/key`) takes the original
//! fast path that streams the object body straight to stdout. A wildcard,
//! prefix or bucket-root source is expanded via `list()` into the set of
//! matching objects, whose bodies are concatenated to stdout.
//!
//! Unlike `select`, `cat` emits raw bytes and concatenation order matters, so
//! the expanded objects are processed strictly SEQUENTIALLY (no concurrency)
//! and sorted by key, making the output deterministic and never interleaved.

use clap::Args;
use tokio::io::AsyncWriteExt;

use super::GlobalOpts;
use crate::storage::s3::S3;
use crate::storage::url::Url;
use crate::storage::Storage as _;

#[derive(Args, Debug)]
pub struct CatArgs {
    /// Source object (s3:// URL or local path).
    pub src: String,

    /// Operate on the given object version id.
    #[arg(long)]
    pub version_id: Option<String>,
}

pub async fn run(global: &GlobalOpts, args: CatArgs) -> anyhow::Result<()> {
    let opts = global.storage_options();
    let url = Url::new(
        &args.src,
        crate::storage::url::UrlOptions {
            version_id: args.version_id.clone(),
            ..Default::default()
        },
    )
    .map_err(|e| anyhow::anyhow!(e))?;

    let mut stdout = tokio::io::stdout();

    // A wildcard / prefix / bucket-root *remote* source is expanded and the
    // matching objects are concatenated. Concrete single objects (remote or
    // local) keep the original behavior exactly.
    //
    // NOTE (simplification): local wildcards/prefixes are not expanded here —
    // a local path is always treated as a single file, matching the original
    // behavior. Expanding local globs is out of scope.
    if url.is_remote() && (url.is_wildcard() || url.is_bucket() || url.is_prefix()) {
        // NOTE (simplification): `--version-id` only applies to the
        // single-object path; a versioned wildcard cat is out of scope.
        if args.version_id.is_some() {
            anyhow::bail!("--version-id cannot be combined with a wildcard/prefix/bucket source");
        }
        let s3 = S3::new(&url, &opts).await?;
        return cat_expanded(&s3, &url, &mut stdout).await;
    }

    if url.is_remote() {
        let s3 = S3::new(&url, &opts).await?;
        let mut body = s3.read(&url).await?;
        while let Some(chunk) = body.try_next().await? {
            stdout.write_all(&chunk).await?;
        }
    } else {
        let data = tokio::fs::read(url.absolute()).await?;
        stdout.write_all(&data).await?;
    }
    stdout.flush().await?;
    Ok(())
}

/// Expands a wildcard/prefix/bucket source via `list()`, then streams every
/// matched object's bytes to `stdout` strictly in sequence, sorted by key, so
/// the concatenation is stable and non-interleaved.
///
/// Directories / common prefixes are skipped. A listing error is propagated
/// immediately.
///
/// NOTE (error policy): a per-object read failure aborts the whole operation
/// rather than continuing — the simplest behavior, and it avoids emitting a
/// truncated/partial concatenation silently. (Some bytes of the failing object
/// may already have reached stdout before the failure surfaces.)
async fn cat_expanded(
    s3: &S3,
    url: &Url,
    stdout: &mut (impl AsyncWriteExt + Unpin),
) -> anyhow::Result<()> {
    // Collect the matched object URLs, skipping directories / common prefixes.
    let mut rx = s3.list(url, false);
    let mut srcs: Vec<Url> = Vec::new();
    while let Some(obj) = rx.recv().await {
        if let Some(err) = obj.err {
            return Err(err);
        }
        if obj.typ.is_dir() {
            continue;
        }
        if let Some(obj_url) = obj.url {
            srcs.push(obj_url);
        }
    }

    if srcs.is_empty() {
        anyhow::bail!("no objects matched {url}");
    }

    // Sort by key so the concatenation order is deterministic regardless of the
    // order in which the listing yielded objects.
    srcs.sort_by(|a, b| a.path.cmp(&b.path));

    for src in &srcs {
        let mut body = s3
            .read(src)
            .await
            .map_err(|e| anyhow::anyhow!("cat {src}: {e:#}"))?;
        while let Some(chunk) = body
            .try_next()
            .await
            .map_err(|e| anyhow::anyhow!("cat {src}: {e:#}"))?
        {
            stdout.write_all(&chunk).await?;
        }
    }
    stdout.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verifies how `run()` classifies a source into the single-object fast
    /// path versus the expand-and-concatenate path. A concrete remote key (and
    /// any local path) takes the fast path; remote wildcards, prefixes and
    /// bucket roots are expanded.
    #[test]
    fn source_classification() {
        let is_expanded = |raw: &str| {
            let u = Url::parse(raw).unwrap();
            u.is_remote() && (u.is_wildcard() || u.is_bucket() || u.is_prefix())
        };

        // Concrete remote object -> fast path.
        assert!(!is_expanded("s3://bucket/key.txt"));
        assert!(!is_expanded("s3://bucket/dir/key.txt"));

        // Expanded remote sources.
        assert!(is_expanded("s3://bucket/dir/*.txt")); // wildcard
        assert!(is_expanded("s3://bucket/dir/")); // prefix
        assert!(is_expanded("s3://bucket")); // bucket root

        // Local paths are never expanded (treated as a single file).
        assert!(!is_expanded("/tmp/file.txt"));
        assert!(!is_expanded("./dir/"));
    }
}
