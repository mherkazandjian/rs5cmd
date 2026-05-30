//! `bucket-version` — get or set a bucket's versioning status.

use clap::Args;

use super::GlobalOpts;
use crate::storage::s3::S3;
use crate::storage::url::Url;

#[derive(Args, Debug)]
pub struct BucketVersionArgs {
    /// Bucket URL, e.g. s3://my-bucket.
    pub bucket: String,

    /// Set the versioning status ("Enabled" or "Suspended"). Omit to read it.
    #[arg(long)]
    pub set: Option<String>,
}

pub async fn run(global: &GlobalOpts, args: BucketVersionArgs) -> anyhow::Result<()> {
    let opts = global.storage_options();
    let url = Url::parse(&args.bucket).map_err(|e| anyhow::anyhow!(e))?;
    if !url.is_remote() || !url.is_bucket() {
        anyhow::bail!("expected a bucket URL like s3://bucket, got {:?}", args.bucket);
    }
    let s3 = S3::new(&url, &opts).await?;

    match &args.set {
        Some(raw) => {
            let lc = raw.to_lowercase();
            let status: &str = if lc == "enabled" {
                "Enabled"
            } else if lc == "suspended" {
                "Suspended"
            } else {
                anyhow::bail!("invalid versioning status {raw:?} (use Enabled or Suspended)");
            };
            s3.set_bucket_versioning(&url.bucket, status).await?;
            emit(&url, status, true);
        }
        None => {
            let status = s3.get_bucket_versioning(&url.bucket).await?;
            emit(&url, status.as_str(), false);
        }
    }
    Ok(())
}

fn emit(url: &Url, status: &str, did_set: bool) {
    if crate::output::is_json() {
        let mut v = serde_json::json!({ "bucket": url.to_string(), "versioning": status });
        if did_set {
            v["success"] = serde_json::Value::Bool(true);
        }
        crate::output::json_line(v);
    } else {
        println!("{url} {status}");
    }
}
