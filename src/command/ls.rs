//! `ls` — list buckets and objects.

use std::time::{Duration, SystemTime};

use clap::Args;
use time::format_description::FormatItem;
use time::macros::format_description;
use time::{OffsetDateTime, UtcOffset};

use super::filters::{filter_key, patterns_with_files, Filters};
use super::GlobalOpts;
use crate::storage::url::Url;
use crate::storage::{new_client, ObjectType};

const DATE_FORMAT: &[FormatItem<'static>] =
    format_description!("[year]/[month]/[day] [hour]:[minute]:[second]");

/// Date format used by `--local-time`: the same as [`DATE_FORMAT`] but with the
/// numeric UTC offset appended (e.g. ` +0200`) so the rendered timestamp is
/// unambiguous and visibly distinct from the default UTC output.
const DATE_FORMAT_OFFSET: &[FormatItem<'static>] = format_description!(
    "[year]/[month]/[day] [hour]:[minute]:[second] [offset_hour sign:mandatory][offset_minute]"
);

/// Resolves the system's local UTC offset without relying on the `time` crate's
/// `local-offset` feature (which would pull extra, possibly-uncached deps).
///
/// We read the offset straight from libc's `localtime_r`, which the binary
/// already links. `tm_gmtoff` is the east-of-UTC offset in seconds for the
/// given instant, so it correctly accounts for whatever DST was in effect at
/// that time.
///
/// Returns `None` if the offset cannot be determined (in which case callers
/// fall back to UTC), keeping the feature best-effort and never panicking.
fn local_utc_offset(t: SystemTime) -> Option<UtcOffset> {
    // Seconds since the Unix epoch for `t`. Negative (pre-1970) times are
    // unusual for object timestamps; clamp to 0 to stay safe.
    let secs: i64 = t
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    // Minimal `struct tm` mirror matching the Linux/glibc C ABI used by the
    // project's Docker image. We only read `tm_gmtoff`.
    #[repr(C)]
    struct Tm {
        tm_sec: libc_int,
        tm_min: libc_int,
        tm_hour: libc_int,
        tm_mday: libc_int,
        tm_mon: libc_int,
        tm_year: libc_int,
        tm_wday: libc_int,
        tm_yday: libc_int,
        tm_isdst: libc_int,
        tm_gmtoff: libc_long,
        tm_zone: *const u8,
    }
    #[allow(non_camel_case_types)]
    type libc_int = i32;
    #[allow(non_camel_case_types)]
    type libc_long = i64;
    extern "C" {
        fn tzset();
        fn localtime_r(timep: *const i64, result: *mut Tm) -> *mut Tm;
    }

    // SAFETY: `localtime_r` is the thread-safe variant (it writes into the
    // caller-provided `tm`, with no shared static buffer). We pass valid
    // pointers to a zeroed `tm` and to `secs`. `tzset()` initialises the
    // timezone from the environment (TZ / /etc/localtime) before the call.
    unsafe {
        tzset();
        let time_t: i64 = secs;
        let mut tm: Tm = std::mem::zeroed();
        let res = localtime_r(&time_t as *const i64, &mut tm as *mut Tm);
        if res.is_null() {
            return None;
        }
        UtcOffset::from_whole_seconds(tm.tm_gmtoff as i32).ok()
    }
}

#[derive(Args, Debug)]
pub struct LsArgs {
    /// Bucket or object URL. Omit to list all buckets.
    pub target: Option<String>,

    /// Human-readable object sizes.
    #[arg(long, short = 'H')]
    pub humanize: bool,

    /// Show storage class.
    #[arg(long)]
    pub storage_class: bool,

    /// Show ETag.
    #[arg(long, short = 'e')]
    pub etag: bool,

    /// List all versions of objects (requires a versioned bucket).
    #[arg(long)]
    pub all_versions: bool,

    /// List only the given version id of the object.
    #[arg(long)]
    pub version_id: Option<String>,

    /// Print a summary footer with the total object count and size.
    #[arg(long)]
    pub summarize: bool,

    /// Print only each object's full s3:// path (one per line), suppressing the
    /// date/size/etag columns. Convenient for piping into `xargs`/`run`.
    #[arg(long)]
    pub show_fullpath: bool,

    /// Start listing after this key (S3 `StartAfter`); useful to resume a
    /// listing or page through a large bucket.
    #[arg(long)]
    pub start_after: Option<String>,

    /// Only list objects modified more recently than this. Accepts either an
    /// RFC3339 timestamp (e.g. `2024-01-02T15:04:05Z`) or a relative duration
    /// from now with an `s`/`m`/`h`/`d` suffix (e.g. `24h`, `7d`, `30m`).
    #[arg(long)]
    pub newer_than: Option<String>,

    /// Only list objects modified before this. Accepts either an RFC3339
    /// timestamp (e.g. `2024-01-02T15:04:05Z`) or a relative duration from now
    /// with an `s`/`m`/`h`/`d` suffix (e.g. `24h`, `7d`, `30m`).
    #[arg(long)]
    pub older_than: Option<String>,

    /// Render timestamps in the system local time zone instead of UTC. A numeric
    /// offset (e.g. `+0200`) is appended so the value is unambiguous. The
    /// default (without this flag) is unchanged UTC output.
    #[arg(long)]
    pub local_time: bool,

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
}

/// Parses a `--newer-than` / `--older-than` value into an absolute
/// [`SystemTime`] bound.
///
/// Two forms are accepted:
///   * a relative duration like `24h`, `7d`, `30m`, `45s` — interpreted as
///     "that long ago", i.e. `now - duration`.
///   * an RFC3339 timestamp such as `2024-01-02T15:04:05Z`.
fn parse_time_bound(spec: &str, now: SystemTime) -> anyhow::Result<SystemTime> {
    let s = spec.trim();
    if s.is_empty() {
        anyhow::bail!("empty time value");
    }
    if let Some(dur) = parse_relative_duration(s)? {
        return now
            .checked_sub(dur)
            .ok_or_else(|| anyhow::anyhow!("duration `{s}` is too far in the past"));
    }
    // Otherwise treat it as an RFC3339 timestamp.
    let odt = OffsetDateTime::parse(s, &time::format_description::well_known::Rfc3339)
        .map_err(|e| anyhow::anyhow!("invalid time `{s}`: expected RFC3339 or a duration like 24h/7d/30m/45s ({e})"))?;
    Ok(odt.into())
}

/// Parses a single-suffix relative duration (`<number><s|m|h|d>`).
///
/// Returns `Ok(None)` if `s` does not end in a recognised suffix (so the caller
/// can fall back to RFC3339 parsing). Returns `Err` if it looks like a duration
/// (recognised suffix) but the numeric part is invalid.
fn parse_relative_duration(s: &str) -> anyhow::Result<Option<Duration>> {
    let bytes = s.as_bytes();
    let suffix = match bytes.last() {
        Some(c) => *c as char,
        None => return Ok(None),
    };
    let unit_secs: u64 = match suffix {
        's' => 1,
        'm' => 60,
        'h' => 60 * 60,
        'd' => 60 * 60 * 24,
        _ => return Ok(None),
    };
    let num = &s[..s.len() - 1];
    if num.is_empty() || !num.bytes().all(|b| b.is_ascii_digit()) {
        return Ok(None);
    }
    let n: u64 = num
        .parse()
        .map_err(|_| anyhow::anyhow!("invalid duration `{s}`"))?;
    Ok(Some(Duration::from_secs(n.saturating_mul(unit_secs))))
}

pub async fn run(global: &GlobalOpts, args: LsArgs) -> anyhow::Result<()> {
    let opts = global.storage_options();

    // No target, or `s3://` with no bucket: list buckets.
    let target = match &args.target {
        None => {
            return list_buckets(global, "", args.local_time).await;
        }
        Some(t) => t.clone(),
    };

    let url = Url::new(
        &target,
        crate::storage::url::UrlOptions {
            all_versions: args.all_versions,
            version_id: args.version_id.clone(),
            start_after: args.start_after.clone(),
            ..Default::default()
        },
    )
    .map_err(|e| anyhow::anyhow!(e))?;

    if url.is_remote() && url.is_bucket() && !target.ends_with('/') {
        // `ls s3://bucket` lists the bucket contents (prefix == "").
    }

    // Parse the optional LastModified bounds once, up front, so a malformed
    // value fails fast before any listing request goes out.
    let now = SystemTime::now();
    let newer_than = match &args.newer_than {
        Some(s) => Some(parse_time_bound(s, now)?),
        None => None,
    };
    let older_than = match &args.older_than {
        Some(s) => Some(parse_time_bound(s, now)?),
        None => None,
    };

    // Compile include/exclude filters into regexes once. Inline patterns are
    // combined with any read from `--include-from`/`--exclude-from` files.
    let includes = patterns_with_files(&args.include, &args.include_from)?;
    let excludes = patterns_with_files(&args.exclude, &args.exclude_from)?;
    let filters = Filters::new(&includes, &excludes)?;

    let client = new_client(&url, &opts).await?;
    let mut rx = client.list(&url, true);

    // Running totals for the optional `--summarize` footer. Only non-directory
    // objects contribute to the count and byte total.
    let mut total_objects: u64 = 0;
    let mut total_size: i64 = 0;

    while let Some(obj) = rx.recv().await {
        if let Some(err) = obj.err {
            return Err(err);
        }
        // Include/exclude glob filter: skip any non-directory object whose
        // relative key is rejected. Directory / common-prefix entries are not
        // filtered (they carry no real key to match and are display-only).
        if !obj.typ.is_dir() {
            if let Some(u) = &obj.url {
                if filters.should_skip(&filter_key(u)) {
                    continue;
                }
            }
        }
        // Client-side LastModified filter: keep objects in [newer_than, older_than).
        // Entries without a mod_time (directories / common prefixes) are skipped
        // only when a time filter is active, mirroring upstream s5cmd #388.
        if newer_than.is_some() || older_than.is_some() {
            match obj.mod_time {
                Some(t) => {
                    if let Some(lo) = newer_than {
                        if t < lo {
                            continue;
                        }
                    }
                    if let Some(hi) = older_than {
                        if t >= hi {
                            continue;
                        }
                    }
                }
                None => continue,
            }
        }
        if args.summarize && !obj.typ.is_dir() {
            total_objects += 1;
            total_size += obj.size;
        }
        if crate::output::is_json() {
            crate::output::json_line(object_json(&obj, args.local_time));
        } else if args.show_fullpath {
            // Only the absolute s3:// path, one per line (no columns).
            if let Some(u) = &obj.url {
                println!("{}", u.absolute());
            }
        } else {
            println!("{}", format_object(&obj, &args));
        }
    }

    if args.summarize {
        if crate::output::is_json() {
            crate::output::json_line(serde_json::json!({
                "summary": true,
                "total_objects": total_objects,
                "total_size": total_size,
            }));
        } else {
            println!("{}", format_summary(total_objects, total_size, args.humanize));
        }
    }

    Ok(())
}

/// Formats the `--summarize` footer for text output.
///
/// Produces two lines: the number of (non-directory) objects and their combined
/// size. The size honours `--humanize` using the same binary-unit convention as
/// `format_object` and `du` (`humansize::BINARY`).
fn format_summary(total_objects: u64, total_size: i64, humanize: bool) -> String {
    let size = if humanize {
        humansize::format_size(total_size.max(0) as u64, humansize::BINARY)
    } else {
        total_size.to_string()
    };
    format!("Total objects: {total_objects}\nTotal size: {size}")
}

/// Builds the JSON representation of a listed object.
fn object_json(obj: &crate::storage::Object, local: bool) -> serde_json::Value {
    let key = obj.url.as_ref().map(|u| u.relative()).unwrap_or_default();
    let typ = if obj.is_delete_marker {
        "delete_marker"
    } else if obj.typ.is_dir() {
        "directory"
    } else {
        "file"
    };
    let mut v = serde_json::json!({ "type": typ, "key": key });
    if obj.is_delete_marker {
        // A delete marker is a versioned tombstone: no size/etag/storage class.
        v["delete_marker"] = serde_json::Value::Bool(true);
        if let Some(t) = obj.mod_time {
            v["last_modified"] = serde_json::Value::String(format_time(t, local));
        }
        if let Some(u) = &obj.url {
            if !u.version_id.is_empty() {
                v["version_id"] = serde_json::Value::String(u.version_id.clone());
            }
        }
        return v;
    }
    if !obj.typ.is_dir() {
        v["size"] = serde_json::json!(obj.size);
        if !obj.etag.is_empty() {
            v["etag"] = serde_json::Value::String(obj.etag.clone());
        }
        if !obj.storage_class.0.is_empty() {
            v["storage_class"] = serde_json::Value::String(obj.storage_class.0.clone());
        }
        if let Some(t) = obj.mod_time {
            v["last_modified"] = serde_json::Value::String(format_time(t, local));
        }
    }
    if let Some(u) = &obj.url {
        if !u.version_id.is_empty() {
            v["version_id"] = serde_json::Value::String(u.version_id.clone());
        }
    }
    v
}

async fn list_buckets(global: &GlobalOpts, prefix: &str, local: bool) -> anyhow::Result<()> {
    let opts = global.storage_options();
    // Buckets require an S3 client; construct one against a dummy bucket URL.
    let url = Url::parse("s3://_").map_err(|e| anyhow::anyhow!(e))?;
    let s3 = crate::storage::s3::S3::new(&url, &opts).await?;
    let buckets = s3.list_buckets(prefix).await?;
    // With `--local-time` the date column carries the extra ` +HHMM` offset
    // token, so the empty placeholder widens to match. UTC (default) output
    // keeps the historical 19-char placeholder, unchanged.
    let placeholder = if local { 25 } else { 19 };
    for b in buckets {
        if crate::output::is_json() {
            let mut v = serde_json::json!({ "name": format!("s3://{}", b.name) });
            if let Some(t) = b.creation_date {
                v["created_at"] = serde_json::Value::String(format_time(t, local));
            }
            crate::output::json_line(v);
            continue;
        }
        let date = b
            .creation_date
            .map(|t| format_time(t, local))
            .unwrap_or_else(|| " ".repeat(placeholder));
        println!("{date}  s3://{}", b.name);
    }
    Ok(())
}

/// Formats a timestamp for display.
///
/// With `local == false` (the default), the instant is rendered in UTC exactly
/// as before — byte-for-byte identical to historical output, with no offset
/// token. With `local == true`, it is rendered in the system local time zone
/// and a numeric offset token (e.g. ` +0200`) is appended so the value is
/// unambiguous. If the local offset cannot be determined, it falls back to UTC
/// with a `+0000` token (still distinguishable from default UTC output).
fn format_time(t: std::time::SystemTime, local: bool) -> String {
    let odt: OffsetDateTime = t.into();
    if !local {
        return odt.format(DATE_FORMAT).unwrap_or_default();
    }
    let offset = local_utc_offset(t).unwrap_or(UtcOffset::UTC);
    odt.to_offset(offset)
        .format(DATE_FORMAT_OFFSET)
        .unwrap_or_default()
}

fn format_object(obj: &crate::storage::Object, args: &LsArgs) -> String {
    let path = obj
        .url
        .as_ref()
        .map(|u| u.relative())
        .unwrap_or_default();

    if obj.typ.is_dir() && matches!(obj.typ, ObjectType::Dir) && obj.size == 0 && obj.mod_time.is_none()
    {
        // Directory / common prefix entry.
        return format!("{:>19} {:>2} {:<1} {:>12}  {}", "", "", "", "DIR", path);
    }

    let version = obj
        .url
        .as_ref()
        .filter(|u| !u.version_id.is_empty())
        .map(|u| format!("  {}", u.version_id))
        .unwrap_or_default();

    if obj.is_delete_marker {
        // Versioned tombstone: render `DELETE_MARKER` in the size column in
        // place of a real size, keeping the date and version id.
        let date = obj
            .mod_time
            .map(|t| format_time(t, args.local_time))
            .unwrap_or_default();
        return format!(
            "{date:>19} {:>2} {:<1} {:>12}  {path}{version}",
            "", "", "DELETE_MARKER"
        );
    }

    let date = obj
        .mod_time
        .map(|t| format_time(t, args.local_time))
        .unwrap_or_default();
    let stclass = if args.storage_class {
        obj.storage_class.0.clone()
    } else {
        String::new()
    };
    let etag = if args.etag { obj.etag.clone() } else { String::new() };
    let size = if args.humanize {
        humansize::format_size(obj.size.max(0) as u64, humansize::BINARY)
    } else {
        obj.size.to_string()
    };

    format!("{date:>19} {stclass:>2} {etag:<1} {size:>12}  {path}{version}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_summary_raw() {
        let s = format_summary(3, 2048, false);
        assert_eq!(s, "Total objects: 3\nTotal size: 2048");
    }

    #[test]
    fn format_summary_humanized() {
        let s = format_summary(2, 1024, true);
        assert!(s.contains("Total objects: 2"), "{s}");
        // humansize BINARY renders 1024 bytes as "1 KiB".
        assert!(s.contains("1 KiB") || s.contains("1.00 KiB"), "{s}");
    }

    #[test]
    fn format_summary_zero_raw() {
        let s = format_summary(0, 0, false);
        assert_eq!(s, "Total objects: 0\nTotal size: 0");
    }

    /// Builds a delete-marker Object for `s3://bucket/key` with a version id.
    fn delete_marker_obj() -> crate::storage::Object {
        let mut u = Url::parse("s3://bucket/key").unwrap();
        u.version_id = "v123".to_string();
        crate::storage::Object {
            url: Some(u),
            typ: ObjectType::File,
            is_delete_marker: true,
            ..Default::default()
        }
    }

    fn ls_args() -> LsArgs {
        LsArgs {
            target: None,
            humanize: false,
            storage_class: false,
            etag: false,
            all_versions: true,
            version_id: None,
            summarize: false,
            show_fullpath: false,
            start_after: None,
            newer_than: None,
            older_than: None,
            local_time: false,
            exclude: Vec::new(),
            include: Vec::new(),
            exclude_from: Vec::new(),
            include_from: Vec::new(),
        }
    }

    #[test]
    fn parse_relative_duration_units() {
        assert_eq!(parse_relative_duration("45s").unwrap(), Some(Duration::from_secs(45)));
        assert_eq!(parse_relative_duration("30m").unwrap(), Some(Duration::from_secs(30 * 60)));
        assert_eq!(parse_relative_duration("24h").unwrap(), Some(Duration::from_secs(24 * 3600)));
        assert_eq!(parse_relative_duration("7d").unwrap(), Some(Duration::from_secs(7 * 86400)));
    }

    #[test]
    fn parse_relative_duration_non_duration_is_none() {
        // No recognised suffix => fall through to RFC3339 parsing.
        assert_eq!(parse_relative_duration("2024-01-02T15:04:05Z").unwrap(), None);
        assert_eq!(parse_relative_duration("h").unwrap(), None);
        assert_eq!(parse_relative_duration("xyz").unwrap(), None);
    }

    #[test]
    fn parse_time_bound_relative_is_in_past() {
        let now = SystemTime::now();
        let bound = parse_time_bound("1h", now).unwrap();
        assert_eq!(now.duration_since(bound).unwrap(), Duration::from_secs(3600));
    }

    #[test]
    fn parse_time_bound_rfc3339() {
        // 1970-01-01T00:00:01Z == UNIX_EPOCH + 1s.
        let bound = parse_time_bound("1970-01-01T00:00:01Z", SystemTime::now()).unwrap();
        assert_eq!(
            bound.duration_since(std::time::UNIX_EPOCH).unwrap(),
            Duration::from_secs(1)
        );
    }

    #[test]
    fn parse_time_bound_rejects_garbage() {
        assert!(parse_time_bound("not-a-time", SystemTime::now()).is_err());
        assert!(parse_time_bound("", SystemTime::now()).is_err());
    }

    #[test]
    fn format_object_delete_marker_text() {
        let line = format_object(&delete_marker_obj(), &ls_args());
        // The size column shows DELETE_MARKER and the version id is appended.
        assert!(line.contains("DELETE_MARKER"), "{line}");
        assert!(line.contains("key"), "{line}");
        assert!(line.contains("v123"), "{line}");
    }

    #[test]
    fn object_json_delete_marker() {
        let v = object_json(&delete_marker_obj(), false);
        assert_eq!(v["type"], "delete_marker");
        assert_eq!(v["delete_marker"], serde_json::Value::Bool(true));
        assert_eq!(v["version_id"], "v123");
        // Tombstones carry no size.
        assert!(v.get("size").is_none(), "{v}");
    }

    #[test]
    fn object_json_regular_file_unchanged() {
        let u = Url::parse("s3://bucket/key").unwrap();
        let obj = crate::storage::Object {
            url: Some(u),
            typ: ObjectType::File,
            size: 42,
            ..Default::default()
        };
        let v = object_json(&obj, false);
        assert_eq!(v["type"], "file");
        assert_eq!(v["size"], 42);
        assert!(v.get("delete_marker").is_none(), "{v}");
    }

    /// A fixed instant: 2024-01-02T03:04:05Z (UNIX 1_704_164_645).
    fn fixed_time() -> SystemTime {
        std::time::UNIX_EPOCH + Duration::from_secs(1_704_164_645)
    }

    #[test]
    fn format_time_utc_is_unchanged() {
        // Default (UTC) output must be byte-identical to the historical format:
        // no offset token, rendered in UTC.
        let s = format_time(fixed_time(), false);
        assert_eq!(s, "2024/01/02 03:04:05");
    }

    #[test]
    fn format_time_local_appends_offset_token() {
        // `--local-time` always appends a numeric offset token (e.g. +0200 or
        // +0000) so the value is unambiguous and distinct from UTC output.
        let s = format_time(fixed_time(), true);
        let bytes = s.as_bytes();
        // "yyyy/mm/dd HH:MM:SS +HHMM" => 25 chars.
        assert_eq!(s.len(), 25, "unexpected local-time length: {s:?}");
        // Position 19 is the separating space; 20 is the sign.
        assert_eq!(bytes[19], b' ', "{s}");
        assert!(
            bytes[20] == b'+' || bytes[20] == b'-',
            "expected signed offset token in {s}"
        );
        // The four chars after the sign are digits (HHMM).
        for i in 21..25 {
            assert!(bytes[i].is_ascii_digit(), "expected digit at {i} in {s}");
        }
        // The date/time portion is still present (the leading 19 chars).
        assert!(s.starts_with("2024/01/02 "), "{s}");
    }

    #[test]
    fn local_utc_offset_does_not_panic() {
        // We can't assert a specific offset (it depends on the host TZ), but the
        // call must not panic. If it ever returns None we silently fall back to
        // UTC in `format_time`, which is acceptable.
        let _ = local_utc_offset(fixed_time());
    }
}
