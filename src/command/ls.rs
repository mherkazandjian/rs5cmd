//! `ls` — list buckets and objects.

use std::time::{Duration, SystemTime};

use clap::Args;
use time::format_description::FormatItem;
use time::macros::format_description;
use time::OffsetDateTime;

use super::GlobalOpts;
use crate::storage::url::Url;
use crate::storage::{new_client, ObjectType};

const DATE_FORMAT: &[FormatItem<'static>] =
    format_description!("[year]/[month]/[day] [hour]:[minute]:[second]");

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
            return list_buckets(global, "").await;
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
            crate::output::json_line(object_json(&obj));
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
fn object_json(obj: &crate::storage::Object) -> serde_json::Value {
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
            v["last_modified"] = serde_json::Value::String(format_time(t));
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
            v["last_modified"] = serde_json::Value::String(format_time(t));
        }
    }
    if let Some(u) = &obj.url {
        if !u.version_id.is_empty() {
            v["version_id"] = serde_json::Value::String(u.version_id.clone());
        }
    }
    v
}

async fn list_buckets(global: &GlobalOpts, prefix: &str) -> anyhow::Result<()> {
    let opts = global.storage_options();
    // Buckets require an S3 client; construct one against a dummy bucket URL.
    let url = Url::parse("s3://_").map_err(|e| anyhow::anyhow!(e))?;
    let s3 = crate::storage::s3::S3::new(&url, &opts).await?;
    let buckets = s3.list_buckets(prefix).await?;
    for b in buckets {
        if crate::output::is_json() {
            let mut v = serde_json::json!({ "name": format!("s3://{}", b.name) });
            if let Some(t) = b.creation_date {
                v["created_at"] = serde_json::Value::String(format_time(t));
            }
            crate::output::json_line(v);
            continue;
        }
        let date = b
            .creation_date
            .map(format_time)
            .unwrap_or_else(|| " ".repeat(19));
        println!("{date}  s3://{}", b.name);
    }
    Ok(())
}

fn format_time(t: std::time::SystemTime) -> String {
    let odt: OffsetDateTime = t.into();
    odt.format(DATE_FORMAT).unwrap_or_default()
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
        let date = obj.mod_time.map(format_time).unwrap_or_default();
        return format!(
            "{date:>19} {:>2} {:<1} {:>12}  {path}{version}",
            "", "", "DELETE_MARKER"
        );
    }

    let date = obj.mod_time.map(format_time).unwrap_or_default();
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
        let v = object_json(&delete_marker_obj());
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
        let v = object_json(&obj);
        assert_eq!(v["type"], "file");
        assert_eq!(v["size"], 42);
        assert!(v.get("delete_marker").is_none(), "{v}");
    }
}
