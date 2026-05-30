//! Inode table: a bidirectional map between FUSE inode numbers and S3 keys.
//!
//! S3 has no inodes, so we synthesize them. The root is always [`ROOT_INODE`].
//! Each file maps to its full key (relative to the bucket); each directory maps
//! to its prefix, which always ends with `/` (the root/base prefix may be `""`
//! or `"some/prefix/"`). Keys are unambiguous because directory keys carry the
//! trailing slash and file keys do not.

use std::collections::HashMap;

/// The fixed root inode number required by the FUSE protocol.
pub const ROOT_INODE: u64 = 1;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum NodeKind {
    Dir,
    File,
}

impl NodeKind {
    pub fn is_dir(self) -> bool {
        matches!(self, NodeKind::Dir)
    }
}

#[derive(Clone, Debug)]
pub struct Node {
    /// Full S3 key (relative to the bucket). Directories end with `/`.
    pub key: String,
    pub kind: NodeKind,
    /// Parent inode (the root's parent is itself).
    pub parent: u64,
}

pub struct InodeTable {
    nodes: HashMap<u64, Node>,
    by_key: HashMap<String, u64>,
    next: u64,
}

impl InodeTable {
    /// Builds a table rooted at `root_key` (the mount's base prefix; `""` for a
    /// whole bucket, or e.g. `"data/"` for a sub-prefix).
    pub fn new(root_key: String) -> Self {
        let mut nodes = HashMap::new();
        let mut by_key = HashMap::new();
        nodes.insert(
            ROOT_INODE,
            Node {
                key: root_key.clone(),
                kind: NodeKind::Dir,
                parent: ROOT_INODE,
            },
        );
        by_key.insert(root_key, ROOT_INODE);
        InodeTable {
            nodes,
            by_key,
            next: ROOT_INODE + 1,
        }
    }

    pub fn get(&self, ino: u64) -> Option<Node> {
        self.nodes.get(&ino).cloned()
    }

    /// Returns the inode currently mapped to `key`, if any (without allocating).
    pub fn lookup_key(&self, key: &str) -> Option<u64> {
        self.by_key.get(key).copied()
    }

    /// Returns the inode for `key`, allocating a fresh one if it is unknown.
    /// `kind`/`parent` are only used when allocating.
    pub fn intern(&mut self, key: String, kind: NodeKind, parent: u64) -> u64 {
        if let Some(&ino) = self.by_key.get(&key) {
            return ino;
        }
        let ino = self.next;
        self.next += 1;
        self.nodes.insert(
            ino,
            Node {
                key: key.clone(),
                kind,
                parent,
            },
        );
        self.by_key.insert(key, ino);
        ino
    }

    /// Re-points an inode at a new key (used by rename in later phases).
    #[allow(dead_code)] // wired up by rename in Phase 3
    pub fn rekey(&mut self, ino: u64, new_key: String) {
        if let Some(node) = self.nodes.get_mut(&ino) {
            let old = std::mem::replace(&mut node.key, new_key.clone());
            self.by_key.remove(&old);
            self.by_key.insert(new_key, ino);
        }
    }

    /// Drops an inode mapping (used by unlink/rmdir in later phases).
    #[allow(dead_code)] // wired up by unlink/rmdir in Phase 3
    pub fn forget(&mut self, ino: u64) {
        if let Some(node) = self.nodes.remove(&ino) {
            self.by_key.remove(&node.key);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_is_interned() {
        let t = InodeTable::new("data/".into());
        let root = t.get(ROOT_INODE).unwrap();
        assert_eq!(root.key, "data/");
        assert!(root.kind.is_dir());
    }

    #[test]
    fn intern_is_stable() {
        let mut t = InodeTable::new(String::new());
        let a = t.intern("a.txt".into(), NodeKind::File, ROOT_INODE);
        let b = t.intern("sub/".into(), NodeKind::Dir, ROOT_INODE);
        assert_ne!(a, b);
        // Re-interning the same key returns the same inode.
        assert_eq!(a, t.intern("a.txt".into(), NodeKind::File, ROOT_INODE));
        assert_eq!(t.get(a).unwrap().key, "a.txt");
        assert!(t.get(b).unwrap().kind.is_dir());
    }

    #[test]
    fn rekey_and_forget() {
        let mut t = InodeTable::new(String::new());
        let a = t.intern("a.txt".into(), NodeKind::File, ROOT_INODE);
        t.rekey(a, "b.txt".into());
        assert_eq!(t.get(a).unwrap().key, "b.txt");
        assert_eq!(a, t.intern("b.txt".into(), NodeKind::File, ROOT_INODE));
        t.forget(a);
        assert!(t.get(a).is_none());
    }
}
