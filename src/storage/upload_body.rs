//! Upload bodies read from a file in large chunks, under one process-wide budget for the bytes
//! read but not yet sent (item 4 of the optimization review).
//!
//! The SDK's file body reads 4 KiB at a time, each read a round trip through the blocking pool,
//! which made large uploads slower than s5cmd. Larger reads alone made memory grow with the
//! number of parts in flight: hyper queues up to ~400 KiB of a body's chunks per connection, so
//! 96 parts in flight held ~50 MB more at 64 KiB reads. Here every chunk holds a permit from one
//! semaphore until hyper has written and dropped it, so the bytes read ahead stay within the
//! budget however many parts are in flight.

use std::fs::File;
use std::future::Future;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex, OnceLock};
use std::task::{Context, Poll};

use aws_sdk_s3::primitives::ByteStream;
use aws_smithy_types::body::SdkBody;
use bytes::Bytes;
use http_body::{Body, Frame, SizeHint};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// Bytes read from the file per chunk. Measured on 4 GiB to 32 x 128 MiB uploads: 64 KiB reads
/// were up to 25 % slower, 256 KiB no faster and larger in memory.
const CHUNK: usize = 128 * 1024;

/// Bytes read but not yet sent, over every upload in the process. 4 MiB uploads as fast as 8 or
/// 16 MiB on the same rows, with the least memory.
const BUDGET: usize = 4 * 1024 * 1024;

/// One permit per chunk read but not yet sent.
fn budget() -> &'static Arc<Semaphore> {
    static B: OnceLock<Arc<Semaphore>> = OnceLock::new();
    B.get_or_init(|| Arc::new(Semaphore::new(BUDGET / CHUNK)))
}

/// `len` bytes of `path` from `offset`, as a body the SDK can rebuild for a retry.
pub(crate) fn file_range(path: &Path, offset: u64, len: u64) -> ByteStream {
    let path = path.to_path_buf();
    ByteStream::new(SdkBody::retryable(move || {
        SdkBody::from_body_1_x(FileBody::new(path.clone(), offset, len, budget().clone()))
    }))
}

/// A chunk and the budget permit it holds until hyper drops it.
struct Held {
    buf: Vec<u8>,
    _permit: OwnedSemaphorePermit,
}

impl AsRef<[u8]> for Held {
    fn as_ref(&self) -> &[u8] {
        &self.buf
    }
}

type PendingRead = Pin<Box<dyn Future<Output = io::Result<(File, Bytes)>> + Send>>;

struct FileBody {
    path: PathBuf,
    offset: u64,
    left: u64,
    len: u64,
    budget: Arc<Semaphore>,
    /// Opened on the first read, then moved in and out of each blocking read.
    file: Option<File>,
    /// The read in progress. In a Mutex only so the body is Sync (the SDK requires it); it is
    /// reached through `get_mut`, never locked.
    pending: Option<Mutex<PendingRead>>,
}

impl FileBody {
    fn new(path: PathBuf, offset: u64, len: u64, budget: Arc<Semaphore>) -> Self {
        FileBody {
            path,
            offset,
            left: len,
            len,
            budget,
            file: None,
            pending: None,
        }
    }

    fn start_read(&mut self) -> PendingRead {
        let n = (CHUNK as u64).min(self.left) as usize;
        let file = self.file.take();
        let path = self.path.clone();
        let offset = self.offset;
        let budget = self.budget.clone();
        Box::pin(async move {
            let permit = budget.acquire_owned().await.map_err(io::Error::other)?;
            // Allocated here, on a runtime worker, and filled on the blocking pool: a buffer
            // allocated on one of its many threads would leave memory in that thread's heap.
            let mut buf = Vec::with_capacity(n);
            let (file, buf) = tokio::task::spawn_blocking(move || -> io::Result<(File, Vec<u8>)> {
                let mut file = match file {
                    Some(f) => f,
                    None => {
                        let mut f = File::open(&path)?;
                        f.seek(SeekFrom::Start(offset))?;
                        f
                    }
                };
                // Read into spare capacity: no zero-filling of every chunk.
                (&mut file).take(n as u64).read_to_end(&mut buf)?;
                if buf.len() < n {
                    return Err(io::ErrorKind::UnexpectedEof.into());
                }
                Ok((file, buf))
            })
            .await
            .map_err(io::Error::other)??;
            Ok((file, Bytes::from_owner(Held { buf, _permit: permit })))
        })
    }
}

impl Body for FileBody {
    type Data = Bytes;
    type Error = io::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, io::Error>>> {
        let this = &mut *self;
        if this.left == 0 {
            return Poll::Ready(None);
        }
        if this.pending.is_none() {
            let read = this.start_read();
            this.pending = Some(Mutex::new(read));
        }
        let read = this.pending.as_mut().expect("set above");
        let read = read.get_mut().unwrap_or_else(|e| e.into_inner());
        let result = std::task::ready!(read.as_mut().poll(cx));
        this.pending = None;
        match result {
            Ok((file, bytes)) => {
                this.file = Some(file);
                this.offset += bytes.len() as u64;
                this.left -= bytes.len() as u64;
                Poll::Ready(Some(Ok(Frame::data(bytes))))
            }
            Err(e) => {
                this.left = 0;
                Poll::Ready(Some(Err(e)))
            }
        }
    }

    fn is_end_stream(&self) -> bool {
        self.left == 0
    }

    fn size_hint(&self) -> SizeHint {
        SizeHint::with_exact(self.len)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn frames(body: &mut FileBody) -> io::Result<Vec<Bytes>> {
        let mut out = Vec::new();
        while let Some(frame) = std::future::poll_fn(|cx| Pin::new(&mut *body).poll_frame(cx)).await {
            out.push(frame?.into_data().expect("data frame"));
        }
        Ok(out)
    }

    fn file_of(n: usize) -> (tempfile::TempDir, PathBuf, Vec<u8>) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f");
        let data: Vec<u8> = (0..n).map(|i| (i % 251) as u8).collect();
        std::fs::write(&path, &data).unwrap();
        (dir, path, data)
    }

    #[tokio::test]
    async fn reads_the_range_in_chunks() {
        let (_d, path, data) = file_of(3 * CHUNK + 1000);
        let (offset, len) = (500u64, 2 * CHUNK as u64 + 700);
        let mut body = FileBody::new(path, offset, len, Arc::new(Semaphore::new(8)));
        assert_eq!(body.size_hint().exact(), Some(len));
        let got = frames(&mut body).await.unwrap();
        assert_eq!(got.len(), 3);
        assert!(got.iter().all(|b| b.len() <= CHUNK));
        assert_eq!(got.concat(), &data[offset as usize..(offset + len) as usize]);
        assert!(body.is_end_stream());
    }

    #[tokio::test]
    async fn short_file_is_an_error() {
        let (_d, path, _) = file_of(1000);
        let mut body = FileBody::new(path, 0, 2000, Arc::new(Semaphore::new(8)));
        assert_eq!(
            frames(&mut body).await.unwrap_err().kind(),
            io::ErrorKind::UnexpectedEof
        );
    }

    #[tokio::test]
    async fn chunks_hold_budget_until_dropped() {
        let (_d, path, _) = file_of(2 * CHUNK);
        let budget = Arc::new(Semaphore::new(2));
        let mut body = FileBody::new(path, 0, 2 * CHUNK as u64, budget.clone());
        let got = frames(&mut body).await.unwrap();
        assert_eq!(budget.available_permits(), 0);
        drop(got);
        assert_eq!(budget.available_permits(), 2);
    }

    #[tokio::test]
    async fn next_read_waits_while_the_budget_is_spent() {
        let (_d, path, data) = file_of(2 * CHUNK);
        let budget = Arc::new(Semaphore::new(1));
        let mut body = FileBody::new(path, 0, 2 * CHUNK as u64, budget);
        let first = std::future::poll_fn(|cx| Pin::new(&mut body).poll_frame(cx)).await;
        let first = first.unwrap().unwrap().into_data().unwrap();
        let next = std::future::poll_fn(|cx| Pin::new(&mut body).poll_frame(cx));
        let waiting = tokio::time::timeout(std::time::Duration::from_millis(200), next).await;
        assert!(waiting.is_err(), "second chunk read while the first still held the budget");
        // The read waiting for a permit stays in the body, so polling again picks it up.
        drop(first);
        let second = std::future::poll_fn(|cx| Pin::new(&mut body).poll_frame(cx)).await;
        let second = second.unwrap().unwrap().into_data().unwrap();
        assert_eq!(&second[..], &data[CHUNK..]);
    }
}
