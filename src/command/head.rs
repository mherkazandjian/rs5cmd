//! `head` — print remote object metadata, or check that a bucket exists.

use clap::Args;
use time::format_description::FormatItem;
use time::macros::format_description;
use time::OffsetDateTime;

use super::GlobalOpts;
use crate::storage::s3::S3;
use crate::storage::url::Url;
use crate::storage::{Metadata, Object, ObjectNotFound, ObjectType, StorageClass};

const DATE_FORMAT: &[FormatItem<'static>] =
    format_description!("[year]/[month]/[day] [hour]:[minute]:[second]");

impl S3 {
    /// Fetches object metadata via HeadObject. Mirrors the Go `S3.HeadObject`,
    /// returning both the `Object` and the extended `Metadata`.
    pub async fn head_object(&self, u: &Url) -> anyhow::Result<(Object, Metadata)> {
        let mut req = self
            .client
            .head_object()
            .bucket(&u.bucket)
            .key(&u.path)
            .set_request_payer(self.request_payer());
        if !u.version_id.is_empty() {
            req = req.version_id(&u.version_id);
        }

        match req.send().await {
            Ok(out) => {
                // STANDARD storage class is not returned in the response header.
                let storage_class = out
                    .storage_class()
                    .map(|s| s.as_str().to_string())
                    .unwrap_or_else(|| "STANDARD".to_string());

                let object = Object {
                    url: Some(u.clone()),
                    etag: out.e_tag().unwrap_or_default().trim_matches('"').to_string(),
                    mod_time: out.last_modified().and_then(|t| {
                        std::time::UNIX_EPOCH
                            .checked_add(std::time::Duration::from_secs(t.secs().max(0) as u64))
                    }),
                    size: out.content_length().unwrap_or(0),
                    typ: ObjectType::File,
                    storage_class: StorageClass(storage_class),
                    is_delete_marker: false,
                    err: None,
                };

                let metadata = Metadata {
                    content_type: out.content_type().map(|s| s.to_string()),
                    encryption_method: out
                        .server_side_encryption()
                        .map(|s| s.as_str().to_string()),
                    user_defined: out
                        .metadata()
                        .map(|m| m.clone().into_iter().collect())
                        .unwrap_or_default(),
                    ..Default::default()
                };

                Ok((object, metadata))
            }
            Err(e) => {
                if e.as_service_error().map(|se| se.is_not_found()).unwrap_or(false) {
                    Err(ObjectNotFound(u.absolute()).into())
                } else {
                    Err(e.into())
                }
            }
        }
    }

    /// Checks that a bucket exists and is accessible. Mirrors `S3.HeadBucket`.
    pub async fn head_bucket(&self, bucket: &str) -> anyhow::Result<()> {
        self.client.head_bucket().bucket(bucket).send().await?;
        Ok(())
    }
}

#[derive(Args, Debug)]
pub struct HeadArgs {
    /// Object or bucket URL (s3://bucket or s3://bucket/key).
    pub src: String,

    /// Use the specified version of an object.
    #[arg(long = "version-id")]
    pub version_id: Option<String>,

    /// Disable wildcard operations (useful with glob-like filenames).
    #[arg(long)]
    pub raw: bool,
}

pub async fn run(global: &GlobalOpts, args: HeadArgs) -> anyhow::Result<()> {
    let opts = global.storage_options();

    let url = Url::new(
        &args.src,
        crate::storage::url::UrlOptions {
            raw: args.raw,
            version_id: args.version_id.clone(),
            ..Default::default()
        },
    )
    .map_err(|e| anyhow::anyhow!(e))?;

    if url.is_prefix() {
        anyhow::bail!("target have to be a object or a bucket");
    }
    if !url.is_remote() {
        anyhow::bail!("target should be remote object or bucket");
    }
    if url.is_wildcard() && !url.is_raw() {
        anyhow::bail!("remote source {url:?} can not contain glob characters");
    }

    let s3 = S3::new(&url, &opts).await?;

    if url.is_bucket() {
        s3.head_bucket(&url.bucket).await?;
        if crate::output::is_json() {
            // Matches Go's `HeadBucketMessage`, which only carries `bucket`.
            crate::output::json_line(serde_json::json!({
                "bucket": url.to_string(),
            }));
        } else {
            println!("{url}  exists");
        }
        return Ok(());
    }

    let (object, metadata) = s3.head_object(&url).await?;

    if crate::output::is_json() {
        crate::output::json_line(object_json(&object, &metadata));
    } else {
        println!("{}", format_object(&object, &metadata));
    }
    Ok(())
}

/// Builds the JSON representation of an object's head metadata, mirroring the
/// Go `HeadObjectMessage` struct. The JSON field names match s5cmd:
/// `key`, `content_type`, `server_side_encryption`, `last_modified`, `size`,
/// `storage_class`, `version_id`, `etag` and `metadata`.
///
/// As in Go, fields tagged `omitempty` are dropped when empty/zero (everything
/// except `metadata`, which is always present — emitted as `{}` when empty to
/// match Go's non-`omitempty` map field). Timestamps are rendered with the same
/// `[year]/[month]/[day] [hour]:[minute]:[second]` format used by `ls`.
fn object_json(object: &Object, metadata: &Metadata) -> serde_json::Value {
    let key = object
        .url
        .as_ref()
        .map(|u| u.to_string())
        .unwrap_or_default();

    // Go marshals `key` with `omitempty`, so only include it when non-empty.
    let mut v = serde_json::Map::new();
    if !key.is_empty() {
        v.insert("key".to_string(), serde_json::Value::String(key));
    }
    if let Some(ct) = &metadata.content_type {
        if !ct.is_empty() {
            v.insert(
                "content_type".to_string(),
                serde_json::Value::String(ct.clone()),
            );
        }
    }
    if let Some(sse) = &metadata.encryption_method {
        if !sse.is_empty() {
            v.insert(
                "server_side_encryption".to_string(),
                serde_json::Value::String(sse.clone()),
            );
        }
    }
    if let Some(t) = object.mod_time {
        let odt: OffsetDateTime = t.into();
        if let Ok(s) = odt.format(DATE_FORMAT) {
            v.insert("last_modified".to_string(), serde_json::Value::String(s));
        }
    }
    // `size` carries `omitempty` in Go, so a zero size is dropped.
    if object.size != 0 {
        v.insert(
            "size".to_string(),
            serde_json::Value::Number(object.size.into()),
        );
    }
    if !object.storage_class.0.is_empty() {
        v.insert(
            "storage_class".to_string(),
            serde_json::Value::String(object.storage_class.0.clone()),
        );
    }
    let version_id = object
        .url
        .as_ref()
        .map(|u| u.version_id.clone())
        .unwrap_or_default();
    if !version_id.is_empty() {
        v.insert(
            "version_id".to_string(),
            serde_json::Value::String(version_id),
        );
    }
    if !object.etag.is_empty() {
        v.insert(
            "etag".to_string(),
            serde_json::Value::String(object.etag.clone()),
        );
    }
    // `metadata` has no `omitempty` in Go; always emit it (as `{}` when empty).
    let map: serde_json::Map<String, serde_json::Value> = metadata
        .user_defined
        .iter()
        .map(|(k, val)| (k.clone(), serde_json::Value::String(val.clone())))
        .collect();
    v.insert("metadata".to_string(), serde_json::Value::Object(map));

    serde_json::Value::Object(v)
}

/// Renders a concise one-line summary of object metadata.
fn format_object(object: &Object, metadata: &Metadata) -> String {
    let key = object
        .url
        .as_ref()
        .map(|u| u.to_string())
        .unwrap_or_default();
    let modified = object
        .mod_time
        .map(|t| {
            let odt: OffsetDateTime = t.into();
            odt.format(DATE_FORMAT).unwrap_or_default()
        })
        .unwrap_or_default();

    let mut parts = vec![
        key,
        format!("{} bytes", object.size),
        format!("etag={}", object.etag),
    ];
    if !modified.is_empty() {
        parts.push(format!("last-modified={modified}"));
    }
    if !object.storage_class.0.is_empty() {
        parts.push(format!("storage-class={}", object.storage_class.0));
    }
    if let Some(ct) = &metadata.content_type {
        if !ct.is_empty() {
            parts.push(format!("content-type={ct}"));
        }
    }
    if let Some(sse) = &metadata.encryption_method {
        if !sse.is_empty() {
            parts.push(format!("sse={sse}"));
        }
    }
    let version_id = object
        .url
        .as_ref()
        .map(|u| u.version_id.clone())
        .unwrap_or_default();
    if !version_id.is_empty() {
        parts.push(format!("version-id={version_id}"));
    }
    for (k, v) in &metadata.user_defined {
        parts.push(format!("{k}={v}"));
    }

    parts.join("  ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn object_json_includes_present_fields_only() {
        let object = Object {
            url: None,
            etag: "abc123".to_string(),
            mod_time: None,
            typ: ObjectType::File,
            size: 42,
            storage_class: StorageClass("STANDARD".to_string()),
            is_delete_marker: false,
            err: None,
        };
        let mut metadata = Metadata {
            content_type: Some("text/plain".to_string()),
            ..Default::default()
        };
        metadata
            .user_defined
            .insert("foo".to_string(), "bar".to_string());

        let v = object_json(&object, &metadata);

        assert_eq!(v["size"], serde_json::json!(42));
        assert_eq!(v["etag"], serde_json::json!("abc123"));
        assert_eq!(v["storage_class"], serde_json::json!("STANDARD"));
        assert_eq!(v["content_type"], serde_json::json!("text/plain"));
        assert_eq!(v["metadata"]["foo"], serde_json::json!("bar"));
        // Absent fields must not appear.
        assert!(v.get("last_modified").is_none());
        assert!(v.get("server_side_encryption").is_none());
        assert!(v.get("version_id").is_none());
        // Go uses `server_side_encryption`, never the short `sse` key.
        assert!(v.get("sse").is_none());
    }

    #[test]
    fn object_json_omits_empty_etag_and_storage_class() {
        let object = Object {
            url: None,
            etag: String::new(),
            mod_time: None,
            typ: ObjectType::File,
            size: 0,
            storage_class: StorageClass(String::new()),
            is_delete_marker: false,
            err: None,
        };
        let metadata = Metadata::default();

        let v = object_json(&object, &metadata);

        // `size`, `etag` and `storage_class` carry `omitempty` in Go.
        assert!(v.get("size").is_none());
        assert!(v.get("etag").is_none());
        assert!(v.get("storage_class").is_none());
        assert!(v.get("content_type").is_none());
        // `metadata` has no `omitempty`: always present, as an empty object.
        assert_eq!(v["metadata"], serde_json::json!({}));
    }

    #[test]
    fn object_json_emits_full_field_set() {
        let mut url = Url::new(
            "s3://bucket/prefix/object",
            crate::storage::url::UrlOptions::default(),
        )
        .unwrap();
        // Set the version id directly so the assertion does not depend on how
        // `Url::new` plumbs `UrlOptions.version_id` into the struct.
        url.version_id = "v-123".to_string();

        let object = Object {
            url: Some(url),
            etag: "deadbeef".to_string(),
            mod_time: Some(
                std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000),
            ),
            typ: ObjectType::File,
            size: 1024,
            storage_class: StorageClass("GLACIER".to_string()),
            is_delete_marker: false,
            err: None,
        };
        let mut metadata = Metadata {
            content_type: Some("application/json".to_string()),
            encryption_method: Some("aws:kms".to_string()),
            ..Default::default()
        };
        metadata
            .user_defined
            .insert("owner".to_string(), "alice".to_string());

        let v = object_json(&object, &metadata);

        assert_eq!(v["key"], serde_json::json!("s3://bucket/prefix/object"));
        assert_eq!(v["size"], serde_json::json!(1024));
        assert_eq!(v["etag"], serde_json::json!("deadbeef"));
        assert_eq!(v["storage_class"], serde_json::json!("GLACIER"));
        assert_eq!(v["content_type"], serde_json::json!("application/json"));
        // Go names the SSE field `server_side_encryption`.
        assert_eq!(v["server_side_encryption"], serde_json::json!("aws:kms"));
        assert_eq!(v["version_id"], serde_json::json!("v-123"));
        assert_eq!(v["metadata"]["owner"], serde_json::json!("alice"));
        assert!(v.get("last_modified").is_some());
    }
}
