//! `mb` / `rb` — make and remove S3 buckets.

use clap::Args;
use tokio::sync::mpsc;

use super::GlobalOpts;
use crate::storage::s3::S3;
use crate::storage::url::Url;
use crate::storage::Storage;

#[derive(Args, Debug)]
pub struct MbArgs {
    /// Bucket URL, e.g. s3://my-bucket.
    pub bucket: String,
}

#[derive(Args, Debug)]
pub struct RbArgs {
    /// Bucket URL, e.g. s3://my-bucket.
    pub bucket: String,

    /// Remove the bucket even if it is not empty: delete all objects first.
    #[arg(long)]
    pub force: bool,
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

    if args.force {
        // List the whole bucket (paginated) and batch-delete via the same
        // chunked multi-delete `rm` uses. `--dry-run` is honored end-to-end with
        // no special-casing: the S3 listing path short-circuits under dry-run
        // (so the loop below sees no objects and deletes nothing), and
        // `remove_bucket` is itself a no-op under dry-run.
        //
        // Note: `list_v2` does not surface object versions / delete-markers, so
        // on a versioning-enabled bucket that still holds versions,
        // `remove_bucket` may fail even with `--force`. Out of scope for #651.
        let mut listrx = s3.list(&url, true);
        let (tx, urlrx) = mpsc::channel::<Url>(256);
        let mut resultrx = s3.multi_delete(urlrx);
        let feeder = tokio::spawn(async move {
            while let Some(obj) = listrx.recv().await {
                if let Some(u) = obj.url {
                    if tx.send(u).await.is_err() {
                        break;
                    }
                }
            }
        });
        let mut had_error = false;
        while let Some(obj) = resultrx.recv().await {
            let key = obj.url.as_ref().map(|u| u.to_string()).unwrap_or_default();
            match obj.err {
                None => crate::output::op_success("rm", &key, None),
                Some(e) => {
                    had_error = true;
                    crate::output::op_error("rm", &key, None, &format!("{e:#}"));
                }
            }
        }
        let _ = feeder.await;
        if had_error {
            anyhow::bail!("rb --force: one or more object deletions failed");
        }
    }

    // Without --force this is unchanged and still fails on a non-empty bucket;
    // with --force the bucket has just been emptied. remove_bucket is a no-op
    // under --dry-run.
    s3.remove_bucket(&name).await?;
    crate::output::op_success("rb", &url.to_string(), None);
    Ok(())
}
