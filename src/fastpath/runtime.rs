//! Thread-per-core driver for the fast path: shard a work-list across N pinned
//! monoio runtimes (one io_uring instance per thread), each running many
//! in-flight transfers concurrently. No cross-thread synchronization on the hot
//! path — results flow back over a channel.

use std::path::PathBuf;
use std::sync::mpsc;
use std::thread;

use futures::stream::{FuturesUnordered, StreamExt};

use super::client::{Endpoint, FastClient};
use super::sign::Signer;

/// A single transfer unit.
pub enum Transfer {
    Upload {
        local: PathBuf,
        bucket: String,
        key: String,
    },
    Download {
        bucket: String,
        key: String,
        local: PathBuf,
    },
    /// Server-side (S3-to-S3) copy.
    Copy {
        src_bucket: String,
        src_key: String,
        dst_bucket: String,
        dst_key: String,
    },
}

/// Result of one transfer.
pub struct Outcome {
    pub label: String,
    /// Source operand (local path or s3:// URL), for structured/JSON output.
    pub source: String,
    /// Destination operand, for structured/JSON output.
    pub destination: String,
    pub result: anyhow::Result<()>,
}

/// Fast-path configuration.
pub struct FastConfig {
    pub endpoint: Endpoint,
    pub access_key: String,
    pub secret_key: String,
    pub session_token: Option<String>,
    pub region: String,
    /// Number of OS threads / io_uring instances.
    pub cores: usize,
    /// Max in-flight requests per core.
    pub per_core_concurrency: usize,
    /// Max retry attempts per transfer on transient errors (0 = no retries).
    pub max_retries: u32,
    /// If true, report what would be transferred without doing any network I/O.
    pub dry_run: bool,
    /// If true, delete the source after a successful transfer (move semantics).
    pub is_move: bool,
}

/// Runs all transfers across `cfg.cores` threads, returning every outcome.
pub fn run_transfers(items: Vec<Transfer>, cfg: FastConfig) -> Vec<Outcome> {
    let n = items.len();
    if n == 0 {
        return Vec::new();
    }

    // Dry run: report each transfer's label without any network or io_uring work
    // (so it also works where io_uring is unavailable).
    if cfg.dry_run {
        return items
            .into_iter()
            .map(|item| {
                let (source, destination) = operands(&item);
                Outcome {
                    label: label_for(&item),
                    source,
                    destination,
                    result: Ok(()),
                }
            })
            .collect();
    }

    let cores = cfg.cores.max(1).min(n);

    // Round-robin shard so work is balanced even if sizes vary.
    let mut shards: Vec<Vec<Transfer>> = (0..cores).map(|_| Vec::new()).collect();
    for (i, item) in items.into_iter().enumerate() {
        shards[i % cores].push(item);
    }

    let core_ids = core_affinity::get_core_ids().unwrap_or_default();
    let (tx, rx) = mpsc::channel::<Outcome>();
    let mut handles = Vec::with_capacity(cores);

    for (ci, shard) in shards.into_iter().enumerate() {
        let tx = tx.clone();
        let endpoint = cfg.endpoint.clone();
        let signer = Signer::new(
            cfg.access_key.clone(),
            cfg.secret_key.clone(),
            cfg.session_token.clone(),
            cfg.region.clone(),
        );
        let conc = cfg.per_core_concurrency.max(1);
        let max_retries = cfg.max_retries;
        let is_move = cfg.is_move;
        let pin = core_ids.get(ci).copied();

        handles.push(thread::spawn(move || {
            if let Some(id) = pin {
                core_affinity::set_for_current(id);
            }
            // io_uring when available; falls back to epoll (legacy) otherwise.
            // The timer is required for retry backoff sleeps.
            let mut rt = monoio::RuntimeBuilder::<monoio::FusionDriver>::new()
                .enable_timer()
                .build()
                .expect("build monoio runtime (is io_uring blocked by seccomp?)");
            rt.block_on(async move {
                match FastClient::new(endpoint, signer) {
                    Ok(client) => {
                        run_shard(&client, shard, conc, max_retries, is_move, &tx).await
                    }
                    Err(e) => {
                        // Report the construction failure once per shard item so
                        // the caller surfaces it rather than hanging.
                        let msg = format!("{e:#}");
                        for _ in 0..shard.len().max(1) {
                            let _ = tx.send(Outcome {
                                label: "fastpath client init".to_string(),
                                source: String::new(),
                                destination: String::new(),
                                result: Err(anyhow::anyhow!("{msg}")),
                            });
                        }
                    }
                }
            });
        }));
    }
    drop(tx);

    let mut outcomes = Vec::with_capacity(n);
    while let Ok(o) = rx.recv() {
        outcomes.push(o);
    }
    for h in handles {
        let _ = h.join();
    }
    outcomes
}

/// Drives one shard with a bounded number of in-flight requests.
async fn run_shard(
    client: &FastClient,
    items: Vec<Transfer>,
    concurrency: usize,
    max_retries: u32,
    is_move: bool,
    tx: &mpsc::Sender<Outcome>,
) {
    let mut it = items.into_iter();
    let mut inflight = FuturesUnordered::new();
    for _ in 0..concurrency {
        if let Some(item) = it.next() {
            inflight.push(do_one(client, item, max_retries, is_move));
        }
    }
    while let Some(outcome) = inflight.next().await {
        let _ = tx.send(outcome);
        if let Some(item) = it.next() {
            inflight.push(do_one(client, item, max_retries, is_move));
        }
    }
}

/// The (source, destination) operands for a transfer.
fn operands(item: &Transfer) -> (String, String) {
    match item {
        Transfer::Upload { local, bucket, key } => {
            (local.display().to_string(), format!("s3://{bucket}/{key}"))
        }
        Transfer::Download { bucket, key, local } => {
            (format!("s3://{bucket}/{key}"), local.display().to_string())
        }
        Transfer::Copy {
            src_bucket,
            src_key,
            dst_bucket,
            dst_key,
        } => (
            format!("s3://{src_bucket}/{src_key}"),
            format!("s3://{dst_bucket}/{dst_key}"),
        ),
    }
}

/// The human label for a transfer.
fn label_for(item: &Transfer) -> String {
    let (s, d) = operands(item);
    format!("cp {s} {d}")
}

async fn do_one(client: &FastClient, item: Transfer, max_retries: u32, is_move: bool) -> Outcome {
    let label = label_for(&item);
    let (source, destination) = operands(&item);
    let result = match item {
        Transfer::Upload { local, bucket, key } => {
            // Read the local file once; only the network PUT is retried.
            match read_file(&local).await {
                Ok(data) => {
                    let put = with_retry(max_retries, &label, || {
                        client.put(&bucket, &key, data.clone())
                    })
                    .await;
                    // For `mv`, delete the local source only after a successful upload.
                    match put {
                        Ok(()) if is_move => std::fs::remove_file(&local).map_err(Into::into),
                        other => other,
                    }
                }
                Err(e) => Err(e),
            }
        }
        Transfer::Download { bucket, key, local } => {
            async {
                let bytes =
                    with_retry(max_retries, &label, || client.get(&bucket, &key)).await?;
                write_file(&local, bytes).await?;
                // For `mv`, delete the remote source after a successful download.
                if is_move {
                    with_retry(max_retries, &label, || client.delete(&bucket, &key)).await?;
                }
                Ok(())
            }
            .await
        }
        Transfer::Copy {
            src_bucket,
            src_key,
            dst_bucket,
            dst_key,
        } => {
            async {
                with_retry(max_retries, &label, || {
                    client.copy(&src_bucket, &src_key, &dst_bucket, &dst_key)
                })
                .await?;
                // For `mv`, delete the remote source after a successful copy.
                if is_move {
                    with_retry(max_retries, &label, || client.delete(&src_bucket, &src_key))
                        .await?;
                }
                Ok(())
            }
            .await
        }
    };
    Outcome {
        label,
        source,
        destination,
        result,
    }
}

/// Runs an async operation with up to `max_retries` additional attempts on
/// error, using exponential backoff with a cap. Errors are classified: network
/// failures and retryable HTTP statuses (5xx/408/429) are retried; permanent
/// HTTP errors (e.g. 403/404) fail immediately rather than burning the budget.
async fn with_retry<T, F, Fut>(max_retries: u32, label: &str, mut op: F) -> anyhow::Result<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<T>>,
{
    let mut attempt = 0u32;
    loop {
        match op().await {
            Ok(v) => return Ok(v),
            Err(e) => {
                if attempt >= max_retries || !is_retryable(&e) {
                    return Err(e);
                }
                // Exponential backoff: 50ms, 100ms, 200ms, ... capped at 5s.
                let backoff_ms = (50u64 << attempt.min(7)).min(5_000);
                tracing::debug!(
                    "fastpath retry {}/{} for {label} after error: {e:#}",
                    attempt + 1,
                    max_retries
                );
                monoio::time::sleep(std::time::Duration::from_millis(backoff_ms)).await;
                attempt += 1;
            }
        }
    }
}

/// Whether an error should be retried. HTTP status errors defer to their own
/// classification; any other error (connect/TLS/IO/signing) is treated as
/// transient and retried.
fn is_retryable(e: &anyhow::Error) -> bool {
    match e.downcast_ref::<super::client::HttpStatusError>() {
        Some(h) => h.is_retryable(),
        None => true,
    }
}

/// Reads a whole file asynchronously via io_uring (monoio::fs), so the read
/// overlaps with in-flight network requests instead of blocking the runtime.
async fn read_file(path: &std::path::Path) -> anyhow::Result<bytes::Bytes> {
    let file = monoio::fs::File::open(path).await?;
    let len = file.metadata().await?.len() as usize;
    let buf = Vec::with_capacity(len);
    let (res, mut buf) = file.read_exact_at(buf, 0).await;
    let _ = file.close().await;
    match res {
        Ok(()) => {
            // SAFETY: read_exact_at filled `len` bytes; reflect that in the Vec.
            unsafe { buf.set_len(len) };
            Ok(bytes::Bytes::from(buf))
        }
        Err(e) => Err(e.into()),
    }
}

/// Writes bytes to a file asynchronously via io_uring (monoio::fs).
async fn write_file(path: &std::path::Path, data: bytes::Bytes) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() && !parent.exists() {
            std::fs::create_dir_all(parent)?;
        }
    }
    let file = monoio::fs::File::create(path).await?;
    let (res, _buf) = file.write_all_at(data, 0).await;
    let _ = file.close().await;
    res?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn operands_for_copy_are_s3_urls() {
        let t = Transfer::Copy {
            src_bucket: "src".into(),
            src_key: "a/b.txt".into(),
            dst_bucket: "dst".into(),
            dst_key: "c/d.txt".into(),
        };
        let (s, d) = operands(&t);
        assert_eq!(s, "s3://src/a/b.txt");
        assert_eq!(d, "s3://dst/c/d.txt");
        assert_eq!(label_for(&t), "cp s3://src/a/b.txt s3://dst/c/d.txt");
    }

    #[test]
    fn operands_for_upload_and_download() {
        let up = Transfer::Upload {
            local: PathBuf::from("/tmp/x"),
            bucket: "b".into(),
            key: "k".into(),
        };
        assert_eq!(operands(&up), ("/tmp/x".to_string(), "s3://b/k".to_string()));

        let down = Transfer::Download {
            bucket: "b".into(),
            key: "k".into(),
            local: PathBuf::from("/tmp/y"),
        };
        assert_eq!(operands(&down), ("s3://b/k".to_string(), "/tmp/y".to_string()));
    }
}
