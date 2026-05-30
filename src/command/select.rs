//! `select` — run S3 Select (SelectObjectContent) SQL queries on objects.
//!
//! Ported from s5cmd's `command/select.go` and the `Select` /
//! `parseInputSerialization` / `parseOutputSerialization` helpers in
//! `storage/s3.go`. Supports json/csv/parquet input and json/csv output.
//!
//! A single concrete object source (e.g. `s3://bucket/key.json`) takes a fast
//! path that streams the Select result straight to stdout. Wildcard, prefix and
//! bucket-root sources are expanded via `list()` into the set of matching
//! objects. The matched objects are sorted by key and processed SEQUENTIALLY,
//! each one's record stream written live to stdout. Sequential processing is
//! the deliberate trade-off for ordered, live streaming output: it gives
//! deterministic ordering (stable across runs) and avoids buffering whole
//! objects in memory, at the cost of cross-object concurrency.
//!
//! Object versions are supported on the expansion path: `--all-versions` lists
//! every version of the matched objects (routing to `ListObjectVersions`).
//! Selecting a *single specific* version via `--version-id` is best-effort: the
//! S3 `SelectObjectContent` API has no `versionId` parameter (see [`S3::select`]),
//! so the query always runs against the current version of the key.

use clap::Args;
use regex::Regex;
use tokio::io::AsyncWriteExt;

use aws_sdk_s3::types::{
    CompressionType, CsvInput, CsvOutput, ExpressionType, FileHeaderInfo, InputSerialization,
    JsonInput, JsonOutput, JsonType, OutputSerialization, ParquetInput,
    SelectObjectContentEventStream,
};

use super::GlobalOpts;
use crate::storage::s3::S3;
use crate::storage::url::{Url, UrlOptions};
use crate::storage::Storage as _;

/// Default SQL expression used when `--query` is omitted.
const DEFAULT_QUERY: &str = "SELECT * FROM s3object s";

/// Parsed select options, mirroring Go's `storage.SelectQuery`.
#[derive(Debug, Clone)]
pub struct SelectQuery {
    /// SQL expression to run.
    pub expression: String,
    /// Input format: `json`, `csv` or `parquet`.
    pub input_format: String,
    /// Compression of the input object: `none`, `gzip` or `bzip2`.
    pub compression: String,
    /// For json input: `lines` or `document`. For csv input: the delimiter.
    pub input_structure: String,
    /// CSV file header handling (`USE`, `IGNORE`, `NONE`).
    pub file_header_info: String,
    /// Output format: `json` or `csv`.
    pub output_format: String,
}

impl SelectQuery {
    /// Builds the AWS `InputSerialization` for this query.
    fn input_serialization(&self) -> anyhow::Result<InputSerialization> {
        let mut builder = InputSerialization::builder();

        match self.input_format.as_str() {
            "json" => {
                // For json input the structure carries the JSON type (lines/document).
                let json_type = match self.input_structure.to_lowercase().as_str() {
                    "document" => JsonType::Document,
                    // default to LINES for "lines" or anything else
                    _ => JsonType::Lines,
                };
                builder = builder.json(JsonInput::builder().r#type(json_type).build());
            }
            "csv" => {
                let header = file_header_info(&self.file_header_info);
                let delimiter = if self.input_structure.is_empty() {
                    ",".to_string()
                } else {
                    self.input_structure.clone()
                };
                builder = builder.csv(
                    CsvInput::builder()
                        .field_delimiter(delimiter)
                        .file_header_info(header)
                        .build(),
                );
            }
            "parquet" => {
                builder = builder.parquet(ParquetInput::builder().build());
            }
            other => {
                anyhow::bail!("input format is not valid: {other}");
            }
        }

        // Parquet input does not support compression here.
        if self.input_format != "parquet" {
            if let Some(ct) = compression_type(&self.compression) {
                builder = builder.compression_type(ct);
            }
        }

        Ok(builder.build())
    }

    /// Builds the AWS `OutputSerialization` for this query.
    fn output_serialization(&self) -> anyhow::Result<OutputSerialization> {
        let builder = OutputSerialization::builder();
        let builder = match self.output_format.as_str() {
            "json" => builder.json(JsonOutput::builder().build()),
            "csv" => {
                // When converting json input to csv output, the input structure
                // (a delimiter) is meaningless; default to a comma.
                let delimiter = if self.input_format == "json" {
                    ",".to_string()
                } else if self.input_structure.is_empty() {
                    ",".to_string()
                } else {
                    self.input_structure.clone()
                };
                builder.csv(CsvOutput::builder().field_delimiter(delimiter).build())
            }
            other => anyhow::bail!("output serialization is not valid: {other}"),
        };
        Ok(builder.build())
    }
}

/// Maps a user compression string to the AWS `CompressionType`.
/// Returns `None` for empty/`none`.
fn compression_type(c: &str) -> Option<CompressionType> {
    match c.to_lowercase().as_str() {
        "" | "none" => None,
        "gzip" => Some(CompressionType::Gzip),
        "bzip2" => Some(CompressionType::Bzip2),
        // Pass through anything else verbatim (other providers may differ).
        other => Some(CompressionType::from(other.to_uppercase().as_str())),
    }
}

/// Maps the `--use-header` value to the AWS `FileHeaderInfo` enum.
fn file_header_info(s: &str) -> FileHeaderInfo {
    match s.to_uppercase().as_str() {
        "USE" => FileHeaderInfo::Use,
        "IGNORE" => FileHeaderInfo::Ignore,
        _ => FileHeaderInfo::None,
    }
}

/// Compiled `--exclude` glob filters, matched against object keys/relative
/// paths. A small private copy of the `sync` filter pattern (kept local so this
/// file does not depend on `sync.rs`).
struct ExcludeFilter {
    patterns: Vec<Regex>,
}

impl ExcludeFilter {
    /// Compiles the given wildcard globs into anchored regexes.
    fn new(excludes: &[String]) -> anyhow::Result<ExcludeFilter> {
        let mut patterns = Vec::with_capacity(excludes.len());
        for p in excludes {
            let mut re = crate::strutil::wildcard_to_regexp(p);
            re = crate::strutil::match_from_start_to_end(&re);
            re = crate::strutil::add_newline_flag(&re);
            patterns.push(Regex::new(&re)?);
        }
        Ok(ExcludeFilter { patterns })
    }

    /// Returns true if an object with the given key should be skipped because it
    /// matches any exclude pattern. With no patterns, nothing is excluded.
    fn is_excluded(&self, key: &str) -> bool {
        self.patterns.iter().any(|re| re.is_match(key))
    }
}

impl S3 {
    /// Runs an S3 Select query on a single object, writing the result records
    /// to `out`. Ignores Stats/Progress/Continuation events and stops on End.
    ///
    /// NOTE: the S3 `SelectObjectContent` operation has no `versionId`
    /// parameter (it is not supported by the AWS API, and the aws-sdk-s3 fluent
    /// builder exposes no `.version_id()` method), so the query always runs
    /// against the current version of `src.path`. Any `src.version_id` is
    /// therefore ignored here; selecting a specific past version is unsupported.
    pub async fn select(
        &self,
        src: &Url,
        query: &SelectQuery,
        out: &mut (impl AsyncWriteExt + Unpin),
    ) -> anyhow::Result<()> {
        if self.dry_run {
            return Ok(());
        }

        let input_serialization = query.input_serialization()?;
        let output_serialization = query.output_serialization()?;

        let resp = self
            .client
            .select_object_content()
            .bucket(&src.bucket)
            .key(&src.path)
            .expression(&query.expression)
            .expression_type(ExpressionType::Sql)
            .input_serialization(input_serialization)
            .output_serialization(output_serialization)
            .send()
            .await?;

        let mut payload = resp.payload;

        // The event stream yields Records/Stats/Progress/Cont/End events. We
        // forward Records payload bytes to the writer and stop at End/EOF.
        while let Some(event) = payload.recv().await? {
            match event {
                SelectObjectContentEventStream::Records(records) => {
                    if let Some(blob) = records.payload() {
                        out.write_all(blob.as_ref()).await?;
                    }
                }
                SelectObjectContentEventStream::End(_) => break,
                // Stats, Progress, Continuation and any unknown events are ignored.
                _ => {}
            }
        }

        out.flush().await?;
        Ok(())
    }
}

#[derive(Args, Debug)]
pub struct SelectArgs {
    /// Source (s3:// URL). A single object, or a wildcard/prefix/bucket that
    /// expands to many objects via listing.
    pub src: String,

    /// SQL expression to run against the object.
    #[arg(long, short = 'e', default_value = DEFAULT_QUERY)]
    pub query: String,

    /// Input format of the source object.
    #[arg(long, default_value = "json", value_parser = ["json", "csv", "parquet"])]
    pub input_format: String,

    /// Input compression format.
    #[arg(long, default_value = "none", value_parser = ["none", "gzip", "bzip2"])]
    pub compression: String,

    /// For JSON input: how records are laid out.
    #[arg(long, default_value = "lines", value_parser = ["lines", "document"])]
    pub json_type: String,

    /// For CSV input: the field delimiter.
    #[arg(long, default_value = ",")]
    pub delimiter: String,

    /// For CSV input: how to treat the header row.
    #[arg(long, default_value = "NONE")]
    pub use_header: String,

    /// Output format of the result. Defaults to the input format (json/csv;
    /// parquet input defaults to json output).
    #[arg(long, value_parser = ["json", "csv"])]
    pub output_format: Option<String>,

    /// Exclude objects whose relative path/key matches the given glob
    /// (repeatable). Only affects the expansion path (wildcard/prefix/bucket
    /// sources); a single concrete object source is selected as given.
    #[arg(long)]
    pub exclude: Vec<String>,

    /// Select against a specific object version id.
    ///
    /// Best-effort only: the S3 `SelectObjectContent` API takes no version id,
    /// so the query still runs against the current version of the key. Provided
    /// for CLI parity and to route a single-object source through versioned URL
    /// construction.
    #[arg(long)]
    pub version_id: Option<String>,

    /// Expand the source across all object versions when listing (requires a
    /// versioned bucket). Routes expansion to `ListObjectVersions`.
    #[arg(long)]
    pub all_versions: bool,
}

impl SelectQuery {
    /// Builds a `SelectQuery` from parsed CLI args, applying the input-structure
    /// and output-format defaulting rules.
    fn from_args(args: &SelectArgs) -> SelectQuery {
        // For json input the "structure" slot carries the JSON type; for csv it
        // carries the delimiter; parquet has no structure.
        let input_structure = match args.input_format.as_str() {
            "json" => args.json_type.clone(),
            "csv" => args.delimiter.clone(),
            _ => String::new(),
        };

        // Output format defaults to the input format, but parquet has no
        // matching output serialization, so fall back to json.
        let output_format = args.output_format.clone().unwrap_or_else(|| {
            if args.input_format == "parquet" {
                "json".to_string()
            } else {
                args.input_format.clone()
            }
        });

        SelectQuery {
            expression: args.query.clone(),
            input_format: args.input_format.clone(),
            compression: args.compression.clone(),
            input_structure,
            file_header_info: args.use_header.clone(),
            output_format,
        }
    }
}

pub async fn run(global: &GlobalOpts, args: SelectArgs) -> anyhow::Result<()> {
    let opts = global.storage_options();

    // Build the source URL with version options so versioned sources route to
    // `ListObjectVersions` during expansion. A `--version-id` on a single object
    // is best-effort (see `S3::select`): the URL carries it, but the Select call
    // ignores it because the API has no `versionId` parameter.
    let url = Url::new(
        &args.src,
        UrlOptions {
            all_versions: args.all_versions,
            version_id: args.version_id.clone(),
            ..Default::default()
        },
    )
    .map_err(|e| anyhow::anyhow!(e))?;

    if !url.is_remote() {
        anyhow::bail!("source must be remote");
    }
    if args.query.is_empty() {
        anyhow::bail!("query must be non-empty");
    }

    let query = SelectQuery::from_args(&args);

    let s3 = S3::new(&url, &opts).await?;

    // A single, concrete object (no wildcard, not a bucket root, not a prefix
    // ending in `/`) keeps the original streaming-to-stdout fast path.
    if !url.is_wildcard() && !url.is_bucket() && !url.is_prefix() {
        let mut stdout = tokio::io::stdout();
        s3.select(&url, &query, &mut stdout).await?;
        stdout.flush().await?;
        return Ok(());
    }

    // Otherwise expand the source by listing it and run the query against every
    // matched object concurrently.
    let exclude = ExcludeFilter::new(&args.exclude)?;
    run_expanded(s3, url, query, global, exclude).await
}

/// Expands a wildcard/prefix/bucket source via `list()` and runs the Select
/// query against every matched object, streaming each object's records live to
/// stdout in a deterministic, key-sorted order.
///
/// The matched object URLs are collected, sorted by key, and then processed
/// SEQUENTIALLY — each object's Select event stream is written straight to
/// stdout before the next object starts. This is the deliberate trade-off for
/// ordered, live streaming output: it yields stable ordering across runs and
/// never buffers a whole object in memory, at the cost of cross-object
/// concurrency. (The previous implementation ran objects concurrently into
/// per-object buffers and flushed in completion order, which produced output in
/// a nondeterministic order.)
///
/// Objects in a Glacier tier are skipped (S3 Select cannot read them until they
/// are restored), and objects matching any `--exclude` glob are skipped too.
///
/// Object versions: when the source `url` was built with `all_versions`, the
/// listing routes through `ListObjectVersions`, so every version of the matched
/// keys is selected (in sorted, deterministic order). The Select call itself is
/// always against the current version of each key — see [`S3::select`].
async fn run_expanded(
    s3: S3,
    url: Url,
    query: SelectQuery,
    _global: &GlobalOpts,
    exclude: ExcludeFilter,
) -> anyhow::Result<()> {
    // Collect the matched object URLs, skipping directories / common prefixes,
    // Glacier objects (unreadable by Select) and `--exclude` matches.
    let mut rx = s3.list(&url, false);
    let mut srcs: Vec<Url> = Vec::new();
    while let Some(obj) = rx.recv().await {
        if let Some(err) = obj.err {
            return Err(err);
        }
        if obj.typ.is_dir() {
            continue;
        }
        let Some(obj_url) = obj.url else { continue };
        // S3 Select cannot read GLACIER objects; note and skip rather than error.
        if obj.storage_class.is_glacier() {
            eprintln!("skipping glacier object {obj_url}");
            continue;
        }
        // Match excludes against the relative path (falls back to the key).
        if exclude.is_excluded(&obj_url.relative()) {
            continue;
        }
        srcs.push(obj_url);
    }

    if srcs.is_empty() {
        anyhow::bail!("no objects matched {url}");
    }

    // Sort by key (then version id) so the output order is deterministic and
    // independent of listing pagination order. With `--all-versions` the same
    // key can appear multiple times; the version id tie-breaks stably.
    srcs.sort_by(|a, b| a.path.cmp(&b.path).then(a.version_id.cmp(&b.version_id)));

    let mut stdout = tokio::io::stdout();
    let mut had_error = false;

    // Process objects one at a time, streaming each object's records straight to
    // stdout. Because only one object writes at a time, the streams cannot
    // interleave and no per-object buffering is needed.
    for src in srcs.iter() {
        if let Err(e) = s3.select(src, &query, &mut stdout).await {
            eprintln!("ERROR select {src}: {e:#}");
            had_error = true;
        }
    }

    stdout.flush().await?;

    if had_error {
        anyhow::bail!("select failed for one or more objects");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_query() -> SelectQuery {
        SelectQuery {
            expression: DEFAULT_QUERY.to_string(),
            input_format: "json".to_string(),
            compression: "none".to_string(),
            input_structure: "lines".to_string(),
            file_header_info: "NONE".to_string(),
            output_format: "json".to_string(),
        }
    }

    #[test]
    fn json_lines_input_builds() {
        let q = base_query();
        let s = q.input_serialization().unwrap();
        assert!(s.json().is_some());
        assert_eq!(s.json().unwrap().r#type(), Some(&JsonType::Lines));
        assert!(s.compression_type().is_none());
    }

    #[test]
    fn json_document_input_builds() {
        let mut q = base_query();
        q.input_structure = "document".to_string();
        let s = q.input_serialization().unwrap();
        assert_eq!(s.json().unwrap().r#type(), Some(&JsonType::Document));
    }

    #[test]
    fn csv_input_with_gzip_builds() {
        let mut q = base_query();
        q.input_format = "csv".to_string();
        q.input_structure = ";".to_string();
        q.file_header_info = "USE".to_string();
        q.compression = "gzip".to_string();
        let s = q.input_serialization().unwrap();
        let csv = s.csv().unwrap();
        assert_eq!(csv.field_delimiter(), Some(";"));
        assert_eq!(csv.file_header_info(), Some(&FileHeaderInfo::Use));
        assert_eq!(s.compression_type(), Some(&CompressionType::Gzip));
    }

    #[test]
    fn parquet_input_ignores_compression() {
        let mut q = base_query();
        q.input_format = "parquet".to_string();
        q.compression = "gzip".to_string();
        let s = q.input_serialization().unwrap();
        assert!(s.parquet().is_some());
        assert!(s.compression_type().is_none());
    }

    #[test]
    fn csv_output_builds() {
        let mut q = base_query();
        q.output_format = "csv".to_string();
        let s = q.output_serialization().unwrap();
        assert!(s.csv().is_some());
    }

    #[test]
    fn invalid_input_format_errors() {
        let mut q = base_query();
        q.input_format = "xml".to_string();
        assert!(q.input_serialization().is_err());
    }

    /// Verifies how `run()` classifies a source into the single-object fast path
    /// versus the expand-and-fan-out path. A concrete key takes the fast path;
    /// wildcards, prefixes and bucket roots are expanded.
    #[test]
    fn source_classification() {
        let is_single = |raw: &str| {
            let u = Url::parse(raw).unwrap();
            u.is_remote() && !u.is_wildcard() && !u.is_bucket() && !u.is_prefix()
        };

        // Single concrete object -> fast path.
        assert!(is_single("s3://bucket/key.json"));
        assert!(is_single("s3://bucket/dir/key.json"));

        // Expanded sources -> fan-out path.
        assert!(!is_single("s3://bucket/dir/*.json")); // wildcard
        assert!(!is_single("s3://bucket/dir/")); // prefix
        assert!(!is_single("s3://bucket")); // bucket root
    }

    #[test]
    fn exclude_filter_matches_globs() {
        // No patterns -> nothing excluded.
        let f = ExcludeFilter::new(&[]).unwrap();
        assert!(!f.is_excluded("data/a.json"));

        // Suffix glob.
        let f = ExcludeFilter::new(&["*.tmp".to_string()]).unwrap();
        assert!(f.is_excluded("data/a.tmp"));
        assert!(!f.is_excluded("data/a.json"));

        // Patterns are anchored start-to-end, so a partial match does not skip.
        let f = ExcludeFilter::new(&["a.json".to_string()]).unwrap();
        assert!(f.is_excluded("a.json"));
        assert!(!f.is_excluded("data/a.json"));

        // Multiple patterns: any match excludes; `?` matches one char.
        let f = ExcludeFilter::new(&["*.bak".to_string(), "tmp?".to_string()]).unwrap();
        assert!(f.is_excluded("x.bak"));
        assert!(f.is_excluded("tmpA"));
        assert!(!f.is_excluded("keep.json"));
    }

    #[test]
    fn from_args_defaults_output_and_structure() {
        let args = SelectArgs {
            src: "s3://bucket/data/".to_string(),
            query: DEFAULT_QUERY.to_string(),
            input_format: "csv".to_string(),
            compression: "gzip".to_string(),
            json_type: "lines".to_string(),
            delimiter: ";".to_string(),
            use_header: "USE".to_string(),
            output_format: None,
            exclude: Vec::new(),
            version_id: None,
            all_versions: false,
        };
        let q = SelectQuery::from_args(&args);
        // csv input -> structure carries the delimiter.
        assert_eq!(q.input_structure, ";");
        // output format defaults to the input format.
        assert_eq!(q.output_format, "csv");
        assert_eq!(q.compression, "gzip");
        assert_eq!(q.file_header_info, "USE");

        // parquet input with no explicit output format falls back to json.
        let parquet = SelectArgs {
            input_format: "parquet".to_string(),
            ..args
        };
        let q = SelectQuery::from_args(&parquet);
        assert_eq!(q.output_format, "json");
        assert_eq!(q.input_structure, "");
    }

    /// The expansion path sorts matched objects by key (then version id) before
    /// streaming, so output ordering is deterministic regardless of the order in
    /// which listing yielded them. This mirrors the sort key used in
    /// `run_expanded`.
    #[test]
    fn matched_objects_sort_deterministically() {
        let mut srcs: Vec<Url> = ["s3://b/c.json", "s3://b/a.json", "s3://b/b.json"]
            .iter()
            .map(|s| Url::parse(s).unwrap())
            .collect();
        srcs.sort_by(|a, b| a.path.cmp(&b.path).then(a.version_id.cmp(&b.version_id)));
        let keys: Vec<&str> = srcs.iter().map(|u| u.path.as_str()).collect();
        assert_eq!(keys, ["a.json", "b.json", "c.json"]);
    }

    /// With multiple versions of the same key, the version id tie-breaks so the
    /// per-key ordering is stable too.
    #[test]
    fn matched_objects_sort_by_version_id() {
        let mut srcs = vec![
            Url::new(
                "s3://b/a.json",
                UrlOptions {
                    version_id: Some("v2".to_string()),
                    ..Default::default()
                },
            )
            .unwrap(),
            Url::new(
                "s3://b/a.json",
                UrlOptions {
                    version_id: Some("v1".to_string()),
                    ..Default::default()
                },
            )
            .unwrap(),
        ];
        srcs.sort_by(|a, b| a.path.cmp(&b.path).then(a.version_id.cmp(&b.version_id)));
        let versions: Vec<&str> = srcs.iter().map(|u| u.version_id.as_str()).collect();
        assert_eq!(versions, ["v1", "v2"]);
    }
}
