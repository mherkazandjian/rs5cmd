//! Downloads land under a temporary sibling name and are renamed over the destination only once
//! complete (F03). Written in place, a failed or interrupted download left a file of the right
//! size with wrong contents, which `sync` then skipped forever (same size, newer mtime).
//! s5cmd does the same (a temporary file in the destination directory, then a rename).

use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// The longest file name the temporary name builds on, so the suffix fits within the usual
/// 255-byte file-name limit.
const MAX_STEM: usize = 200;

/// A file being written for `dst`. Dropped without [`PartialFile::commit`], it removes itself.
/// A rename replaces a destination symlink itself, not the file it points to.
#[derive(Debug)]
pub(crate) struct PartialFile {
    tmp: PathBuf,
    dst: PathBuf,
    committed: bool,
}

impl PartialFile {
    /// Creates `<dst>.<pid>.<seq>.rs5cmd-tmp` next to `dst` (new, never an existing file; the
    /// usual umask applies) and returns it opened for writing.
    pub(crate) fn create(dst: &Path) -> io::Result<(PartialFile, File)> {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let name = dst
            .file_name()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no file name"))?
            .to_string_lossy();
        let mut stem = name.as_ref();
        if stem.len() > MAX_STEM {
            let mut end = MAX_STEM;
            while !stem.is_char_boundary(end) {
                end -= 1;
            }
            stem = &stem[..end];
        }
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let tmp = dst.with_file_name(format!("{stem}.{}.{seq}.rs5cmd-tmp", std::process::id()));
        let file = OpenOptions::new().write(true).create_new(true).open(&tmp)?;
        let guard = PartialFile {
            tmp,
            dst: dst.to_path_buf(),
            committed: false,
        };
        Ok((guard, file))
    }

    /// The temporary path (for setting its mtime before the rename).
    pub(crate) fn path(&self) -> &Path {
        &self.tmp
    }

    /// Renames the finished file over the destination.
    pub(crate) fn commit(mut self) -> io::Result<()> {
        std::fs::rename(&self.tmp, &self.dst)?;
        self.committed = true;
        Ok(())
    }
}

impl Drop for PartialFile {
    fn drop(&mut self) {
        if !self.committed {
            let _ = std::fs::remove_file(&self.tmp);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn commit_replaces_destination() {
        let d = tempfile::tempdir().unwrap();
        let dst = d.path().join("a.txt");
        std::fs::write(&dst, b"old").unwrap();
        let (guard, mut f) = PartialFile::create(&dst).unwrap();
        f.write_all(b"new").unwrap();
        drop(f);
        assert_eq!(std::fs::read(&dst).unwrap(), b"old");
        guard.commit().unwrap();
        assert_eq!(std::fs::read(&dst).unwrap(), b"new");
        assert_eq!(std::fs::read_dir(d.path()).unwrap().count(), 1);
    }

    #[test]
    fn drop_without_commit_removes_temp_and_keeps_destination() {
        let d = tempfile::tempdir().unwrap();
        let dst = d.path().join("a.txt");
        std::fs::write(&dst, b"old").unwrap();
        let (guard, mut f) = PartialFile::create(&dst).unwrap();
        f.write_all(b"partial").unwrap();
        assert!(guard.path().exists());
        drop(guard);
        assert_eq!(std::fs::read(&dst).unwrap(), b"old");
        assert_eq!(std::fs::read_dir(d.path()).unwrap().count(), 1);
    }

    #[test]
    fn long_names_fit() {
        let d = tempfile::tempdir().unwrap();
        let dst = d.path().join(format!("{}é", "x".repeat(250)));
        let (guard, _f) = PartialFile::create(&dst).unwrap();
        assert!(guard.path().file_name().unwrap().len() <= 255);
    }
}
