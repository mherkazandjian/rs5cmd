//! The fuse3 adapter: a thin translation layer between kernel FUSE operations
//! and the binding-agnostic [`Vfs`] core. It converts [`Attr`]/[`DirEntry`]
//! into `fuse3` replies and maps backend errors to errnos. Keeping this layer
//! thin is what lets a `fuser` shim back the same core on macOS.

use std::ffi::{OsStr, OsString};
use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use fuse3::raw::prelude::*;
use fuse3::Errno;
use futures::stream;

use crate::storage::{NoObjectFound, ObjectNotFound};

use super::inode::NodeKind;
use super::vfs::{Attr, DirNotEmpty, Vfs};

pub struct S3Fuse {
    vfs: Arc<Vfs>,
    ttl: Duration,
    read_only: bool,
    uid: u32,
    gid: u32,
}

impl S3Fuse {
    pub fn new(vfs: Arc<Vfs>, ttl: Duration, read_only: bool) -> Self {
        let cfg = vfs.config();
        S3Fuse {
            vfs,
            ttl,
            read_only,
            uid: cfg.uid,
            gid: cfg.gid,
        }
    }

    fn to_file_attr(&self, a: &Attr) -> FileAttr {
        let (kind, perm, nlink) = match a.kind {
            NodeKind::Dir => (FileType::Directory, 0o755u16, 2u32),
            NodeKind::File => (FileType::RegularFile, 0o644u16, 1u32),
        };
        FileAttr {
            ino: a.ino,
            size: a.size,
            blocks: a.size.div_ceil(512),
            atime: a.mtime.into(),
            mtime: a.mtime.into(),
            ctime: a.mtime.into(),
            kind,
            perm,
            nlink,
            uid: self.uid,
            gid: self.gid,
            rdev: 0,
            blksize: 512,
        }
    }

    /// The full ordered entry list for a directory: `.`, `..`, then children.
    async fn dir_entries(&self, ino: u64) -> Result<Vec<(u64, String, NodeKind)>, Errno> {
        let children = self.vfs.readdir(ino).await.map_err(|e| errno(&e))?;
        let parent = self.vfs.parent_of(ino);
        let mut all = Vec::with_capacity(children.len() + 2);
        all.push((ino, ".".to_string(), NodeKind::Dir));
        all.push((parent, "..".to_string(), NodeKind::Dir));
        for c in children {
            all.push((c.ino, c.name, c.kind));
        }
        Ok(all)
    }
}

impl Filesystem for S3Fuse {
    type DirEntryStream<'a>
        = stream::Iter<std::vec::IntoIter<fuse3::Result<DirectoryEntry>>>
    where
        Self: 'a;
    type DirEntryPlusStream<'a>
        = stream::Iter<std::vec::IntoIter<fuse3::Result<DirectoryEntryPlus>>>
    where
        Self: 'a;

    async fn init(&self, _req: Request) -> fuse3::Result<ReplyInit> {
        Ok(ReplyInit {
            max_write: NonZeroU32::new(128 * 1024).unwrap(),
        })
    }

    async fn destroy(&self, _req: Request) {}

    async fn lookup(&self, _req: Request, parent: u64, name: &OsStr) -> fuse3::Result<ReplyEntry> {
        let name = name.to_str().ok_or_else(|| Errno::from(libc::EINVAL))?;
        let attr = self.vfs.lookup(parent, name).await.map_err(|e| errno(&e))?;
        Ok(ReplyEntry {
            ttl: self.ttl,
            attr: self.to_file_attr(&attr),
            generation: 0,
        })
    }

    async fn getattr(
        &self,
        _req: Request,
        inode: u64,
        _fh: Option<u64>,
        _flags: u32,
    ) -> fuse3::Result<ReplyAttr> {
        let attr = self.vfs.getattr(inode).await.map_err(|e| errno(&e))?;
        Ok(ReplyAttr {
            ttl: self.ttl,
            attr: self.to_file_attr(&attr),
        })
    }

    async fn opendir(&self, _req: Request, _inode: u64, _flags: u32) -> fuse3::Result<ReplyOpen> {
        Ok(ReplyOpen { fh: 0, flags: 0 })
    }

    async fn readdir(
        &self,
        _req: Request,
        parent: u64,
        _fh: u64,
        offset: i64,
    ) -> fuse3::Result<ReplyDirectory<Self::DirEntryStream<'_>>> {
        let all = self.dir_entries(parent).await?;
        let mut out: Vec<fuse3::Result<DirectoryEntry>> = Vec::new();
        for (i, (ino, name, kind)) in all.into_iter().enumerate() {
            if (i as i64) < offset {
                continue;
            }
            out.push(Ok(DirectoryEntry {
                inode: ino,
                kind: fuse_kind(kind),
                name: OsString::from(name),
                offset: (i + 1) as i64,
            }));
        }
        Ok(ReplyDirectory {
            entries: stream::iter(out),
        })
    }

    async fn readdirplus(
        &self,
        _req: Request,
        parent: u64,
        _fh: u64,
        offset: u64,
        _lock_owner: u64,
    ) -> fuse3::Result<ReplyDirectoryPlus<Self::DirEntryPlusStream<'_>>> {
        let all = self.dir_entries(parent).await?;
        let mut out: Vec<fuse3::Result<DirectoryEntryPlus>> = Vec::new();
        for (i, (ino, name, _kind)) in all.into_iter().enumerate() {
            if (i as u64) < offset {
                continue;
            }
            let attr = self.vfs.getattr(ino).await.map_err(|e| errno(&e))?;
            out.push(Ok(DirectoryEntryPlus {
                inode: ino,
                generation: 0,
                kind: fuse_kind(attr.kind),
                name: OsString::from(name),
                offset: (i + 1) as i64,
                attr: self.to_file_attr(&attr),
                entry_ttl: self.ttl,
                attr_ttl: self.ttl,
            }));
        }
        Ok(ReplyDirectoryPlus {
            entries: stream::iter(out),
        })
    }

    async fn open(&self, _req: Request, inode: u64, flags: u32) -> fuse3::Result<ReplyOpen> {
        // Read-only mounts reject any write/append access mode.
        let accmode = (flags as i32) & libc::O_ACCMODE;
        if self.read_only && accmode != libc::O_RDONLY {
            return Err(Errno::from(libc::EROFS));
        }
        let fh = self
            .vfs
            .open(inode, flags as i32)
            .await
            .map_err(|e| errno(&e))?;
        Ok(ReplyOpen { fh, flags: 0 })
    }

    async fn read(
        &self,
        _req: Request,
        inode: u64,
        fh: u64,
        offset: u64,
        size: u32,
    ) -> fuse3::Result<ReplyData> {
        let data = self
            .vfs
            .read_handle(inode, fh, offset, size)
            .await
            .map_err(|e| errno(&e))?;
        Ok(ReplyData {
            data: Bytes::from(data),
        })
    }

    async fn release(
        &self,
        _req: Request,
        _inode: u64,
        fh: u64,
        _flags: u32,
        _lock_owner: u64,
        _flush: bool,
    ) -> fuse3::Result<()> {
        self.vfs.release(fh).await.map_err(|e| errno(&e))?;
        Ok(())
    }

    async fn releasedir(
        &self,
        _req: Request,
        _inode: u64,
        _fh: u64,
        _flags: u32,
    ) -> fuse3::Result<()> {
        Ok(())
    }

    async fn flush(
        &self,
        _req: Request,
        _inode: u64,
        fh: u64,
        _lock_owner: u64,
    ) -> fuse3::Result<()> {
        self.vfs.flush_handle(fh).await.map_err(|e| errno(&e))?;
        Ok(())
    }

    async fn fsync(
        &self,
        _req: Request,
        _inode: u64,
        fh: u64,
        _datasync: bool,
    ) -> fuse3::Result<()> {
        self.vfs.flush_handle(fh).await.map_err(|e| errno(&e))?;
        Ok(())
    }

    async fn create(
        &self,
        _req: Request,
        parent: u64,
        name: &OsStr,
        _mode: u32,
        _flags: u32,
    ) -> fuse3::Result<ReplyCreated> {
        if self.read_only {
            return Err(Errno::from(libc::EROFS));
        }
        let name = name.to_str().ok_or_else(|| Errno::from(libc::EINVAL))?;
        let (_ino, fh, attr) = self.vfs.create(parent, name).await.map_err(|e| errno(&e))?;
        Ok(ReplyCreated {
            ttl: self.ttl,
            attr: self.to_file_attr(&attr),
            generation: 0,
            fh,
            flags: 0,
        })
    }

    async fn write(
        &self,
        _req: Request,
        _inode: u64,
        fh: u64,
        offset: u64,
        data: &[u8],
        _write_flags: u32,
        _flags: u32,
    ) -> fuse3::Result<ReplyWrite> {
        if self.read_only {
            return Err(Errno::from(libc::EROFS));
        }
        let written = self
            .vfs
            .write_handle(fh, offset, data)
            .await
            .map_err(|e| errno(&e))?;
        Ok(ReplyWrite { written })
    }

    async fn setattr(
        &self,
        _req: Request,
        inode: u64,
        fh: Option<u64>,
        set_attr: SetAttr,
    ) -> fuse3::Result<ReplyAttr> {
        if self.read_only && set_attr.size.is_some() {
            return Err(Errno::from(libc::EROFS));
        }
        let attr = self
            .vfs
            .setattr(inode, set_attr.size, fh)
            .await
            .map_err(|e| errno(&e))?;
        Ok(ReplyAttr {
            ttl: self.ttl,
            attr: self.to_file_attr(&attr),
        })
    }

    async fn unlink(&self, _req: Request, parent: u64, name: &OsStr) -> fuse3::Result<()> {
        if self.read_only {
            return Err(Errno::from(libc::EROFS));
        }
        let name = name.to_str().ok_or_else(|| Errno::from(libc::EINVAL))?;
        self.vfs.unlink(parent, name).await.map_err(|e| errno(&e))?;
        Ok(())
    }

    async fn mkdir(
        &self,
        _req: Request,
        parent: u64,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
    ) -> fuse3::Result<ReplyEntry> {
        if self.read_only {
            return Err(Errno::from(libc::EROFS));
        }
        let name = name.to_str().ok_or_else(|| Errno::from(libc::EINVAL))?;
        let attr = self.vfs.mkdir(parent, name).await.map_err(|e| errno(&e))?;
        Ok(ReplyEntry {
            ttl: self.ttl,
            attr: self.to_file_attr(&attr),
            generation: 0,
        })
    }

    async fn rmdir(&self, _req: Request, parent: u64, name: &OsStr) -> fuse3::Result<()> {
        if self.read_only {
            return Err(Errno::from(libc::EROFS));
        }
        let name = name.to_str().ok_or_else(|| Errno::from(libc::EINVAL))?;
        self.vfs.rmdir(parent, name).await.map_err(|e| errno(&e))?;
        Ok(())
    }

    async fn rename(
        &self,
        _req: Request,
        parent: u64,
        name: &OsStr,
        new_parent: u64,
        new_name: &OsStr,
    ) -> fuse3::Result<()> {
        if self.read_only {
            return Err(Errno::from(libc::EROFS));
        }
        let name = name.to_str().ok_or_else(|| Errno::from(libc::EINVAL))?;
        let new_name = new_name.to_str().ok_or_else(|| Errno::from(libc::EINVAL))?;
        self.vfs
            .rename(parent, name, new_parent, new_name)
            .await
            .map_err(|e| errno(&e))?;
        Ok(())
    }
}

fn fuse_kind(k: NodeKind) -> FileType {
    match k {
        NodeKind::Dir => FileType::Directory,
        NodeKind::File => FileType::RegularFile,
    }
}

/// Maps a backend error to a FUSE errno. "Not found" variants become ENOENT;
/// everything else is logged and surfaced as EIO.
fn errno(e: &anyhow::Error) -> Errno {
    if e.downcast_ref::<ObjectNotFound>().is_some() || e.downcast_ref::<NoObjectFound>().is_some() {
        Errno::from(libc::ENOENT)
    } else if e.downcast_ref::<DirNotEmpty>().is_some() {
        Errno::from(libc::ENOTEMPTY)
    } else {
        tracing::warn!("mount op failed: {e:#}");
        Errno::from(libc::EIO)
    }
}
