//! `mb` / `rb` — make and remove S3 buckets.

use clap::Args;

use super::GlobalOpts;
use crate::storage::s3::S3;
use crate::storage::url::Url;

#[derive(Args, Debug)]
pub struct MbArgs {
    /// Bucket URL, e.g. s3://my-bucket.
    pub bucket: String,
}

#[derive(Args, Debug)]
pub struct RbArgs {
    /// Bucket URL, e.g. s3://my-bucket.
    pub bucket: String,
}

fn bucket_name(arg: &str) -> anyhow::Result<(Url, String)> {
    let url = Url::parse(arg).map_err(|e| anyhow::anyhow!(e))?;
    if !url.is_remote() || !url.is_bucket() {
        anyhow::bail!("expected a bucket URL like s3://bucket-name, got {arg:?}");
    }
    let name = url.bucket.clone();
    Ok((url, name))
}

pub async fn run_mb(global: &GlobalOpts, args: MbArgs) -> anyhow::Result<()> {
    let opts = global.storage_options();
    let (url, name) = bucket_name(&args.bucket)?;
    let s3 = S3::new(&url, &opts).await?;
    s3.make_bucket(&name).await?;
    crate::output::op_success("mb", &url.to_string(), None);
    Ok(())
}

pub async fn run_rb(global: &GlobalOpts, args: RbArgs) -> anyhow::Result<()> {
    let opts = global.storage_options();
    let (url, name) = bucket_name(&args.bucket)?;
    let s3 = S3::new(&url, &opts).await?;
    s3.remove_bucket(&name).await?;
    crate::output::op_success("rb", &url.to_string(), None);
    Ok(())
}
