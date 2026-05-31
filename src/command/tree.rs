//! `tree` — list objects under a prefix as a hierarchical tree (upstream #489).
//!
//! Lists every object/common-prefix under the target (s3:// URL or local path)
//! using the same delimiter-aware listing that `ls`/`du` use, splits each key on
//! `/`, builds a nested [`Node`] tree, and renders it with box-drawing
//! connectors:
//!
//! ```text
//! s3://bucket/
//! ├── a
//! │   ├── b
//! │   │   └── c.txt
//! │   └── d.txt
//! └── e.txt
//! ```
//!
//! Directories (common prefixes) and objects both appear; with `--size` an
//! object's byte size is shown in parentheses. `--depth <N>` caps how many
//! levels are descended; `--limit <N>` caps how many object keys are collected.

use std::collections::BTreeMap;

use clap::Args;

use super::GlobalOpts;
use crate::storage::new_client;
use crate::storage::url::Url;

#[derive(Args, Debug)]
pub struct TreeArgs {
    /// Prefix to walk (s3:// URL or local path). A bucket root (`s3://bucket`)
    /// or trailing-slash prefix lists everything beneath it.
    pub target: String,

    /// Maximum number of levels to descend (unlimited if omitted).
    #[arg(long)]
    pub depth: Option<usize>,

    /// Maximum number of object keys to collect (unlimited if omitted).
    #[arg(long)]
    pub limit: Option<usize>,

    /// Show object sizes in parentheses next to leaf names.
    #[arg(long)]
    pub size: bool,
}

/// A node in the rendered tree. `children` is keyed by path segment so output is
/// deterministic (BTreeMap is sorted). A node is a directory when it has
/// children or its key ended in `/`; otherwise it is a leaf object.
#[derive(Default)]
struct Node {
    children: BTreeMap<String, Node>,
    /// Byte size for a leaf object, if known.
    size: Option<i64>,
    /// True when this entry came from a key that ended in `/` (a directory /
    /// common prefix) even if no children were observed beneath it.
    is_dir: bool,
}

impl Node {
    /// Inserts `segments` (already split on `/`) under this node. The final
    /// non-empty segment carries `size` unless `dir` is set (trailing slash).
    fn insert(&mut self, segments: &[&str], size: Option<i64>, dir: bool) {
        let mut filtered = segments.iter().filter(|s| !s.is_empty()).peekable();
        let mut cur = self;
        while let Some(seg) = filtered.next() {
            let last = filtered.peek().is_none();
            let child = cur.children.entry((*seg).to_string()).or_default();
            if last {
                if dir {
                    child.is_dir = true;
                } else {
                    child.size = size;
                }
            } else {
                child.is_dir = true;
            }
            cur = child;
        }
    }
}

pub async fn run(global: &GlobalOpts, args: TreeArgs) -> anyhow::Result<()> {
    let opts = global.storage_options();
    // The URL shown as the root label and used to build the client.
    let display_url = Url::new(&args.target, crate::storage::url::UrlOptions::default())
        .map_err(|e| anyhow::anyhow!(e))?;

    // Build a recursive *wildcard* listing URL (append `*`), mirroring how `sync`
    // forces a full recursive listing. A wildcard URL has an empty delimiter (so
    // S3 returns every nested key, not one level of common prefixes) AND makes
    // `relative()` return the full key relative to the prefix's parent — which is
    // exactly the per-object path the tree builder splits on. A plain prefix URL
    // would instead relativize each key to only its first segment, collapsing the
    // hierarchy to a single level (the original bug).
    let listing_target = if display_url.is_remote() {
        let base = display_url.absolute();
        if display_url.is_bucket() {
            format!("{base}/*")
        } else if base.ends_with('/') {
            format!("{base}*")
        } else {
            format!("{base}/*")
        }
    } else {
        // Local path: append the recursive wildcard onto the directory.
        let base = display_url.absolute();
        if base.ends_with('/') {
            format!("{base}*")
        } else {
            format!("{base}/*")
        }
    };
    let listing_url = Url::new(&listing_target, crate::storage::url::UrlOptions::default())
        .map_err(|e| anyhow::anyhow!(e))?;

    let client = new_client(&display_url, &opts).await?;

    let mut root = Node::default();
    let mut collected: usize = 0;

    // Recursive listing so we see the full hierarchy, then rebuild the tree
    // locally. Mirrors the `ls`/`du`/`sync` streaming pattern.
    let mut rx = client.list(&listing_url, true);
    while let Some(obj) = rx.recv().await {
        if let Some(err) = obj.err {
            return Err(err);
        }

        let key = obj.url.as_ref().map(|u| u.relative()).unwrap_or_default();
        if key.is_empty() {
            continue;
        }

        let is_dir = obj.typ.is_dir() || key.ends_with('/');
        let segments: Vec<&str> = key.split('/').collect();

        // Honor `--depth`: drop segments deeper than the requested level.
        let segments: Vec<&str> = match args.depth {
            Some(d) if d > 0 => segments.into_iter().take(d).collect(),
            Some(_) => continue, // depth == 0: nothing to show
            None => segments,
        };

        let size = if is_dir { None } else { Some(obj.size) };
        root.insert(&segments, size, is_dir);

        // Honor `--limit` against the number of object (non-dir) keys collected.
        if !is_dir {
            collected += 1;
            if let Some(limit) = args.limit {
                if collected >= limit {
                    break;
                }
            }
        }
    }

    // The root label is the target URL itself (e.g. `s3://bucket/` or a path).
    println!("{}", display_url);
    print_children(&root, "", args.size);
    Ok(())
}

/// Renders the children of `node`, prefixing each line with the accumulated
/// `prefix` of `│   ` / `    ` segments and the appropriate connector.
fn print_children(node: &Node, prefix: &str, show_size: bool) {
    let n = node.children.len();
    for (i, (name, child)) in node.children.iter().enumerate() {
        let last = i + 1 == n;
        let connector = if last { "└── " } else { "├── " };
        let line = format_node(name, child, show_size);
        println!("{prefix}{connector}{line}");
        let child_prefix = format!("{prefix}{}", if last { "    " } else { "│   " });
        print_children(child, &child_prefix, show_size);
    }
}

/// Formats a single node's label: its name, plus an optional size for leaves.
fn format_node(name: &str, node: &Node, show_size: bool) -> String {
    let is_dir = node.is_dir || !node.children.is_empty();
    if !is_dir && show_size {
        if let Some(sz) = node.size {
            return format!("{name} ({sz})");
        }
    }
    name.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collect_output(root: &Node, show_size: bool) -> String {
        // Re-implements the println path into a String so we can assert on it.
        fn rec(node: &Node, prefix: &str, show_size: bool, out: &mut String) {
            let n = node.children.len();
            for (i, (name, child)) in node.children.iter().enumerate() {
                let last = i + 1 == n;
                let connector = if last { "└── " } else { "├── " };
                let line = format_node(name, child, show_size);
                out.push_str(&format!("{prefix}{connector}{line}\n"));
                let child_prefix =
                    format!("{prefix}{}", if last { "    " } else { "│   " });
                rec(child, &child_prefix, show_size, out);
            }
        }
        let mut s = String::new();
        rec(root, "", show_size, &mut s);
        s
    }

    fn build(keys: &[(&str, i64, bool)]) -> Node {
        let mut root = Node::default();
        for (key, size, is_dir) in keys {
            let segs: Vec<&str> = key.split('/').collect();
            let sz = if *is_dir { None } else { Some(*size) };
            root.insert(&segs, sz, *is_dir);
        }
        root
    }

    #[test]
    fn builds_nested_tree_with_connectors() {
        let root = build(&[
            ("a/b/c.txt", 3, false),
            ("a/d.txt", 5, false),
            ("e.txt", 7, false),
        ]);
        let out = collect_output(&root, false);
        // Sorted: `a` before `e`; under `a`, `b` before `d.txt`.
        assert!(out.contains("├── a"), "{out}");
        assert!(out.contains("└── e.txt"), "{out}");
        assert!(out.contains("c.txt"), "{out}");
        assert!(out.contains("d.txt"), "{out}");
        // Deeper levels use the vertical / blank indentation.
        assert!(out.contains("│   "), "{out}");
        // Last child of root uses the corner connector.
        assert!(out.lines().last().unwrap().starts_with("└── e.txt"), "{out}");
    }

    #[test]
    fn last_child_uses_corner_connector() {
        let root = build(&[("only.txt", 1, false)]);
        let out = collect_output(&root, false);
        assert_eq!(out, "└── only.txt\n");
    }

    #[test]
    fn directory_prefix_creates_dir_node() {
        // A trailing-slash key is a common prefix (directory) with no size.
        let root = build(&[("dir/", 0, true)]);
        let out = collect_output(&root, false);
        assert_eq!(out, "└── dir\n");
    }

    #[test]
    fn size_is_shown_for_leaves_when_requested() {
        let root = build(&[("a/b.txt", 42, false)]);
        let out = collect_output(&root, true);
        assert!(out.contains("└── b.txt (42)"), "{out}");
        // The intermediate directory `a` carries no size.
        assert!(out.contains("└── a\n") || out.starts_with("└── a"), "{out}");
        assert!(!out.contains("a ("), "{out}");
    }

    #[test]
    fn intermediate_segments_become_directories() {
        // Only a deep file is inserted; intermediate `x`/`y` must appear as dirs.
        let root = build(&[("x/y/z.txt", 9, false)]);
        let out = collect_output(&root, false);
        assert!(out.contains("└── x"), "{out}");
        assert!(out.contains("└── y"), "{out}");
        assert!(out.contains("└── z.txt"), "{out}");
    }
}
