//! `du` — show object size usage. Ported from s5cmd's `command/du.go`.
//!
//! Lists every object matching the target (wildcard / prefix / bucket / local
//! path), sums their count and total size, and prints a summary line in the
//! same format as the Go tool's `SizeMessage`:
//!
//! ```text
//! <size> bytes in <N> objects: <source>[ [<storage-class>]]
//! ```
//!
//! Supports `--exclude <glob>` (repeatable) to skip objects whose relative
//! path matches a glob, and `--all-versions` / `--version-id` to size object
//! versions on versioned buckets.

use std::collections::BTreeMap;

use clap::Args;
use regex::Regex;

use super::GlobalOpts;
use crate::storage::new_client;
use crate::storage::url::Url;

#[derive(Args, Debug)]
pub struct DuArgs {
    /// Target to measure (s3:// URL or local path; wildcards/prefixes allowed).
    pub target: Option<String>,

    /// Human-readable output for object sizes.
    #[arg(long, short = 'H')]
    pub humanize: bool,

    /// Group sizes by storage class.
    #[arg(long, short = 'g')]
    pub group: bool,

    /// Exclude objects whose relative path matches the given glob (repeatable).
    #[arg(long)]
    pub exclude: Vec<String>,

    /// Measure all versions of objects (requires a versioned bucket).
    #[arg(long)]
    pub all_versions: bool,

    /// Measure only the given version id of the object.
    #[arg(long)]
    pub version_id: Option<String>,
}

/// Running size and object count for a set of objects.
#[derive(Debug, Default, Clone, Copy)]
struct SizeAndCount {
    size: i64,
    count: i64,
}

impl SizeAndCount {
    fn add(&mut self, size: i64) {
        self.size += size;
        self.count += 1;
    }
}

pub async fn run(global: &GlobalOpts, args: DuArgs) -> anyhow::Result<()> {
    let target = args
        .target
        .clone()
        .ok_or_else(|| anyhow::anyhow!("expected only 1 argument"))?;

    let opts = global.storage_options();
    let url = Url::new(
        &target,
        crate::storage::url::UrlOptions {
            all_versions: args.all_versions,
            version_id: args.version_id.clone(),
            ..Default::default()
        },
    )
    .map_err(|e| anyhow::anyhow!(e))?;

    // Compile exclude globs into anchored regexes once.
    let excludes = compile_globs(&args.exclude)?;

    let client = new_client(&url, &opts).await?;

    let mut total = SizeAndCount::default();
    // BTreeMap keeps the per-class output deterministic.
    let mut by_class: BTreeMap<String, SizeAndCount> = BTreeMap::new();

    let mut rx = client.list(&url, false);
    while let Some(obj) = rx.recv().await {
        if let Some(err) = obj.err {
            // A listing-level failure (e.g. AccessDenied / NoSuchBucket) aborts
            // `du` immediately. For those fatal AWS errors, annotate with a
            // clearer message naming the target; other errors propagate as-is.
            return Err(annotate_fatal(&target, err));
        }
        if obj.typ.is_dir() {
            continue;
        }

        // Skip objects whose relative path matches any exclude glob.
        let key = obj.url.as_ref().map(|u| u.relative()).unwrap_or_default();
        if should_exclude(&excludes, &key) {
            continue;
        }

        total.add(obj.size);
        by_class
            .entry(obj.storage_class.0.clone())
            .or_default()
            .add(obj.size);
    }

    let source = url.to_string();
    let json = crate::output::is_json();

    if !args.group {
        if json {
            crate::output::json_line(size_json(&source, None, total));
        } else {
            crate::outln!("{}", size_message(&source, None, total, args.humanize));
        }
        return Ok(());
    }

    for (class, sc) in &by_class {
        if json {
            crate::output::json_line(size_json(&source, Some(class), *sc));
        } else {
            crate::outln!("{}", size_message(&source, Some(class), *sc, args.humanize));
        }
    }
    Ok(())
}

/// Wraps fatal AWS listing errors (`AccessDenied` / `NoSuchBucket`,
/// case-insensitive) with a clearer message naming the target; non-fatal errors
/// are returned unchanged so existing behavior and messages are preserved.
fn annotate_fatal(target: &str, e: anyhow::Error) -> anyhow::Error {
    let msg = format!("{e:#}").to_ascii_lowercase();
    if msg.contains("accessdenied") || msg.contains("nosuchbucket") {
        anyhow::anyhow!("cannot access {target}: {e:#}")
    } else {
        e
    }
}

/// Compiles wildcard glob strings into anchored regexes, mirroring `sync`'s
/// filter compilation (`wildcard_to_regexp` then `^...$` anchoring).
fn compile_globs(globs: &[String]) -> anyhow::Result<Vec<Regex>> {
    let mut out = Vec::with_capacity(globs.len());
    for g in globs {
        let mut re = crate::strutil::wildcard_to_regexp(g);
        re = crate::strutil::match_from_start_to_end(&re);
        re = crate::strutil::add_newline_flag(&re);
        out.push(Regex::new(&re).map_err(|e| anyhow::anyhow!(e))?);
    }
    Ok(out)
}

/// Returns true if the given relative key matches any of the exclude globs.
fn should_exclude(excludes: &[Regex], key: &str) -> bool {
    excludes.iter().any(|re| re.is_match(key))
}

/// JSON form of a du summary: {"source":..,"count":N,"size":N[,"storage_class":..]}.
fn size_json(source: &str, class: Option<&str>, sc: SizeAndCount) -> serde_json::Value {
    let mut v = serde_json::json!({
        "source": source,
        "count": sc.count,
        "size": sc.size,
    });
    if let Some(c) = class {
        if !c.is_empty() {
            v["storage_class"] = serde_json::Value::String(c.to_string());
        }
    }
    v
}

/// Formats a size in bytes, optionally humanized as s5cmd's `du -H` does it (`humanize_bytes`):
/// the line already says `bytes`, so the humanized size carries no unit of its own (#12).
fn humanize_size(size: i64, humanize: bool) -> String {
    if humanize {
        humanize_bytes(size)
    } else {
        size.to_string()
    }
}

/// s5cmd's `strutil.HumanizeBytes`: below 1024 the bare number, otherwise one decimal and the
/// letter of the largest power of 1024 that fits (`1.2K`, `3.0M`, ...).
fn humanize_bytes(size: i64) -> String {
    const UNIT: i64 = 1024;
    if size < UNIT {
        return size.to_string();
    }
    let (mut div, mut exp) = (UNIT, 0);
    let mut n = size / UNIT;
    while n >= UNIT {
        div *= UNIT;
        exp += 1;
        n /= UNIT;
    }
    format!("{:.1}{}", size as f64 / div as f64, b"KMGTPE"[exp] as char)
}

/// Builds the summary line for a (source, optional storage class) tuple.
fn size_message(
    source: &str,
    storage_class: Option<&str>,
    sc: SizeAndCount,
    humanize: bool,
) -> String {
    let class_suffix = match storage_class {
        Some(c) if !c.is_empty() => format!(" [{c}]"),
        _ => String::new(),
    };
    format!(
        "{} bytes in {} objects: {}{}",
        humanize_size(sc.size, humanize),
        sc.count,
        source,
        class_suffix,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_plain_size() {
        let mut sc = SizeAndCount::default();
        sc.add(100);
        sc.add(200);
        assert_eq!(
            size_message("s3://b/*", None, sc, false),
            "300 bytes in 2 objects: s3://b/*"
        );
    }

    #[test]
    fn formats_with_storage_class() {
        let mut sc = SizeAndCount::default();
        sc.add(1);
        assert_eq!(
            size_message("s3://b/*", Some("STANDARD"), sc, false),
            "1 bytes in 1 objects: s3://b/* [STANDARD]"
        );
    }

    #[test]
    fn humanizes_size_as_s5cmd_with_the_unit_once() {
        let line = |n: i64| {
            let mut sc = SizeAndCount::default();
            sc.add(n);
            size_message("s3://b/*", None, sc, true)
        };
        assert_eq!(line(6), "6 bytes in 1 objects: s3://b/*");
        assert_eq!(line(1024), "1.0K bytes in 1 objects: s3://b/*");
        assert_eq!(line(1260), "1.2K bytes in 1 objects: s3://b/*");
    }

    #[test]
    fn humanize_bytes_matches_s5cmd() {
        for (n, want) in [
            (0, "0"),
            (1023, "1023"),
            (1024, "1.0K"),
            (1536, "1.5K"),
            (1024 * 1024 - 1, "1024.0K"),
            (1024 * 1024, "1.0M"),
            (5 << 30, "5.0G"),
            (3 << 40, "3.0T"),
            (i64::MAX, "8.0E"),
        ] {
            assert_eq!(humanize_bytes(n), want, "{n}");
        }
    }

    #[test]
    fn exclude_glob_matches_relative_keys() {
        let excludes = compile_globs(&["*.txt".to_string(), "logs/*".to_string()]).unwrap();
        assert!(should_exclude(&excludes, "notes.txt"));
        assert!(should_exclude(&excludes, "logs/app.log"));
        assert!(!should_exclude(&excludes, "data.csv"));
        assert!(!should_exclude(&excludes, "archive/notes.txt.bak"));
    }

    #[test]
    fn no_exclude_globs_skips_nothing() {
        let excludes = compile_globs(&[]).unwrap();
        assert!(!should_exclude(&excludes, "anything"));
    }

    #[test]
    fn annotate_fatal_wraps_known_aws_errors() {
        // Fatal AWS errors get the clearer "cannot access" prefix (matched
        // case-insensitively against the Display chain).
        let e = annotate_fatal("s3://b/*", anyhow::anyhow!("AccessDenied: forbidden"));
        assert!(format!("{e:#}").starts_with("cannot access s3://b/*:"), "{e:#}");

        let e = annotate_fatal("s3://b/*", anyhow::anyhow!("the bucket NoSuchBucket"));
        assert!(format!("{e:#}").starts_with("cannot access s3://b/*:"), "{e:#}");

        // Unrelated errors are passed through unchanged.
        let e = annotate_fatal("s3://b/*", anyhow::anyhow!("connection reset"));
        assert_eq!(format!("{e:#}"), "connection reset");
    }

    #[test]
    fn empty_storage_class_has_no_suffix() {
        let mut sc = SizeAndCount::default();
        sc.add(5);
        assert_eq!(
            size_message("s3://b/*", Some(""), sc, false),
            "5 bytes in 1 objects: s3://b/*"
        );
    }
}
