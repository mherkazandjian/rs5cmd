//! `rm` — remove objects (local or S3), with wildcard expansion.
//!
//! Supports `--exclude` / `--include` glob filtering. The filters are applied
//! ONLY when a target expands via wildcard/prefix listing; they are matched
//! against each listed object's relative path. A single concrete-object delete
//! (no expansion) is never filtered.

use clap::Args;
use regex::Regex;
use tokio::sync::mpsc;

use super::GlobalOpts;
use crate::storage::s3::S3;
use crate::storage::url::Url;
use crate::storage::new_client;

#[derive(Args, Debug)]
pub struct RmArgs {
    /// One or more targets (local paths or s3:// URLs), may contain wildcards.
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
    /// Only applied when a target expands via wildcard/prefix listing.
    #[arg(long)]
    pub exclude: Vec<String>,

    /// Only include objects whose relative path matches the given glob (repeatable).
    /// Only applied when a target expands via wildcard/prefix listing.
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

pub async fn run(global: &GlobalOpts, args: RmArgs) -> anyhow::Result<()> {
    let opts = global.storage_options();
    let mut had_error = false;

    // Compile include/exclude filters into regexes once. Inline patterns are
    // combined with any read from `--include-from`/`--exclude-from` files.
    let includes = patterns_with_files(&args.include, &args.include_from)?;
    let excludes = patterns_with_files(&args.exclude, &args.exclude_from)?;
    let filters = Filters::new(&includes, &excludes)?;

    for target in &args.targets {
        let url = Url::new(
            target,
            crate::storage::url::UrlOptions {
                raw: args.raw,
                version_id: args.version_id.clone(),
                ..Default::default()
            },
        )
        .map_err(|e| anyhow::anyhow!(e))?;

        // Collect the concrete object URLs to delete (expanding wildcards/prefixes).
        // `--raw` forces literal single-key handling: no wildcard, no prefix
        // expansion. `is_wildcard()` is already false when raw, but `is_prefix()`
        // only checks the trailing slash, so guard it explicitly so a dir-marker
        // key (e.g. `s3://b/dirobj/`) routes to the single delete path.
        let expand = !url.is_raw() && (url.is_wildcard() || (url.is_remote() && url.is_prefix()));

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

/// Derives the path used for include/exclude matching from a listed object URL.
/// Mirrors `sync.rs`, which matches against the object's relative path (with OS
/// separators normalised to forward slashes).
fn filter_key(u: &Url) -> String {
    to_slash(&u.relative())
}

/// Compiled include/exclude glob filters, matched against relative paths.
///
/// This is a small private copy of the equivalent helper in `sync.rs`: excludes
/// always win, and when any includes are present a key must match at least one.
#[derive(Clone)]
struct Filters {
    includes: Vec<Regex>,
    excludes: Vec<Regex>,
}

impl Filters {
    fn new(includes: &[String], excludes: &[String]) -> anyhow::Result<Filters> {
        Ok(Filters {
            includes: compile_globs(includes)?,
            excludes: compile_globs(excludes)?,
        })
    }

    /// Returns true if an object with the given relative key should be skipped.
    fn should_skip(&self, key: &str) -> bool {
        // Excluded patterns win.
        if self.excludes.iter().any(|re| re.is_match(key)) {
            return true;
        }
        // If includes are present, the key must match at least one.
        if !self.includes.is_empty() && !self.includes.iter().any(|re| re.is_match(key)) {
            return true;
        }
        false
    }
}

/// Compiles wildcard glob strings into anchored regexes.
/// Returns the inline patterns followed by any read from the given files (one
/// pattern per line; blank lines and lines starting with `#` are ignored).
fn patterns_with_files(inline: &[String], files: &[String]) -> anyhow::Result<Vec<String>> {
    let mut out = inline.to_vec();
    for f in files {
        let content = std::fs::read_to_string(f)
            .map_err(|e| anyhow::anyhow!("reading pattern file {f}: {e}"))?;
        for line in content.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            out.push(line.to_string());
        }
    }
    Ok(out)
}

fn compile_globs(patterns: &[String]) -> anyhow::Result<Vec<Regex>> {
    let mut out = Vec::with_capacity(patterns.len());
    for p in patterns {
        let mut re = crate::strutil::wildcard_to_regexp(p);
        re = crate::strutil::match_from_start_to_end(&re);
        re = crate::strutil::add_newline_flag(&re);
        out.push(Regex::new(&re)?);
    }
    Ok(out)
}

/// Converts OS path separators to forward slashes for stable matching, matching
/// Go's `filepath.ToSlash`.
fn to_slash(s: &str) -> String {
    if std::path::MAIN_SEPARATOR == '/' {
        s.to_string()
    } else {
        s.replace(std::path::MAIN_SEPARATOR, "/")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filters_exclude_wins() {
        let f = Filters::new(&[], &["*.txt".to_string()]).unwrap();
        assert!(f.should_skip("a.txt"));
        assert!(!f.should_skip("a.csv"));
    }

    #[test]
    fn filters_include_requires_match() {
        let f = Filters::new(&["*.csv".to_string()], &[]).unwrap();
        assert!(!f.should_skip("a.csv"));
        assert!(f.should_skip("a.txt"));
    }

    #[test]
    fn filters_exclude_beats_include() {
        // A key matching both an include and an exclude is still skipped.
        let f = Filters::new(&["data/*".to_string()], &["*.tmp".to_string()]).unwrap();
        assert!(f.should_skip("data/x.tmp"));
        assert!(!f.should_skip("data/x.log"));
        // Outside the include set is also skipped.
        assert!(f.should_skip("other/x.log"));
    }

    #[test]
    fn filters_empty_keeps_everything() {
        let f = Filters::new(&[], &[]).unwrap();
        assert!(!f.should_skip("anything/at/all.bin"));
    }

    #[test]
    fn raw_routes_dir_marker_to_single_delete() {
        use crate::storage::url::{Url, UrlOptions};
        // Non-raw: a trailing-slash key is a prefix and takes the (child-listing)
        // expansion path, so the marker object itself is never deleted.
        let plain = Url::new("s3://b/dirobj/", UrlOptions::default()).unwrap();
        assert!(plain.is_prefix());
        let plain_expand = plain.is_wildcard() || (plain.is_remote() && plain.is_prefix());
        assert!(plain_expand, "non-raw dir-marker takes the prefix/list path");

        // Raw: the fixed predicate from rm.rs:69 must route to the single delete.
        let raw = Url::new(
            "s3://b/dirobj/",
            UrlOptions { raw: true, ..Default::default() },
        ).unwrap();
        assert!(raw.is_raw());
        let raw_expand = !raw.is_raw()
            && (raw.is_wildcard() || (raw.is_remote() && raw.is_prefix()));
        assert!(!raw_expand, "raw dir-marker must use client.delete single path");
    }
}
