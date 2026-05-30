//! Local filesystem `Storage` implementation. Ported from s5cmd's `storage/fs.go`.

use std::fs;
use std::path::Path;

use async_trait::async_trait;
use tokio::sync::mpsc;
use walkdir::WalkDir;

use super::url::{Url, UrlOptions};
use super::{Metadata, Object, ObjectNotFound, ObjectType, Storage};

#[derive(Debug, Clone)]
pub struct Filesystem {
    dry_run: bool,
}

impl Filesystem {
    pub fn new(dry_run: bool) -> Filesystem {
        Filesystem { dry_run }
    }

    fn stat_sync(&self, u: &Url) -> anyhow::Result<Object> {
        let abs = u.absolute();
        let meta = match fs::symlink_metadata(&abs) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(ObjectNotFound(abs).into());
            }
            Err(e) => return Err(e.into()),
        };

        // Resolve type honoring symlinks like Go's os.Stat (follows links) while
        // still reporting symlink-ness for the listing logic.
        let ft = meta.file_type();
        let typ = if ft.is_symlink() {
            ObjectType::Symlink
        } else if ft.is_dir() {
            ObjectType::Dir
        } else {
            ObjectType::File
        };

        // For size/mtime mirror os.Stat which follows symlinks.
        let followed = fs::metadata(&abs).unwrap_or(meta);

        Ok(Object {
            url: Some(u.clone()),
            typ,
            size: followed.len() as i64,
            mod_time: followed.modified().ok(),
            etag: String::new(),
            ..Default::default()
        })
    }
}

#[async_trait]
impl Storage for Filesystem {
    async fn stat(&self, src: &Url) -> anyhow::Result<Object> {
        self.stat_sync(src)
    }

    fn list(&self, src: &Url, follow_symlinks: bool) -> mpsc::Receiver<Object> {
        let (tx, rx) = mpsc::channel::<Object>(128);
        let this = self.clone();
        let src = src.clone();

        tokio::task::spawn_blocking(move || {
            if src.is_wildcard() {
                expand_glob(&this, &src, follow_symlinks, &tx);
                return;
            }

            match this.stat_sync(&src) {
                Ok(obj) if obj.typ.is_dir() => {
                    walk_dir(&this, &src, follow_symlinks, &tx);
                }
                Ok(obj) => {
                    let _ = tx.blocking_send(obj);
                }
                Err(e) => {
                    let _ = tx.blocking_send(Object::with_error(e));
                }
            }
        });

        rx
    }

    async fn delete(&self, src: &Url) -> anyhow::Result<()> {
        if self.dry_run {
            return Ok(());
        }
        fs::remove_file(src.absolute())?;
        Ok(())
    }

    async fn copy(&self, src: &Url, dst: &Url, _metadata: &Metadata) -> anyhow::Result<()> {
        if self.dry_run {
            return Ok(());
        }
        let dir = dst.dir();
        if !dir.is_empty() && dir != "." {
            fs::create_dir_all(&dir)?;
        }
        fs::copy(src.absolute(), dst.absolute())?;
        Ok(())
    }
}

fn expand_glob(f: &Filesystem, src: &Url, follow_symlinks: bool, tx: &mpsc::Sender<Object>) {
    let pattern = src.absolute();
    let matches = match glob::glob(&pattern) {
        Ok(m) => m,
        Err(e) => {
            let _ = tx.blocking_send(Object::with_error(anyhow::anyhow!(e)));
            return;
        }
    };

    let mut found = false;
    for entry in matches {
        let path = match entry {
            Ok(p) => p,
            Err(e) => {
                let _ = tx.blocking_send(Object::with_error(anyhow::anyhow!(e)));
                return;
            }
        };
        found = true;
        let mut fileurl = match Url::new(&path.to_string_lossy(), UrlOptions::default()) {
            Ok(u) => u,
            Err(e) => {
                let _ = tx.blocking_send(Object::with_error(anyhow::anyhow!(e)));
                return;
            }
        };
        fileurl.set_relative(src);

        match f.stat_sync(&fileurl) {
            Ok(obj) if !obj.typ.is_dir() => {
                let _ = tx.blocking_send(obj);
            }
            Ok(_) => walk_dir(f, &fileurl, follow_symlinks, tx),
            Err(e) => {
                let _ = tx.blocking_send(Object::with_error(e));
                return;
            }
        }
    }

    if !found {
        let _ = tx.blocking_send(Object::with_error(anyhow::anyhow!(
            "no match found for {:?}",
            src.to_string()
        )));
    }
}

fn walk_dir(f: &Filesystem, src: &Url, follow_symlinks: bool, tx: &mpsc::Sender<Object>) {
    if !should_process_url(Path::new(&src.absolute()), follow_symlinks) {
        return;
    }

    for entry in WalkDir::new(src.absolute())
        .follow_links(follow_symlinks)
        .into_iter()
    {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                let _ = tx.blocking_send(Object::with_error(anyhow::anyhow!(e)));
                return;
            }
        };
        // We're interested in files only.
        if entry.file_type().is_dir() {
            continue;
        }
        let mut fileurl = match Url::new(&entry.path().to_string_lossy(), UrlOptions::default()) {
            Ok(u) => u,
            Err(e) => {
                let _ = tx.blocking_send(Object::with_error(anyhow::anyhow!(e)));
                return;
            }
        };
        fileurl.set_relative(src);

        if !should_process_url(entry.path(), follow_symlinks) {
            continue;
        }

        match f.stat_sync(&fileurl) {
            Ok(obj) => {
                if tx.blocking_send(obj).is_err() {
                    return;
                }
            }
            Err(e) => {
                let _ = tx.blocking_send(Object::with_error(e));
                return;
            }
        }
    }
}

/// Returns true if the URL should be processed: always for remote/follow, and
/// for local non-symlinks otherwise.
fn should_process_url(path: &Path, follow_symlinks: bool) -> bool {
    if follow_symlinks {
        return true;
    }
    match fs::symlink_metadata(path) {
        Ok(m) => !m.file_type().is_symlink(),
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn url_for(p: &std::path::Path) -> Url {
        Url::parse(&p.to_string_lossy()).unwrap()
    }

    #[tokio::test]
    async fn stat_missing_returns_not_found() {
        let fs = Filesystem::new(false);
        let u = Url::parse("/definitely/not/here/xyz").unwrap();
        let err = fs.stat(&u).await.unwrap_err();
        assert!(err.downcast_ref::<ObjectNotFound>().is_some());
    }

    #[tokio::test]
    async fn stat_and_copy_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("a.txt");
        let mut f = std::fs::File::create(&src).unwrap();
        f.write_all(b"hello").unwrap();

        let fs = Filesystem::new(false);
        let obj = fs.stat(&url_for(&src)).await.unwrap();
        assert_eq!(obj.size, 5);
        assert!(obj.typ.is_regular());

        let dst = dir.path().join("nested/b.txt");
        fs.copy(&url_for(&src), &url_for(&dst), &Metadata::default())
            .await
            .unwrap();
        assert_eq!(std::fs::read(&dst).unwrap(), b"hello");
    }

    #[tokio::test]
    async fn list_directory_walks_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("one.txt"), b"1").unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub/two.txt"), b"22").unwrap();

        let fs = Filesystem::new(false);
        let mut rx = fs.list(&url_for(dir.path()), false);
        let mut names = vec![];
        while let Some(obj) = rx.recv().await {
            assert!(obj.err.is_none(), "unexpected err: {:?}", obj.err);
            names.push(obj.url.unwrap().base());
        }
        names.sort();
        assert_eq!(names, vec!["one.txt", "two.txt"]);
    }

    #[tokio::test]
    async fn list_glob_matches() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), b"1").unwrap();
        std::fs::write(dir.path().join("b.txt"), b"2").unwrap();
        std::fs::write(dir.path().join("c.csv"), b"3").unwrap();

        let fs = Filesystem::new(false);
        let pattern = format!("{}/*.txt", dir.path().to_string_lossy());
        let mut rx = fs.list(&Url::parse(&pattern).unwrap(), false);
        let mut names = vec![];
        while let Some(obj) = rx.recv().await {
            assert!(obj.err.is_none());
            names.push(obj.url.unwrap().base());
        }
        names.sort();
        assert_eq!(names, vec!["a.txt", "b.txt"]);
    }
}
