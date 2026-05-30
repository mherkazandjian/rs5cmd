//! Sync comparison strategies — decide whether a source object should be
//! copied over its destination counterpart. Ported from s5cmd's
//! `command/sync_strategy.go`.
//!
//! The Go version returns an `error` from `ShouldSync` (a sentinel error means
//! "skip, they match"). Here we model the decision as a plain boolean: `true`
//! means the object should be (re)copied, `false` means it can be skipped.

use crate::storage::Object;

/// The comparison strategy used to decide whether a common object (present in
/// both source and destination) needs to be synced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncStrategy {
    /// Compare only sizes: sync when sizes differ.
    SizeOnly,
    /// Compare size and modification time (the default). The source is treated
    /// as the source-of-truth:
    ///
    /// | time            | size            | should sync |
    /// |-----------------|-----------------|-------------|
    /// | src >  dst      | src != dst      | yes         |
    /// | src >  dst      | src == dst      | yes         |
    /// | src <= dst      | src != dst      | yes         |
    /// | src <= dst      | src == dst      | no          |
    SizeAndModification,
    /// Compare content checksums (S3 ETag / MD5): sync when they differ. If
    /// either checksum is missing or is a multipart-composite ETag (contains
    /// `-`, which is not a plain MD5 and cannot be compared), the object is
    /// re-copied to stay safe.
    Checksum,
}

impl SyncStrategy {
    /// Constructs the strategy. `checksum` takes precedence over `size_only`,
    /// which takes precedence over the default size+modtime comparison.
    pub fn new(size_only: bool, checksum: bool) -> SyncStrategy {
        if checksum {
            SyncStrategy::Checksum
        } else if size_only {
            SyncStrategy::SizeOnly
        } else {
            SyncStrategy::SizeAndModification
        }
    }

    /// Returns `true` if `src` should be copied over `dst`.
    pub fn should_sync(&self, src: &Object, dst: &Object) -> bool {
        match self {
            SyncStrategy::SizeOnly => src.size != dst.size,
            SyncStrategy::SizeAndModification => {
                // Source is newer than destination -> sync.
                if is_after(src.mod_time, dst.mod_time) {
                    return true;
                }
                // Sizes differ -> sync.
                if src.size != dst.size {
                    return true;
                }
                // Same size and source not newer -> skip.
                false
            }
            SyncStrategy::Checksum => {
                let s = src.etag.trim_matches('"');
                let d = dst.etag.trim_matches('"');
                // Missing or multipart-composite ETags can't be compared as
                // plain MD5s; re-copy to be safe.
                if s.is_empty() || d.is_empty() || s.contains('-') || d.contains('-') {
                    return true;
                }
                s != d
            }
        }
    }
}

/// Whether `a` is strictly after `b`. Unknown timestamps are treated as the
/// Unix epoch, matching the Go zero-time comparison semantics closely enough
/// for the sync decision.
fn is_after(
    a: Option<std::time::SystemTime>,
    b: Option<std::time::SystemTime>,
) -> bool {
    let a = a.unwrap_or(std::time::UNIX_EPOCH);
    let b = b.unwrap_or(std::time::UNIX_EPOCH);
    a > b
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::Object;
    use std::time::{Duration, UNIX_EPOCH};

    fn obj(size: i64, secs: u64) -> Object {
        Object {
            size,
            mod_time: Some(UNIX_EPOCH + Duration::from_secs(secs)),
            ..Default::default()
        }
    }

    fn obj_etag(etag: &str) -> Object {
        Object {
            etag: etag.to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn size_only_syncs_on_size_difference() {
        let s = SyncStrategy::new(true, false);
        assert!(s.should_sync(&obj(100, 10), &obj(200, 10)));
        // Newer source but same size -> no sync under size-only.
        assert!(!s.should_sync(&obj(100, 999), &obj(100, 1)));
    }

    #[test]
    fn default_syncs_when_source_newer() {
        let s = SyncStrategy::new(false, false);
        // Newer source, same size -> sync.
        assert!(s.should_sync(&obj(100, 50), &obj(100, 10)));
    }

    #[test]
    fn default_syncs_when_sizes_differ() {
        let s = SyncStrategy::new(false, false);
        // Older source but different size -> sync.
        assert!(s.should_sync(&obj(100, 1), &obj(200, 50)));
    }

    #[test]
    fn default_skips_when_same_size_and_not_newer() {
        let s = SyncStrategy::new(false, false);
        // Same size, source not newer -> skip.
        assert!(!s.should_sync(&obj(100, 10), &obj(100, 50)));
        assert!(!s.should_sync(&obj(100, 10), &obj(100, 10)));
    }

    #[test]
    fn checksum_compares_etags() {
        let s = SyncStrategy::new(false, true);
        // Equal checksums -> skip; differing -> sync (quotes are tolerated).
        assert!(!s.should_sync(&obj_etag("abc"), &obj_etag("\"abc\"")));
        assert!(s.should_sync(&obj_etag("abc"), &obj_etag("def")));
        // Missing or multipart-composite ETag -> re-copy to be safe.
        assert!(s.should_sync(&obj_etag(""), &obj_etag("abc")));
        assert!(s.should_sync(&obj_etag("abc-2"), &obj_etag("abc-2")));
    }
}
