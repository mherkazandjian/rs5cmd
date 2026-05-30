//! Per-open-handle chunked read-ahead reader.
//!
//! FUSE issues reads in small (~128 KiB) slices. Doing one ranged GET per slice
//! is correct (the Phase 1 fallback) but slow for large objects. This reader
//! instead fetches the object in larger **chunks** (default 4 MiB), keeps a
//! bounded **LRU** of recent chunks per handle (sized by `--buffer-size`),
//! fetches any missing chunks **concurrently** (`buffer_unordered`, mirroring
//! `S3::download_remaining_parts`), and **prefetches ahead** on sequential
//! access so the next reads hit the cache.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;

use futures::stream::{self, StreamExt};

use crate::storage::s3::S3;
use crate::storage::url::Url;

pub struct ChunkReader {
    s3: S3,
    url: Url,
    size: u64,
    chunk_size: u64,
    /// Max chunks resident in the per-handle cache.
    max_chunks: usize,
    concurrency: usize,
    chunks: HashMap<u64, Arc<Vec<u8>>>,
    /// LRU order; front is the least-recently-used chunk index.
    order: VecDeque<u64>,
    /// End offset of the previous read, for sequential-access detection.
    last_end: u64,
}

impl ChunkReader {
    pub fn new(
        s3: S3,
        url: Url,
        size: u64,
        chunk_size: u64,
        buffer_size: u64,
        concurrency: usize,
    ) -> Self {
        let chunk_size = chunk_size.max(64 * 1024);
        // Keep at least 2 chunks (current + one read-ahead) resident.
        let max_chunks = ((buffer_size / chunk_size) as usize).max(2);
        ChunkReader {
            s3,
            url,
            size,
            chunk_size,
            max_chunks,
            concurrency: concurrency.max(1),
            chunks: HashMap::new(),
            order: VecDeque::new(),
            last_end: 0,
        }
    }

    fn n_chunks(&self) -> u64 {
        self.size.div_ceil(self.chunk_size)
    }

    /// Byte range `[start, end)` covered by chunk `idx` (clamped to EOF).
    fn chunk_bounds(&self, idx: u64) -> (u64, u64) {
        let start = idx * self.chunk_size;
        let end = ((idx + 1) * self.chunk_size).min(self.size);
        (start, end)
    }

    /// Reads up to `len` bytes at `offset`.
    pub async fn read(&mut self, offset: u64, len: u32) -> anyhow::Result<Vec<u8>> {
        if offset >= self.size || len == 0 {
            return Ok(Vec::new());
        }
        let len = (len as u64).min(self.size - offset);
        let first = offset / self.chunk_size;
        let last = (offset + len - 1) / self.chunk_size;

        // On sequential access, prefetch ahead to fill the cache window.
        let sequential = self.last_end == 0 || offset == self.last_end;
        let mut want_end = last + 1;
        if sequential {
            want_end = want_end.max(first + self.max_chunks as u64);
        }
        // Never plan a window larger than the cache or past EOF.
        want_end = want_end
            .min(first + self.max_chunks as u64)
            .min(self.n_chunks());

        let missing: Vec<u64> = (first..want_end)
            .filter(|i| !self.chunks.contains_key(i))
            .collect();
        if !missing.is_empty() {
            for (idx, data) in self.fetch_chunks(&missing).await? {
                self.chunks.insert(idx, Arc::new(data));
                self.touch(idx);
            }
        }

        // Mark the covering chunks most-recently-used and evict down to budget.
        let keep: HashSet<u64> = (first..=last).collect();
        for i in first..=last {
            self.touch(i);
        }
        self.evict(&keep);

        // Assemble the requested bytes from the (now resident) chunks.
        let mut out = Vec::with_capacity(len as usize);
        let mut pos = offset;
        let end = offset + len;
        while pos < end {
            let idx = pos / self.chunk_size;
            let (cstart, _) = self.chunk_bounds(idx);
            let chunk = self
                .chunks
                .get(&idx)
                .ok_or_else(|| anyhow::anyhow!("chunk {idx} missing after fetch"))?;
            let within = (pos - cstart) as usize;
            if within >= chunk.len() {
                break; // defensive: short object
            }
            let take = ((end - pos) as usize).min(chunk.len() - within);
            out.extend_from_slice(&chunk[within..within + take]);
            pos += take as u64;
        }
        self.last_end = end;
        Ok(out)
    }

    /// Fetches the given chunk indices concurrently via ranged GETs.
    async fn fetch_chunks(&self, idxs: &[u64]) -> anyhow::Result<Vec<(u64, Vec<u8>)>> {
        // Pre-compute owned (idx, offset, len) so the future-producing closure
        // captures only owned clones, not `&self` (avoids an HRTB error when
        // this runs inside the fuse3 trait's async `read`).
        let s3 = self.s3.clone();
        let url = self.url.clone();
        let reqs: Vec<(u64, u64, u64)> = idxs
            .iter()
            .map(|&idx| {
                let (start, end) = self.chunk_bounds(idx);
                (idx, start, end - start)
            })
            .collect();
        let tasks = reqs.into_iter().map(|(idx, start, len)| {
            let s3 = s3.clone();
            let url = url.clone();
            async move {
                let data = s3.read_range(&url, start, len).await?;
                anyhow::Ok((idx, data))
            }
        });
        stream::iter(tasks)
            .buffer_unordered(self.concurrency)
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .collect()
    }

    fn touch(&mut self, idx: u64) {
        if let Some(pos) = self.order.iter().position(|&x| x == idx) {
            self.order.remove(pos);
        }
        self.order.push_back(idx);
    }

    /// Evicts least-recently-used chunks until within budget, never dropping a
    /// chunk in `keep` (the ones the current read needs).
    fn evict(&mut self, keep: &HashSet<u64>) {
        let mut guard = self.order.len();
        while self.chunks.len() > self.max_chunks && guard > 0 {
            guard -= 1;
            let Some(idx) = self.order.pop_front() else {
                break;
            };
            if keep.contains(&idx) {
                self.order.push_back(idx); // can't evict; rotate
                continue;
            }
            self.chunks.remove(&idx);
        }
    }
}
