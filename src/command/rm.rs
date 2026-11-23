//! `rm` — remove objects (local or S3), with wildcard expansion.
//!
//! Supports `--exclude` / `--include` glob filtering. The filters are applied
//! ONLY when a target expands via a wildcard listing; they are matched
//! against each listed object's relative path. A single concrete-object delete
//! (no expansion) is never filtered.

use clap::Args;
use tokio::sync::mpsc;

use super::filters::{filter_key, patterns_with_files, Filters};
use super::GlobalOpts;
use crate::storage::s3::S3;
use crate::storage::url::Url;
use crate::storage::new_client;

#[derive(Args, Debug)]
pub struct RmArgs {
    /// One or more targets (local paths or s3:// URLs), may contain wildcards. A bucket or a
    /// prefix ending in `/` is refused, as in s5cmd: delete everything under it with
    /// `'s3://bucket/prefix/*'`.
    #[arg(required = true)]
    pub targets: Vec<String>,

    /// Delete the given object version id (use with a single concrete target).
    #[arg(long)]
    pub version_id: Option<String>,

    /// Disable wildcard and prefix (trailing-slash) expansion; treat each target
    /// as a single literal key. Lets you delete a directory-marker object such as
    /// `s3://bucket/path/dirobj/` directly. Mirrors upstream s5cmd PR #861.
    #[arg(long)]
    pub raw: bool,

    /// Exclude objects whose relative path matches the given glob (repeatable).
    /// Only applied when a target expands via a wildcard listing.
    #[arg(long)]
    pub exclude: Vec<String>,

    /// Only include objects whose relative path matches the given glob (repeatable).
    /// Only applied when a target expands via a wildcard listing.
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

/// s5cmd's refusal of a bucket or a bare prefix, word for word.
const BARE_PREFIX: &str =
    "s3 bucket/prefix cannot be used for delete operations (forgot wildcard character?)";

/// Whether `url` names a whole bucket or, without `--raw`, every object under a prefix: s5cmd
/// refuses both, since a trailing `/` is easy to type where one object was meant (#30). With
/// `--raw` a key ending in `/` is that one key (a directory marker).
fn refused(url: &Url) -> bool {
    url.is_bucket() || (url.is_prefix() && !url.is_raw())
}

pub async fn run(global: &GlobalOpts, args: RmArgs) -> anyhow::Result<()> {
    let opts = global.storage_options();
    let mut had_error = false;

    // Compile include/exclude filters into regexes once. Inline patterns are
    // combined with any read from `--include-from`/`--exclude-from` files.
    let includes = patterns_with_files(&args.include, &args.include_from)?;
    let excludes = patterns_with_files(&args.exclude, &args.exclude_from)?;
    let filters = Filters::new(&includes, &excludes)?;

    let urls = args
        .targets
        .iter()
        .map(|target| {
            let options = crate::storage::url::UrlOptions {
                raw: args.raw,
                version_id: args.version_id.clone(),
                ..Default::default()
            };
            Url::new(target, options).map_err(|e| anyhow::anyhow!(e))
        })
        .collect::<anyhow::Result<Vec<Url>>>()?;
    // Every target is checked before anything is deleted, dry runs included.
    let bare: Vec<String> = urls.iter().filter(|u| refused(u)).map(Url::to_string).collect();
    if !bare.is_empty() {
        for target in &bare {
            crate::output::op_error("rm", target, None, BARE_PREFIX);
        }
        anyhow::bail!("nothing deleted");
    }

    for url in urls {

        // Collect the concrete object URLs to delete, expanding wildcards. A bare prefix was
        // refused above; with `--raw` there is no expansion (`is_wildcard` is false), so a key
        // ending in `/` (a directory marker) takes the single delete.
        let expand = url.is_wildcard();

        if !expand {
            // Single concrete object: deleted as-is, never filtered.
            let client = new_client(&url, &opts).await?;
            match client.delete(&url).await {
                Ok(()) => crate::output::op_success("rm", &url.to_string(), None),
                Err(e) => {
                    had_error = true;
                    crate::output::op_error("rm", &url.to_string(), None, &format!("{e:#}"));
                }
            }
            continue;
        }

        let client = new_client(&url, &opts).await?;
        let mut rx = client.list(&url, true);

        if url.is_remote() {
            // Batch delete through the S3 multi-delete path.
            let s3 = S3::new(&url, &opts).await?;
            let (tx, urlrx) = mpsc::channel::<Url>(256);
            let mut resultrx = s3.multi_delete(urlrx);

            // The feeder forwards listed object URLs to the delete channel,
            // skipping any that the include/exclude filters reject.
            let filters = filters.clone();
            let feeder = tokio::spawn(async move {
                while let Some(obj) = rx.recv().await {
                    if let Some(u) = obj.url {
                        if filters.should_skip(&filter_key(&u)) {
                            continue;
                        }
                        if tx.send(u).await.is_err() {
                            break;
                        }
                    }
                }
            });

            while let Some(obj) = resultrx.recv().await {
                let key = obj.url.as_ref().map(|u| u.to_string()).unwrap_or_default();
                match obj.err {
                    None => crate::output::op_success("rm", &key, None),
                    Some(e) => {
                        had_error = true;
                        crate::output::op_error("rm", &key, None, &format!("{e:#}"));
                    }
                }
            }
            let _ = feeder.await;
        } else {
            while let Some(obj) = rx.recv().await {
                if let Some(err) = obj.err {
                    had_error = true;
                    crate::output::op_error("rm", "", None, &format!("{err:#}"));
                    continue;
                }
                if let Some(u) = obj.url {
                    // Skip objects rejected by the include/exclude filters.
                    if filters.should_skip(&filter_key(&u)) {
                        continue;
                    }
                    match client.delete(&u).await {
                        Ok(()) => crate::output::op_success("rm", &u.to_string(), None),
                        Err(e) => {
                            had_error = true;
                            crate::output::op_error("rm", &u.to_string(), None, &format!("{e:#}"));
                        }
                    }
                }
            }
        }
    }

    if had_error {
        anyhow::bail!("one or more rm operations failed");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::refused;
    use crate::storage::url::{Url, UrlOptions};

    fn url(s: &str, raw: bool) -> Url {
        Url::new(s, UrlOptions { raw, ..Default::default() }).unwrap()
    }

    #[test]
    fn a_bucket_or_a_bare_prefix_is_refused_as_s5cmd_refuses_it() {
        for s in ["s3://b", "s3://b/", "s3://b/p/", "s3://b/p/q/"] {
            assert!(refused(&url(s, false)), "{s}");
        }
        for s in ["s3://b/p/*", "s3://b/p/k", "s3://b/p*", "local/dir/", "f.txt"] {
            assert!(!refused(&url(s, false)), "{s}");
        }
        // `--raw`: a key ending in `/` is one object (a directory marker); a bucket stays refused.
        assert!(!refused(&url("s3://b/dirobj/", true)));
        assert!(refused(&url("s3://b", true)));
    }

    #[test]
    fn raw_takes_the_single_delete() {
        assert!(!url("s3://b/dirobj/", true).is_wildcard());
        assert!(!url("s3://b/what?.txt", true).is_wildcard());
        assert!(url("s3://b/what?.txt", false).is_wildcard());
    }
}
