//! Abstracts local and remote (s3://) file URLs. Ported from s5cmd's
//! `storage/url` package.

use std::path::{Path, PathBuf};

use regex::Regex;

use crate::strutil;

const GLOB_CHARACTERS: &str = "?*";
const S3_SCHEME: &str = "s3://";
const S3_SEPARATOR: &str = "/";
const MATCH_ALL_RE: &str = ".*";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UrlType {
    Remote,
    Local,
}

/// The canonical representation of an object on local or remote storage.
#[derive(Debug, Clone)]
pub struct Url {
    pub kind: UrlType,
    pub scheme: String,
    pub bucket: String,
    pub path: String,
    pub delimiter: String,
    pub prefix: String,
    pub version_id: String,
    pub all_versions: bool,
    /// When set, listing starts after this key (S3 `StartAfter` / V1 `Marker`).
    pub start_after: String,

    relative_path: String,
    filter: String,
    filter_regex: Option<Regex>,
    raw: bool,
}

/// Options applied at construction time (mirrors the Go functional options).
#[derive(Debug, Default, Clone)]
pub struct UrlOptions {
    pub raw: bool,
    pub version_id: Option<String>,
    pub all_versions: bool,
    /// Start listing after this key (maps to S3 `StartAfter` / V1 `Marker`).
    pub start_after: Option<String>,
}

impl Url {
    /// Creates a new URL from the given path string.
    pub fn new(s: &str, opts: UrlOptions) -> Result<Url, String> {
        match s.split_once("://") {
            None => {
                let mut url = Url {
                    kind: UrlType::Local,
                    scheme: String::new(),
                    bucket: String::new(),
                    path: s.to_string(),
                    delimiter: String::new(),
                    prefix: String::new(),
                    version_id: opts.version_id.clone().unwrap_or_default(),
                    all_versions: opts.all_versions,
                    start_after: opts.start_after.clone().unwrap_or_default(),
                    relative_path: String::new(),
                    filter: String::new(),
                    filter_regex: None,
                    raw: opts.raw,
                };
                url.set_prefix_and_filter()?;
                Ok(url)
            }
            Some((scheme, rest)) => {
                if scheme != "s3" {
                    return Err(format!("s3 url should start with {S3_SCHEME:?}"));
                }

                let (bucket, key) = match rest.split_once(S3_SEPARATOR) {
                    Some((b, k)) => (b.to_string(), k.to_string()),
                    None => (rest.to_string(), String::new()),
                };

                if bucket.is_empty() {
                    return Err("s3 url should have a bucket".to_string());
                }
                if has_glob_character(&bucket) {
                    return Err("bucket name cannot contain wildcards".to_string());
                }

                let mut url = Url {
                    kind: UrlType::Remote,
                    scheme: "s3".to_string(),
                    bucket,
                    path: key,
                    delimiter: String::new(),
                    prefix: String::new(),
                    version_id: opts.version_id.clone().unwrap_or_default(),
                    all_versions: opts.all_versions,
                    start_after: opts.start_after.clone().unwrap_or_default(),
                    relative_path: String::new(),
                    filter: String::new(),
                    filter_regex: None,
                    raw: opts.raw,
                };
                url.set_prefix_and_filter()?;
                Ok(url)
            }
        }
    }

    /// Convenience constructor with default options.
    pub fn parse(s: &str) -> Result<Url, String> {
        Url::new(s, UrlOptions::default())
    }

    pub fn is_remote(&self) -> bool {
        self.kind == UrlType::Remote
    }

    /// Whether the remote object is an S3 prefix (ends with `/`).
    pub fn is_prefix(&self) -> bool {
        self.is_remote() && self.path.ends_with('/')
    }

    /// Whether the URL contains only a bucket name.
    pub fn is_bucket(&self) -> bool {
        self.is_remote() && self.path.is_empty()
    }

    pub fn is_versioned(&self) -> bool {
        self.all_versions || !self.version_id.is_empty()
    }

    pub fn is_raw(&self) -> bool {
        self.raw
    }

    /// Whether the path contains any wildcard characters.
    pub fn is_wildcard(&self) -> bool {
        !self.raw && has_glob_character(&self.path)
    }

    /// The absolute URL representation of the object.
    pub fn absolute(&self) -> String {
        if !self.is_remote() {
            return self.path.clone();
        }
        self.remote_url()
    }

    /// A URI reference based on the calculated prefix.
    pub fn relative(&self) -> String {
        if !self.relative_path.is_empty() {
            return self.relative_path.clone();
        }
        self.absolute()
    }

    /// The last element of the object path.
    pub fn base(&self) -> String {
        if self.is_remote() {
            remote_base(&self.path)
        } else {
            Path::new(&self.path)
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| local_base_fallback(&self.path))
        }
    }

    /// All but the last element of the path (the directory).
    pub fn dir(&self) -> String {
        if self.is_remote() {
            remote_dir(&self.path)
        } else {
            Path::new(&self.path)
                .parent()
                .map(|p| {
                    let s = p.to_string_lossy();
                    if s.is_empty() {
                        ".".to_string()
                    } else {
                        s.into_owned()
                    }
                })
                .unwrap_or_else(|| ".".to_string())
        }
    }

    /// Joins a string and returns a new URL.
    pub fn join(&self, s: &str) -> Url {
        let mut clone = self.clone();
        if !clone.is_remote() {
            // Local: clean the path, removing adjacent slashes.
            clone.path = path_join(&clone.path, s);
        } else {
            // Remote: keep as-is, allowing adjacent slashes.
            clone.path = format!("{}{}", clone.path, s);
        }
        clone
    }

    fn remote_url(&self) -> String {
        let mut s = format!("{}://", self.scheme);
        if !self.bucket.is_empty() {
            s.push_str(&self.bucket);
        }
        if !self.path.is_empty() {
            s.push('/');
            s.push_str(&self.path);
        }
        s
    }

    /// Creates url metadata for both wildcard and non-wildcard operations,
    /// pre-compiling the filter regex.
    fn set_prefix_and_filter(&mut self) -> Result<(), String> {
        if self.raw {
            return Ok(());
        }

        match self.path.find(|c| GLOB_CHARACTERS.contains(c)) {
            None => {
                self.delimiter = S3_SEPARATOR.to_string();
                self.prefix = self.path.clone();
            }
            Some(loc) => {
                self.prefix = self.path[..loc].to_string();
                self.filter = self.path[loc..].to_string();
            }
        }

        let mut filter_regex = if self.filter.is_empty() {
            MATCH_ALL_RE.to_string()
        } else {
            strutil::wildcard_to_regexp(&self.filter)
        };
        filter_regex = format!("{}{}", regex::escape(&self.prefix), filter_regex);
        filter_regex = strutil::match_from_start_to_end(&filter_regex);
        filter_regex = strutil::add_newline_flag(&filter_regex);
        let r = Regex::new(&filter_regex).map_err(|e| e.to_string())?;
        self.filter_regex = Some(r);
        Ok(())
    }

    /// Explicitly sets the relative path of `self` against the given base.
    pub fn set_relative(&mut self, base: &Url) {
        let mut base_path = base.absolute();
        if base.is_wildcard() {
            if let Some(loc) = base_path.find(|c| GLOB_CHARACTERS.contains(c)) {
                base_path = base_path[..loc].to_string();
            }
        }
        let base_dir = go_dir(&base_path);
        let abs = self.absolute();
        self.relative_path = rel_path(Path::new(&base_dir), Path::new(&abs))
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
    }

    /// Whether the given key matches the object's filter; updates relative path.
    pub fn matches(&mut self, key: &str) -> bool {
        let Some(re) = &self.filter_regex else {
            return false;
        };
        if !re.is_match(key) {
            return false;
        }
        let is_batch = !self.filter.is_empty();
        let v = if is_batch {
            parse_batch(&self.prefix, key)
        } else {
            parse_non_batch(&self.prefix, key)
        };
        self.relative_path = v;
        true
    }

    /// Percent-escapes each path element (used when building source keys).
    pub fn escaped_path(&self) -> String {
        let source_key = self
            .absolute()
            .strip_prefix("s3://")
            .map(|s| s.to_string())
            .unwrap_or_else(|| self.absolute());
        source_key
            .split('/')
            .map(query_escape)
            .collect::<Vec<_>>()
            .join("/")
    }
}

impl std::fmt::Display for Url {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.absolute())
    }
}

fn has_glob_character(s: &str) -> bool {
    s.contains(|c| GLOB_CHARACTERS.contains(c))
}

/// Parses keys for wildcard operations, cutting from the first directory
/// before the wildcard part.
fn parse_batch(prefix: &str, key: &str) -> String {
    let Some(index) = prefix.rfind(S3_SEPARATOR) else {
        return key.to_string();
    };
    if !key.starts_with(prefix) {
        return key.to_string();
    }
    let trimmed = &key[index..];
    trimmed
        .strip_prefix(S3_SEPARATOR)
        .unwrap_or(trimmed)
        .to_string()
}

/// Parses keys for non-wildcard operations.
fn parse_non_batch(prefix: &str, key: &str) -> String {
    // Relativize a key that exactly equals the prefix the same way as its
    // siblings (drop the upstream `key == prefix` special case that returned
    // the absolute key, giving inconsistent ls output — upstream #755).
    if !key.starts_with(prefix) {
        return key.to_string();
    }
    let parsed_key = key.strip_suffix(S3_SEPARATOR).unwrap_or(key);
    match parsed_key.rfind(S3_SEPARATOR) {
        Some(loc) if loc < prefix.len() => {
            let parsed = &key[loc..];
            parsed.strip_prefix(S3_SEPARATOR).unwrap_or(parsed).to_string()
        }
        None => key.to_string(),
        Some(_) => {
            let stripped = key.strip_prefix(prefix).unwrap_or(key);
            let stripped = stripped.strip_prefix(S3_SEPARATOR).unwrap_or(stripped);
            match stripped.find(S3_SEPARATOR) {
                Some(i) => {
                    let index = i + 1;
                    if index >= stripped.len() {
                        stripped.to_string()
                    } else {
                        stripped[..index].to_string()
                    }
                }
                None => stripped.to_string(),
            }
        }
    }
}

// --- path helpers mirroring Go's path/filepath semantics on Unix ---

fn remote_base(p: &str) -> String {
    if p.is_empty() {
        return ".".to_string();
    }
    let trimmed = p.trim_end_matches('/');
    if trimmed.is_empty() {
        return "/".to_string();
    }
    match trimmed.rfind('/') {
        Some(i) => trimmed[i + 1..].to_string(),
        None => trimmed.to_string(),
    }
}

/// Mirrors Go's `filepath.Dir` on Unix: drops the final path element (the
/// trailing run of non-slash chars) and cleans the remainder. Notably,
/// `go_dir("/tmp/data/")` is `/tmp/data`, not `/tmp`.
fn go_dir(p: &str) -> String {
    let b = p.as_bytes();
    if b.is_empty() {
        return ".".to_string();
    }
    let mut i = b.len() as isize - 1;
    while i >= 0 && b[i as usize] != b'/' {
        i -= 1;
    }
    let end = (i + 1).max(0) as usize;
    clean_path(&p[..end])
}

fn remote_dir(p: &str) -> String {
    match p.rfind('/') {
        None => ".".to_string(),
        Some(0) => "/".to_string(),
        Some(i) => p[..i].to_string(),
    }
}

fn local_base_fallback(p: &str) -> String {
    if p.is_empty() {
        ".".to_string()
    } else {
        p.to_string()
    }
}

/// Mirrors Go's `path.Join` for two elements (clean, slash-separated).
fn path_join(a: &str, b: &str) -> String {
    let joined = if a.is_empty() {
        b.to_string()
    } else if b.is_empty() {
        a.to_string()
    } else {
        format!("{}/{}", a.trim_end_matches('/'), b)
    };
    clean_path(&joined)
}

/// A minimal lexical path cleaner (Go's `path.Clean` for forward slashes).
fn clean_path(p: &str) -> String {
    if p.is_empty() {
        return ".".to_string();
    }
    let rooted = p.starts_with('/');
    let mut out: Vec<&str> = Vec::new();
    for seg in p.split('/') {
        match seg {
            "" | "." => continue,
            ".." => {
                if let Some(last) = out.last() {
                    if *last != ".." {
                        out.pop();
                        continue;
                    }
                }
                if !rooted {
                    out.push("..");
                }
            }
            s => out.push(s),
        }
    }
    let mut result = out.join("/");
    if rooted {
        result = format!("/{result}");
    }
    if result.is_empty() {
        ".".to_string()
    } else {
        result
    }
}

/// Mirrors Go's `filepath.Rel` for the cases s5cmd relies on.
fn rel_path(base: &Path, target: &Path) -> Option<PathBuf> {
    let base = clean_path(&base.to_string_lossy());
    let target = clean_path(&target.to_string_lossy());
    if base == target {
        return Some(PathBuf::from("."));
    }
    let base_segs: Vec<&str> = base.split('/').filter(|s| !s.is_empty()).collect();
    let target_segs: Vec<&str> = target.split('/').filter(|s| !s.is_empty()).collect();

    let mut i = 0;
    while i < base_segs.len() && i < target_segs.len() && base_segs[i] == target_segs[i] {
        i += 1;
    }
    let ups = base_segs.len() - i;
    let mut parts: Vec<String> = Vec::new();
    for _ in 0..ups {
        parts.push("..".to_string());
    }
    parts.extend(target_segs[i..].iter().map(|s| s.to_string()));
    if parts.is_empty() {
        Some(PathBuf::from("."))
    } else {
        Some(PathBuf::from(parts.join("/")))
    }
}

/// Mirrors Go's `url.QueryEscape`.
fn query_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_local() {
        let u = Url::parse("/tmp/foo/bar.txt").unwrap();
        assert!(!u.is_remote());
        assert_eq!(u.path, "/tmp/foo/bar.txt");
        assert_eq!(u.base(), "bar.txt");
        assert_eq!(u.dir(), "/tmp/foo");
    }

    #[test]
    fn parses_remote() {
        let u = Url::parse("s3://mybucket/a/b/c.txt").unwrap();
        assert!(u.is_remote());
        assert_eq!(u.bucket, "mybucket");
        assert_eq!(u.path, "a/b/c.txt");
        assert_eq!(u.base(), "c.txt");
        assert_eq!(u.absolute(), "s3://mybucket/a/b/c.txt");
    }

    #[test]
    fn rejects_non_s3_scheme() {
        assert!(Url::parse("gs://bucket/key").is_err());
    }

    #[test]
    fn rejects_empty_bucket() {
        assert!(Url::parse("s3:///key").is_err());
    }

    #[test]
    fn rejects_wildcard_bucket() {
        assert!(Url::parse("s3://buck*et/key").is_err());
    }

    #[test]
    fn bucket_only() {
        let u = Url::parse("s3://mybucket").unwrap();
        assert!(u.is_bucket());
        assert_eq!(u.bucket, "mybucket");
    }

    #[test]
    fn prefix_and_delimiter_for_plain_key() {
        let u = Url::parse("s3://b/a/b/c").unwrap();
        assert_eq!(u.prefix, "a/b/c");
        assert_eq!(u.delimiter, "/");
        assert!(!u.is_wildcard());
    }

    #[test]
    fn wildcard_sets_filter() {
        let u = Url::parse("s3://b/a/b/test?/c/*.tsv").unwrap();
        assert_eq!(u.prefix, "a/b/test");
        assert!(u.is_wildcard());
        assert!(u.delimiter.is_empty());
    }

    #[test]
    fn match_wildcard_key() {
        let mut u = Url::parse("s3://b/a/b/test?/c/*.tsv").unwrap();
        assert!(u.matches("a/b/test2/c/example_file.tsv"));
        assert_eq!(u.relative(), "test2/c/example_file.tsv");
        assert!(!u.matches("a/b/nope/x.csv"));
    }

    #[test]
    fn parse_non_batch_relativizes_key_equal_to_prefix() {
        // An object whose key equals the prefix must relativize like its
        // siblings (to its basename), not be returned as the full un-relativized
        // key (upstream #755). The prefix "a/b" is NOT slash-terminated — that
        // is the case that reproduces the bug.
        let mut equal = Url::parse("s3://bucket/a/b").unwrap();
        assert!(equal.matches("a/b"));
        assert_eq!(equal.relative(), "b");
        // Regression guard: must NOT be the old absolute/un-relativized value.
        assert_ne!(equal.relative(), "a/b");

        // A sibling under the same prefix relativizes to its basename, and the
        // equal-prefix object now matches that consistent style.
        let mut sibling = Url::parse("s3://bucket/a/b").unwrap();
        assert!(sibling.matches("a/b/file1"));
        assert_eq!(sibling.relative(), "file1");
    }

    #[test]
    fn join_remote_keeps_slashes() {
        let u = Url::parse("s3://b/prefix/").unwrap();
        let j = u.join("sub/file");
        assert_eq!(j.path, "prefix/sub/file");
    }

    #[test]
    fn set_relative_against_wildcard_base() {
        // `cp /tmp/data/*.txt ...` — the matched file's relative path should be
        // computed against the wildcard's parent dir, yielding just "1.txt".
        let base = Url::parse("/tmp/data/*.txt").unwrap();
        let mut matched = Url::parse("/tmp/data/1.txt").unwrap();
        matched.set_relative(&base);
        assert_eq!(matched.relative(), "1.txt");
    }

    #[test]
    fn set_relative_against_dir_base() {
        // Recursive dir copy preserves the directory name in the relative path.
        let base = Url::parse("/tmp/data").unwrap();
        let mut matched = Url::parse("/tmp/data/sub/3.txt").unwrap();
        matched.set_relative(&base);
        assert_eq!(matched.relative(), "data/sub/3.txt");
    }

    #[test]
    fn join_local_cleans() {
        let u = Url::parse("/tmp/a").unwrap();
        let j = u.join("b/c");
        assert_eq!(j.path, "/tmp/a/b/c");
    }
}
