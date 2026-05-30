//! The VFS core — deliberately independent of any FUSE binding.
//!
//! It owns the inode table, the attribute and directory caches, the open-handle
//! table (chunked readers and write-back writers), and the read/write paths,
//! translating between the synthesized filesystem namespace and the flat S3
//! keyspace via the reused [`crate::storage::s3::S3`] backend. The fuse3 adapter
//! in [`super::fs`] converts these binding-agnostic [`Attr`]/[`DirEntry`] values
//! into `fuse3` replies, so a `fuser` shim could reuse this core unchanged.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use crate::storage::s3::S3;
use crate::storage::url::Url;
use crate::storage::{Metadata, NoObjectFound, ObjectNotFound, ObjectType, Storage};

use super::inode::{InodeTable, Node, NodeKind, ROOT_INODE};
use super::reader::ChunkReader;
use super::writer::{open_cache_file, Writer};

/// Error mapped to `ENOTEMPTY` for `rmdir` on a non-empty directory.
#[derive(Debug, thiserror::Error)]
#[error("directory not empty")]
pub struct DirNotEmpty;

/// A resolved attribute snapshot for an inode (binding-agnostic).
#[derive(Clone, Copy, Debug)]
pub struct Attr {
    pub ino: u64,
    pub kind: NodeKind,
    pub size: u64,
    pub mtime: SystemTime,
}

/// One directory entry (binding-agnostic).
#[derive(Clone, Debug)]
pub struct DirEntry {
    pub ino: u64,
    pub name: String,
    pub kind: NodeKind,
}

/// Tunable cache lifetimes and presentation settings for the VFS.
#[derive(Clone, Copy, Debug)]
pub struct VfsConfig {
    pub attr_ttl: Duration,
    pub dir_ttl: Duration,
    pub uid: u32,
    pub gid: u32,
    /// Read chunk size in bytes for the per-handle chunked reader.
    pub chunk_size: u64,
    /// Per-handle read-ahead buffer budget in bytes.
    pub buffer_size: u64,
    /// Concurrent chunk fetches per handle.
    pub concurrency: usize,
}

/// What backs an open file handle.
#[derive(Clone)]
enum HandleKind {
    Read(Arc<tokio::sync::Mutex<ChunkReader>>),
    Write(Arc<tokio::sync::Mutex<Writer>>),
}

struct OpenFile {
    ino: u64,
    kind: HandleKind,
}

pub struct Vfs {
    s3: S3,
    bucket: String,
    cfg: VfsConfig,
    /// Directory holding write-back cache files for this mount.
    cache_dir: PathBuf,
    /// Stable timestamp used as the synthetic mtime for directories.
    start_time: SystemTime,
    inodes: Mutex<InodeTable>,
    attr_cache: Mutex<HashMap<u64, (Attr, Instant)>>,
    dir_cache: Mutex<HashMap<u64, (Vec<DirEntry>, Instant)>>,
    handles: Mutex<HashMap<u64, OpenFile>>,
    /// Sizes of files with unflushed local writes; authoritative while open.
    dirty_sizes: Mutex<HashMap<u64, u64>>,
    next_fh: AtomicU64,
}

impl Vfs {
    /// Builds a VFS rooted at `bucket` + `root_prefix` (the prefix is `""` for a
    /// whole bucket, or e.g. `"data/"`; a non-empty prefix must end with `/`).
    /// `cache_dir` holds write-back cache files.
    pub fn new(
        s3: S3,
        bucket: String,
        root_prefix: String,
        cache_dir: PathBuf,
        cfg: VfsConfig,
    ) -> Self {
        Vfs {
            s3,
            bucket,
            cfg,
            cache_dir,
            start_time: SystemTime::now(),
            inodes: Mutex::new(InodeTable::new(root_prefix)),
            attr_cache: Mutex::new(HashMap::new()),
            dir_cache: Mutex::new(HashMap::new()),
            handles: Mutex::new(HashMap::new()),
            dirty_sizes: Mutex::new(HashMap::new()),
            next_fh: AtomicU64::new(0),
        }
    }

    pub fn config(&self) -> VfsConfig {
        self.cfg
    }

    // --- attribute lookups ------------------------------------------------

    /// Returns the attributes of `ino`. Files with unflushed local writes
    /// report their in-progress size; otherwise the attr cache / HeadObject.
    pub async fn getattr(&self, ino: u64) -> anyhow::Result<Attr> {
        let node = self.node(ino)?;
        match node.kind {
            NodeKind::Dir => Ok(self.dir_attr(ino)),
            NodeKind::File => {
                if let Some(size) = self.dirty_size(ino) {
                    return Ok(Attr {
                        ino,
                        kind: NodeKind::File,
                        size,
                        mtime: SystemTime::now(),
                    });
                }
                if let Some(a) = self.cached_attr(ino) {
                    return Ok(a);
                }
                let obj = self.s3.stat(&self.obj_url(&node.key)?).await?;
                let attr = Attr {
                    ino,
                    kind: NodeKind::File,
                    size: obj.size.max(0) as u64,
                    mtime: obj.mod_time.unwrap_or(self.start_time),
                };
                self.store_attr(attr);
                Ok(attr)
            }
        }
    }

    /// Resolves `name` within directory `parent`.
    pub async fn lookup(&self, parent: u64, name: &str) -> anyhow::Result<Attr> {
        let pnode = self.node(parent)?;
        if !pnode.kind.is_dir() {
            return Err(ObjectNotFound(name.to_string()).into());
        }
        let file_key = format!("{}{}", pnode.key, name);

        // A just-created (still-dirty) file may not be in S3 yet; resolve it
        // from the inode table. Bind to a local so the lock guard is dropped
        // before the await below.
        let pending = self.inodes.lock().unwrap().lookup_key(&file_key);
        if let Some(ino) = pending {
            if self.dirty_size(ino).is_some() {
                return self.getattr(ino).await;
            }
        }

        // Fast path: a fresh directory listing is authoritative for this level.
        if let Some(entries) = self.cached_dir(parent) {
            return match entries.iter().find(|e| e.name == name) {
                Some(e) => self.getattr(e.ino).await,
                None => Err(ObjectNotFound(name.to_string()).into()),
            };
        }

        // Slow path: probe S3 directly. Try a file first (one HeadObject)...
        match self.s3.stat(&self.obj_url(&file_key)?).await {
            Ok(obj) => {
                let cino = self.intern(file_key, NodeKind::File, parent);
                let attr = Attr {
                    ino: cino,
                    kind: NodeKind::File,
                    size: obj.size.max(0) as u64,
                    mtime: obj.mod_time.unwrap_or(self.start_time),
                };
                self.store_attr(attr);
                return Ok(attr);
            }
            Err(e) if is_not_found(&e) => {}
            Err(e) => return Err(e),
        }

        // ...otherwise treat it as a directory if anything lives under it.
        let dir_key = format!("{}{}/", pnode.key, name);
        if self.prefix_exists(&dir_key).await? {
            let cino = self.intern(dir_key, NodeKind::Dir, parent);
            return Ok(self.dir_attr(cino));
        }

        Err(ObjectNotFound(name.to_string()).into())
    }

    // --- directory listing ------------------------------------------------

    /// Lists the children of directory `ino` (single level), populating the
    /// inode table and attr/dir caches.
    pub async fn readdir(&self, ino: u64) -> anyhow::Result<Vec<DirEntry>> {
        let node = self.node(ino)?;
        if !node.kind.is_dir() {
            return Err(ObjectNotFound(format!("inode {ino}")).into());
        }
        if let Some(entries) = self.cached_dir(ino) {
            return Ok(entries);
        }

        let mut rx = self.s3.list(&self.list_url(&node.key)?, false);
        let mut entries: Vec<DirEntry> = Vec::new();
        let mut attrs: Vec<Attr> = Vec::new();
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

        while let Some(obj) = rx.recv().await {
            if let Some(err) = obj.err {
                if is_no_object(&err) {
                    break; // empty directory
                }
                return Err(err);
            }
            let Some(u) = &obj.url else { continue };
            let rel = u.path.strip_prefix(&node.key).unwrap_or(&u.path);
            if rel.is_empty() {
                continue; // the directory's own marker object
            }
            match obj.typ {
                ObjectType::Dir => {
                    let name = rel.trim_end_matches('/').to_string();
                    if name.is_empty() || !seen.insert(name.clone()) {
                        continue;
                    }
                    let dir_key = format!("{}{}/", node.key, name);
                    let cino = self.intern(dir_key, NodeKind::Dir, ino);
                    attrs.push(self.dir_attr(cino));
                    entries.push(DirEntry {
                        ino: cino,
                        name,
                        kind: NodeKind::Dir,
                    });
                }
                ObjectType::File => {
                    let name = rel.to_string();
                    if !seen.insert(name.clone()) {
                        continue;
                    }
                    let file_key = format!("{}{}", node.key, name);
                    let cino = self.intern(file_key, NodeKind::File, ino);
                    attrs.push(Attr {
                        ino: cino,
                        kind: NodeKind::File,
                        size: obj.size.max(0) as u64,
                        mtime: obj.mod_time.unwrap_or(self.start_time),
                    });
                    entries.push(DirEntry {
                        ino: cino,
                        name,
                        kind: NodeKind::File,
                    });
                }
                _ => continue,
            }
        }

        for a in &attrs {
            self.store_attr(*a);
        }
        self.store_dir(ino, entries.clone());
        Ok(entries)
    }

    // --- read path --------------------------------------------------------

    /// Direct ranged read of file `ino` (Phase 1 fallback when there is no open
    /// handle). Normal reads go through [`Self::read_handle`].
    pub async fn read(&self, ino: u64, offset: u64, size: u32) -> anyhow::Result<Vec<u8>> {
        let node = self.node(ino)?;
        if node.kind.is_dir() {
            return Err(ObjectNotFound(format!("inode {ino} is a directory")).into());
        }
        let attr = self.getattr(ino).await?;
        if offset >= attr.size {
            return Ok(Vec::new());
        }
        let len = (size as u64).min(attr.size - offset);
        self.s3
            .read_range(&self.obj_url(&node.key)?, offset, len)
            .await
    }

    /// Reads through the handle: a chunked reader for read opens, or the
    /// write-back cache file for write opens. Falls back to a direct ranged GET
    /// for an unknown handle.
    pub async fn read_handle(
        &self,
        ino: u64,
        fh: u64,
        offset: u64,
        size: u32,
    ) -> anyhow::Result<Vec<u8>> {
        let kind = self
            .handles
            .lock()
            .unwrap()
            .get(&fh)
            .map(|of| of.kind.clone());
        match kind {
            Some(HandleKind::Read(r)) => r.lock().await.read(offset, size).await,
            Some(HandleKind::Write(w)) => Ok(w.lock().await.read(offset, size)?),
            None => self.read(ino, offset, size).await,
        }
    }

    // --- open / create ----------------------------------------------------

    /// Opens file `ino`. Read-only opens get a chunked reader; write opens get
    /// a write-back cache file (downloading the current object unless O_TRUNC).
    pub async fn open(&self, ino: u64, flags: i32) -> anyhow::Result<u64> {
        let node = self.node(ino)?;
        if node.kind.is_dir() {
            return Err(ObjectNotFound(format!("inode {ino} is a directory")).into());
        }
        let writing = (flags & libc::O_ACCMODE != libc::O_RDONLY) || (flags & libc::O_TRUNC != 0);
        let fh = self.alloc_fh();

        if !writing {
            let attr = self.getattr(ino).await?;
            let reader = ChunkReader::new(
                self.s3.clone(),
                self.obj_url(&node.key)?,
                attr.size,
                self.cfg.chunk_size,
                self.cfg.buffer_size,
                self.cfg.concurrency,
            );
            self.handles.lock().unwrap().insert(
                fh,
                OpenFile {
                    ino,
                    kind: HandleKind::Read(Arc::new(tokio::sync::Mutex::new(reader))),
                },
            );
            return Ok(fh);
        }

        let dst = self.obj_url(&node.key)?;
        let cache_path = self.cache_dir.join(format!("h{fh}.tmp"));
        let trunc = flags & libc::O_TRUNC != 0;
        let (file, size, dirty) = if trunc {
            (open_cache_file(&cache_path, true)?, 0u64, true)
        } else {
            match self.s3.stat(&dst).await {
                Ok(obj) => {
                    self.s3.download(&dst, &cache_path).await?;
                    (
                        open_cache_file(&cache_path, false)?,
                        obj.size.max(0) as u64,
                        false,
                    )
                }
                Err(e) if is_not_found(&e) => (open_cache_file(&cache_path, true)?, 0, true),
                Err(e) => return Err(e),
            }
        };
        let writer = Writer::new(self.s3.clone(), file, cache_path, dst, size, dirty);
        self.handles.lock().unwrap().insert(
            fh,
            OpenFile {
                ino,
                kind: HandleKind::Write(Arc::new(tokio::sync::Mutex::new(writer))),
            },
        );
        self.set_dirty_size(ino, size);
        self.store_attr(Attr {
            ino,
            kind: NodeKind::File,
            size,
            mtime: SystemTime::now(),
        });
        Ok(fh)
    }

    /// Creates a new (empty) file under `parent` and opens it for writing.
    /// Returns `(inode, fh, attr)`. The object is uploaded on flush/close.
    pub async fn create(&self, parent: u64, name: &str) -> anyhow::Result<(u64, u64, Attr)> {
        let pnode = self.node(parent)?;
        if !pnode.kind.is_dir() {
            return Err(ObjectNotFound(name.to_string()).into());
        }
        let file_key = format!("{}{}", pnode.key, name);
        let ino = self.intern(file_key.clone(), NodeKind::File, parent);
        let fh = self.alloc_fh();
        let cache_path = self.cache_dir.join(format!("h{fh}.tmp"));
        let file = open_cache_file(&cache_path, true)?;
        let writer = Writer::new(
            self.s3.clone(),
            file,
            cache_path,
            self.obj_url(&file_key)?,
            0,
            true,
        );
        self.handles.lock().unwrap().insert(
            fh,
            OpenFile {
                ino,
                kind: HandleKind::Write(Arc::new(tokio::sync::Mutex::new(writer))),
            },
        );
        self.set_dirty_size(ino, 0);
        let attr = Attr {
            ino,
            kind: NodeKind::File,
            size: 0,
            mtime: SystemTime::now(),
        };
        self.store_attr(attr);
        self.invalidate_dir(parent);
        Ok((ino, fh, attr))
    }

    // --- write path -------------------------------------------------------

    /// Writes `data` at `offset` through the handle's write-back cache file.
    pub async fn write_handle(&self, fh: u64, offset: u64, data: &[u8]) -> anyhow::Result<u32> {
        let (ino, w) = self.write_handle_of(fh)?;
        let new_size = {
            let mut g = w.lock().await;
            g.write(offset, data)?;
            g.size()
        };
        self.set_dirty_size(ino, new_size);
        self.store_attr(Attr {
            ino,
            kind: NodeKind::File,
            size: new_size,
            mtime: SystemTime::now(),
        });
        Ok(data.len() as u32)
    }

    /// Flushes (uploads) the handle's cache file if it has unflushed changes.
    pub async fn flush_handle(&self, fh: u64) -> anyhow::Result<()> {
        if let Ok((_, w)) = self.write_handle_of(fh) {
            w.lock().await.flush().await?;
        }
        Ok(())
    }

    /// Releases a handle: a write handle performs its final upload and refreshes
    /// caches; a read handle simply drops its reader.
    pub async fn release(&self, fh: u64) -> anyhow::Result<()> {
        let of = self.handles.lock().unwrap().remove(&fh);
        let Some(of) = of else { return Ok(()) };
        if let HandleKind::Write(w) = of.kind {
            let size = {
                let mut g = w.lock().await;
                g.flush().await?;
                g.size()
            };
            self.clear_dirty_size(of.ino);
            self.store_attr(Attr {
                ino: of.ino,
                kind: NodeKind::File,
                size,
                mtime: SystemTime::now(),
            });
            self.invalidate_dir(self.parent_of(of.ino));
        }
        Ok(())
    }

    /// Applies a size change (truncate). Other attributes (mode/owner/times) are
    /// synthesized and accepted as no-ops.
    pub async fn setattr(
        &self,
        ino: u64,
        size: Option<u64>,
        fh: Option<u64>,
    ) -> anyhow::Result<Attr> {
        if let Some(new_size) = size {
            if let Some(w) = self.find_write_handle(ino, fh) {
                let s = {
                    let mut g = w.lock().await;
                    g.truncate(new_size)?;
                    g.size()
                };
                self.set_dirty_size(ino, s);
                self.store_attr(Attr {
                    ino,
                    kind: NodeKind::File,
                    size: s,
                    mtime: SystemTime::now(),
                });
            } else {
                self.truncate_object(ino, new_size).await?;
            }
        }
        self.getattr(ino).await
    }

    // --- namespace mutations ---------------------------------------------

    /// Removes a file.
    pub async fn unlink(&self, parent: u64, name: &str) -> anyhow::Result<()> {
        let pnode = self.node(parent)?;
        let file_key = format!("{}{}", pnode.key, name);
        self.s3.delete(&self.obj_url(&file_key)?).await?;
        let ino = self.inodes.lock().unwrap().lookup_key(&file_key);
        if let Some(ino) = ino {
            self.invalidate_attr(ino);
            self.clear_dirty_size(ino);
            self.inodes.lock().unwrap().forget(ino);
        }
        self.invalidate_dir(parent);
        Ok(())
    }

    /// Creates a directory (a zero-byte `prefix/` marker object).
    pub async fn mkdir(&self, parent: u64, name: &str) -> anyhow::Result<Attr> {
        let pnode = self.node(parent)?;
        let dir_key = format!("{}{}/", pnode.key, name);
        self.put_empty_object(&dir_key).await?;
        let ino = self.intern(dir_key, NodeKind::Dir, parent);
        self.invalidate_dir(parent);
        Ok(self.dir_attr(ino))
    }

    /// Removes an empty directory.
    pub async fn rmdir(&self, parent: u64, name: &str) -> anyhow::Result<()> {
        let pnode = self.node(parent)?;
        let dir_key = format!("{}{}/", pnode.key, name);
        if !self.dir_is_empty(&dir_key).await? {
            return Err(DirNotEmpty.into());
        }
        // Delete the marker (idempotent if there was only an implicit prefix).
        self.s3.delete(&self.obj_url(&dir_key)?).await?;
        let ino = self.inodes.lock().unwrap().lookup_key(&dir_key);
        if let Some(ino) = ino {
            self.invalidate_dir(ino);
            self.inodes.lock().unwrap().forget(ino);
        }
        self.invalidate_dir(parent);
        Ok(())
    }

    /// Renames a file or directory. S3 has no atomic rename, so this is
    /// copy+delete; a directory rename copies+deletes every key under it
    /// (O(n), non-atomic).
    pub async fn rename(
        &self,
        parent: u64,
        name: &str,
        newparent: u64,
        newname: &str,
    ) -> anyhow::Result<()> {
        let pnode = self.node(parent)?;
        let npnode = self.node(newparent)?;
        let old_file = format!("{}{}", pnode.key, name);
        let new_file = format!("{}{}", npnode.key, newname);

        // File rename.
        match self.s3.stat(&self.obj_url(&old_file)?).await {
            Ok(_) => {
                self.s3
                    .copy(
                        &self.obj_url(&old_file)?,
                        &self.obj_url(&new_file)?,
                        &Metadata::default(),
                    )
                    .await?;
                self.s3.delete(&self.obj_url(&old_file)?).await?;
                let ino = self.inodes.lock().unwrap().lookup_key(&old_file);
                if let Some(ino) = ino {
                    self.inodes.lock().unwrap().rekey(ino, new_file);
                    self.invalidate_attr(ino);
                }
                self.invalidate_dir(parent);
                self.invalidate_dir(newparent);
                return Ok(());
            }
            Err(e) if is_not_found(&e) => {}
            Err(e) => return Err(e),
        }

        // Directory rename: copy+delete the whole subtree.
        let old_prefix = format!("{}{}/", pnode.key, name);
        let new_prefix = format!("{}{}/", npnode.key, newname);
        if !self.prefix_exists(&old_prefix).await? {
            return Err(ObjectNotFound(name.to_string()).into());
        }
        let mut rx = self.s3.list(&self.list_url_recursive(&old_prefix)?, false);
        let mut keys: Vec<String> = Vec::new();
        while let Some(obj) = rx.recv().await {
            if let Some(err) = obj.err {
                if is_no_object(&err) {
                    break;
                }
                return Err(err);
            }
            if let Some(u) = &obj.url {
                keys.push(u.path.clone());
            }
        }
        for k in &keys {
            let suffix = k.strip_prefix(&old_prefix).unwrap_or(k);
            let nk = format!("{new_prefix}{suffix}");
            self.s3
                .copy(&self.obj_url(k)?, &self.obj_url(&nk)?, &Metadata::default())
                .await?;
            self.s3.delete(&self.obj_url(k)?).await?;
        }
        let ino = self.inodes.lock().unwrap().lookup_key(&old_prefix);
        if let Some(ino) = ino {
            self.inodes.lock().unwrap().forget(ino);
        }
        self.invalidate_dir(parent);
        self.invalidate_dir(newparent);
        Ok(())
    }

    /// The parent inode of `ino` (the root is its own parent).
    pub fn parent_of(&self, ino: u64) -> u64 {
        self.node(ino).map(|n| n.parent).unwrap_or(ROOT_INODE)
    }

    // --- helpers ----------------------------------------------------------

    fn node(&self, ino: u64) -> anyhow::Result<Node> {
        self.inodes
            .lock()
            .unwrap()
            .get(ino)
            .ok_or_else(|| ObjectNotFound(format!("inode {ino}")).into())
    }

    fn intern(&self, key: String, kind: NodeKind, parent: u64) -> u64 {
        self.inodes.lock().unwrap().intern(key, kind, parent)
    }

    fn alloc_fh(&self) -> u64 {
        self.next_fh.fetch_add(1, Ordering::Relaxed) + 1
    }

    fn write_handle_of(&self, fh: u64) -> anyhow::Result<(u64, Arc<tokio::sync::Mutex<Writer>>)> {
        let g = self.handles.lock().unwrap();
        match g.get(&fh) {
            Some(OpenFile {
                ino,
                kind: HandleKind::Write(w),
            }) => Ok((*ino, w.clone())),
            Some(_) => Err(anyhow::anyhow!("handle {fh} is not open for writing")),
            None => Err(anyhow::anyhow!("unknown handle {fh}")),
        }
    }

    fn find_write_handle(
        &self,
        ino: u64,
        fh: Option<u64>,
    ) -> Option<Arc<tokio::sync::Mutex<Writer>>> {
        let g = self.handles.lock().unwrap();
        if let Some(fh) = fh {
            if let Some(OpenFile {
                ino: hino,
                kind: HandleKind::Write(w),
            }) = g.get(&fh)
            {
                if *hino == ino {
                    return Some(w.clone());
                }
            }
        }
        g.values().find_map(|of| match &of.kind {
            HandleKind::Write(w) if of.ino == ino => Some(w.clone()),
            _ => None,
        })
    }

    /// Standalone truncate (no open handle): load-or-empty, resize, re-upload.
    async fn truncate_object(&self, ino: u64, new_size: u64) -> anyhow::Result<()> {
        let node = self.node(ino)?;
        let dst = self.obj_url(&node.key)?;
        let id = self.alloc_fh();
        let cache_path = self.cache_dir.join(format!("t{id}.tmp"));
        match self.s3.stat(&dst).await {
            Ok(_) => self.s3.download(&dst, &cache_path).await?,
            Err(e) if is_not_found(&e) => std::fs::write(&cache_path, b"")?,
            Err(e) => return Err(e),
        }
        let file = open_cache_file(&cache_path, false)?;
        file.set_len(new_size)?;
        file.sync_all()?;
        self.s3
            .upload(&cache_path, &dst, &Metadata::default())
            .await?;
        let _ = std::fs::remove_file(&cache_path);
        self.store_attr(Attr {
            ino,
            kind: NodeKind::File,
            size: new_size,
            mtime: SystemTime::now(),
        });
        Ok(())
    }

    /// Uploads a zero-byte object at `key` (directory marker / touch).
    async fn put_empty_object(&self, key: &str) -> anyhow::Result<()> {
        let id = self.alloc_fh();
        let tmp = self.cache_dir.join(format!("e{id}.tmp"));
        std::fs::write(&tmp, b"")?;
        let res = self
            .s3
            .upload(&tmp, &self.obj_url(key)?, &Metadata::default())
            .await;
        let _ = std::fs::remove_file(&tmp);
        res
    }

    fn dir_attr(&self, ino: u64) -> Attr {
        Attr {
            ino,
            kind: NodeKind::Dir,
            size: 0,
            mtime: self.start_time,
        }
    }

    /// Lists `dir_key` (single level) and reports whether anything lives under
    /// it — i.e. whether it should be presented as a directory.
    async fn prefix_exists(&self, dir_key: &str) -> anyhow::Result<bool> {
        let mut rx = self.s3.list(&self.list_url(dir_key)?, false);
        match rx.recv().await {
            Some(obj) => match obj.err {
                Some(err) if is_no_object(&err) => Ok(false),
                Some(err) => Err(err),
                None => Ok(true),
            },
            None => Ok(false),
        }
    }

    /// Whether `dir_key` has no children other than its own marker object.
    async fn dir_is_empty(&self, dir_key: &str) -> anyhow::Result<bool> {
        let mut rx = self.s3.list(&self.list_url(dir_key)?, false);
        while let Some(obj) = rx.recv().await {
            if let Some(err) = obj.err {
                if is_no_object(&err) {
                    return Ok(true);
                }
                return Err(err);
            }
            if let Some(u) = &obj.url {
                if u.path != dir_key {
                    return Ok(false);
                }
            }
        }
        Ok(true)
    }

    /// A single-level listing URL for `prefix` (delimiter `/`, prefix set).
    fn list_url(&self, prefix: &str) -> anyhow::Result<Url> {
        Url::parse(&format!("s3://{}/{}", self.bucket, prefix)).map_err(|e| anyhow::anyhow!(e))
    }

    /// A recursive (no-delimiter) listing URL for `prefix`.
    fn list_url_recursive(&self, prefix: &str) -> anyhow::Result<Url> {
        let mut u = self.list_url(prefix)?;
        u.delimiter.clear();
        Ok(u)
    }

    /// An object URL for `key` (used for HeadObject / ranged GET / put / copy).
    fn obj_url(&self, key: &str) -> anyhow::Result<Url> {
        Url::parse(&format!("s3://{}/{}", self.bucket, key)).map_err(|e| anyhow::anyhow!(e))
    }

    fn cached_attr(&self, ino: u64) -> Option<Attr> {
        let cache = self.attr_cache.lock().unwrap();
        cache
            .get(&ino)
            .and_then(|(a, t)| (t.elapsed() < self.cfg.attr_ttl).then_some(*a))
    }

    fn store_attr(&self, attr: Attr) {
        self.attr_cache
            .lock()
            .unwrap()
            .insert(attr.ino, (attr, Instant::now()));
    }

    fn invalidate_attr(&self, ino: u64) {
        self.attr_cache.lock().unwrap().remove(&ino);
    }

    fn cached_dir(&self, ino: u64) -> Option<Vec<DirEntry>> {
        let cache = self.dir_cache.lock().unwrap();
        cache
            .get(&ino)
            .and_then(|(entries, t)| (t.elapsed() < self.cfg.dir_ttl).then(|| entries.clone()))
    }

    fn store_dir(&self, ino: u64, entries: Vec<DirEntry>) {
        self.dir_cache
            .lock()
            .unwrap()
            .insert(ino, (entries, Instant::now()));
    }

    fn invalidate_dir(&self, ino: u64) {
        self.dir_cache.lock().unwrap().remove(&ino);
    }

    fn set_dirty_size(&self, ino: u64, size: u64) {
        self.dirty_sizes.lock().unwrap().insert(ino, size);
    }

    fn dirty_size(&self, ino: u64) -> Option<u64> {
        self.dirty_sizes.lock().unwrap().get(&ino).copied()
    }

    fn clear_dirty_size(&self, ino: u64) {
        self.dirty_sizes.lock().unwrap().remove(&ino);
    }
}

fn is_not_found(e: &anyhow::Error) -> bool {
    e.downcast_ref::<ObjectNotFound>().is_some()
}

fn is_no_object(e: &anyhow::Error) -> bool {
    e.downcast_ref::<NoObjectFound>().is_some()
}
