//! Shared include/exclude glob filtering used by `rm`, `sync`, `ls`, and
//! `cp`/`mv`.
//!
//! Patterns are compiled once into anchored regexes and matched against an
//! object's relative path (with OS separators normalised to forward slashes).
//! Excludes always win; when any includes are present a key must match at least
//! one. Inline patterns can be combined with patterns read from
//! `--include-from` / `--exclude-from` files.

use regex::Regex;

use crate::storage::url::Url;

/// Compiled include/exclude glob filters, matched against relative paths.
#[derive(Clone)]
pub struct Filters {
    includes: Vec<Regex>,
    excludes: Vec<Regex>,
}

impl Filters {
    pub fn new(includes: &[String], excludes: &[String]) -> anyhow::Result<Filters> {
        Ok(Filters {
            includes: compile_globs(includes)?,
            excludes: compile_globs(excludes)?,
        })
    }

    /// Returns true if an object with the given relative key should be skipped.
    pub fn should_skip(&self, key: &str) -> bool {
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

/// Derives the path used for include/exclude matching from a listed object URL.
/// Matches against the object's relative path (with OS separators normalised to
/// forward slashes).
pub fn filter_key(u: &Url) -> String {
    to_slash(&u.relative())
}

/// Returns the inline patterns followed by any read from the given files (one
/// pattern per line; blank lines and lines starting with `#` are ignored).
pub fn patterns_with_files(inline: &[String], files: &[String]) -> anyhow::Result<Vec<String>> {
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

/// Compiles wildcard glob strings into anchored regexes.
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
}
