//! Per-open-handle write-back cache file.
//!
//! S3 objects are immutable and written whole, but POSIX lets a program open a
//! file and write at arbitrary offsets before closing. We bridge the two by
//! backing every write-opened file with a local cache file: writes (and reads
//! of written data) go to that file via `pwrite`/`pread`, and the whole file is
//! uploaded to S3 on flush/close via the existing `S3::upload` (which already
//! picks a single PUT or a concurrent multipart upload by size). This also
//! gives correct random writes, append, and truncate for free.

use std::fs::File;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};

use crate::storage::s3::S3;
use crate::storage::url::Url;
use crate::storage::Metadata;

pub struct Writer {
    s3: S3,
    file: File,
    /// Local cache file path (removed on drop).
    path: PathBuf,
    /// Destination object URL.
    dst: Url,
    size: u64,
    dirty: bool,
}

impl Writer {
    /// Opens a writer backed by `path` (already created/truncated/populated by
    /// the caller). `dirty` should be true for a freshly created/truncated file
    /// so it is uploaded on flush even if never written to.
    pub fn new(s3: S3, file: File, path: PathBuf, dst: Url, size: u64, dirty: bool) -> Self {
        Writer {
            s3,
            file,
            path,
            dst,
            size,
            dirty,
        }
    }

    pub fn size(&self) -> u64 {
        self.size
    }

    /// Writes `data` at `offset`, returning the number of bytes written.
    pub fn write(&mut self, offset: u64, data: &[u8]) -> anyhow::Result<u32> {
        self.file.write_all_at(data, offset)?;
        self.size = self.size.max(offset.saturating_add(data.len() as u64));
        self.dirty = true;
        Ok(data.len() as u32)
    }

    /// Re-points the writer at a new destination key (used by rename so an
    /// in-flight write-back uploads to the new location, not the old one).
    pub fn set_dst(&mut self, dst: Url) {
        self.dst = dst;
    }

    /// Reads up to `size` bytes at `offset` from the cache file (for O_RDWR).
    pub fn read(&self, offset: u64, size: u32) -> anyhow::Result<Vec<u8>> {
        if offset >= self.size {
            return Ok(Vec::new());
        }
        let len = (size as u64).min(self.size - offset) as usize;
        let mut buf = vec![0u8; len];
        let n = self.file.read_at(&mut buf, offset)?;
        buf.truncate(n);
        Ok(buf)
    }

    /// Truncates (or extends) the cache file to `new_size`.
    pub fn truncate(&mut self, new_size: u64) -> anyhow::Result<()> {
        self.file.set_len(new_size)?;
        self.size = new_size;
        self.dirty = true;
        Ok(())
    }

    /// Uploads the cache file to S3 if it has unflushed changes.
    pub async fn flush(&mut self) -> anyhow::Result<()> {
        if !self.dirty {
            return Ok(());
        }
        self.file.sync_all()?;
        self.s3
            .upload(&self.path, &self.dst, &Metadata::default())
            .await?;
        self.dirty = false;
        Ok(())
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        // Only remove the local cache file once its contents are safely in S3.
        // If an upload failed (still dirty), KEEP the file so the bytes aren't
        // lost silently, and log where they are for recovery.
        if self.dirty {
            tracing::error!(
                "unflushed write-back data retained after upload failure: {}",
                self.path.display()
            );
        } else {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// Opens a cache file at `path` for read+write, creating it if necessary.
/// `truncate` empties an existing file (used for create / O_TRUNC).
pub fn open_cache_file(path: &Path, truncate: bool) -> anyhow::Result<File> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(truncate)
        .open(path)?;
    Ok(file)
}
