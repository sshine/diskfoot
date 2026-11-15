//! The size engine.
//!
//! A walk uses `lstat` for every name it meets, which asks the kernel for
//! `st_blocks` (allocated) and `st_size` (apparent). It never opens a file.
//! Hard links are deduplicated by `(dev, ino)` for every inode whose link count
//! is greater than one, so a file linked into two subdirectories is counted
//! once per walk. Symlinks are not followed.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::error::{Error, Result};
use crate::fastpath;
use crate::fs::{self as fsmeta, Stat};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    File,
    Directory,
    Symlink,
    Other,
}

impl Kind {
    pub fn from_mode(mode: u32) -> Self {
        let masked = mode & libc::S_IFMT;
        if masked == libc::S_IFREG {
            Self::File
        } else if masked == libc::S_IFDIR {
            Self::Directory
        } else if masked == libc::S_IFLNK {
            Self::Symlink
        } else {
            Self::Other
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::File => "file",
            Self::Directory => "dir",
            Self::Symlink => "link",
            Self::Other => "other",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SizeSource {
    /// Generic `statx` walk: both allocated and apparent bytes are known.
    Statx,
    /// btrfs reported the exclusive bytes for a subvolume.
    Btrfs,
    /// ZFS reported the used bytes for a dataset.
    Zfs,
    /// XFS reported the used bytes for a project quota.
    Xfs,
}

impl SizeSource {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Statx => "statx",
            Self::Btrfs => "btrfs",
            Self::Zfs => "zfs",
            Self::Xfs => "xfs",
        }
    }

    pub const fn is_fast_path(self) -> bool {
        !matches!(self, Self::Statx)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Size {
    pub allocated: u64,
    /// `None` when a filesystem fast path reported used bytes only.
    pub apparent: Option<u64>,
}

impl Size {
    pub const fn empty() -> Self {
        Self {
            allocated: 0,
            apparent: Some(0),
        }
    }

    pub fn add(&mut self, other: Self) {
        self.allocated = self.allocated.saturating_add(other.allocated);
        self.apparent = match (self.apparent, other.apparent) {
            (Some(a), Some(b)) => Some(a.saturating_add(b)),
            _ => None,
        };
    }

    pub fn key(self, allocated: bool) -> u64 {
        if allocated {
            self.allocated
        } else {
            self.apparent.unwrap_or(0)
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ErrorNote {
    pub path: PathBuf,
    pub message: String,
}

#[derive(Debug, Clone)]
pub struct Node {
    pub name: String,
    pub path: PathBuf,
    pub kind: Kind,
    pub size: Size,
    pub source: SizeSource,
    pub complete: bool,
    pub entries: Vec<Node>,
    pub skipped_other_filesystems: u64,
    pub errors: Vec<ErrorNote>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Options {
    /// Sort and display allocated bytes (`st_blocks * 512`) rather than apparent.
    pub allocated: bool,
    /// Descend into mounts other than the one the root lives on.
    pub cross: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            allocated: true,
            cross: false,
        }
    }
}

#[derive(Debug, Clone)]
pub struct WalkResult {
    pub root: Node,
    pub errors: Vec<ErrorNote>,
    pub skipped: u64,
}

#[derive(Debug, Clone)]
pub struct ChildSize {
    pub name: String,
    pub path: PathBuf,
    pub kind: Kind,
    pub size: Size,
    pub source: SizeSource,
    pub complete: bool,
    pub skipped_other_filesystem: bool,
    pub errors: Vec<ErrorNote>,
}

#[derive(Debug, Clone, Default)]
pub struct ChildrenResult {
    pub children: Vec<ChildSize>,
    pub errors: Vec<ErrorNote>,
    pub skipped: u64,
}

struct Walk {
    options: Options,
    root_dev: u64,
    seen: HashSet<(u64, u64)>,
}

impl Walk {
    fn new(options: Options, root_dev: u64) -> Self {
        Self {
            options,
            root_dev,
            seen: HashSet::new(),
        }
    }

    /// Account for an inode once. Returns its contribution, zero if this
    /// `(dev, ino)` pair was already counted by this walk.
    fn own_size(&mut self, stat: &Stat) -> Size {
        if stat.nlink > 1 && !self.seen.insert((stat.dev, stat.ino)) {
            return Size::empty();
        }
        Size {
            allocated: stat.allocated_bytes(),
            apparent: Some(stat.size),
        }
    }

    fn node(
        &mut self,
        path: PathBuf,
        stat: Stat,
        remaining: Option<usize>,
        allow_fast: bool,
    ) -> Node {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());
        let kind = Kind::from_mode(stat.mode);
        let mut node = Node {
            name,
            path: path.clone(),
            kind,
            size: self.own_size(&stat),
            source: SizeSource::Statx,
            complete: true,
            entries: Vec::new(),
            skipped_other_filesystems: 0,
            errors: Vec::new(),
        };

        if kind != Kind::Directory {
            return node;
        }

        // A fast path answers "how many bytes does this subtree occupy?" from
        // the filesystem's own accounting, but cannot list children and cannot
        // supply apparent bytes. Only use it for an unbounded, allocated walk.
        if allow_fast
            && remaining.is_none()
            && self.options.allocated
            && !self.options.cross
            && let Some(usage) = fastpath::measure(&path)
        {
            node.size = Size {
                allocated: usage.allocated,
                apparent: None,
            };
            node.source = usage.source;
            node.complete = true;
            return node;
        }

        if remaining == Some(0) {
            node.complete = false;
            return node;
        }

        let directory = match fs::read_dir(&path) {
            Ok(directory) => directory,
            Err(error) => {
                node.errors.push(ErrorNote {
                    path: path.clone(),
                    message: error.to_string(),
                });
                node.complete = false;
                return node;
            }
        };

        let child_depth = remaining.map(|depth| depth.saturating_sub(1));
        for entry in directory {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    node.errors.push(ErrorNote {
                        path: path.clone(),
                        message: error.to_string(),
                    });
                    node.complete = false;
                    continue;
                }
            };
            let child_path = entry.path();
            let child_stat = match fsmeta::lstat(&child_path) {
                Ok(stat) => stat,
                Err(error) => {
                    node.errors.push(ErrorNote {
                        path: child_path,
                        message: error.to_string(),
                    });
                    node.complete = false;
                    continue;
                }
            };
            if child_stat.dev != self.root_dev && !self.options.cross {
                node.skipped_other_filesystems += 1;
                node.complete = false;
                continue;
            }
            let child = self.node(child_path, child_stat, child_depth, false);
            node.size.add(child.size);
            node.complete &= child.complete;
            node.skipped_other_filesystems += child.skipped_other_filesystems;
            node.errors.extend(child.errors.iter().cloned());
            node.entries.push(child);
        }

        sort_nodes(&mut node.entries, self.options.allocated);
        node
    }
}

pub fn sort_nodes(nodes: &mut [Node], allocated: bool) {
    nodes.sort_by(|a, b| {
        b.size
            .key(allocated)
            .cmp(&a.size.key(allocated))
            .then_with(|| a.name.cmp(&b.name))
    });
}

pub fn sort_children(children: &mut [ChildSize], allocated: bool) {
    children.sort_by(|a, b| {
        b.size
            .key(allocated)
            .cmp(&a.size.key(allocated))
            .then_with(|| a.name.cmp(&b.name))
    });
}

fn flatten_errors(node: &Node, out: &mut Vec<ErrorNote>) {
    out.extend(node.errors.iter().cloned());
    for entry in &node.entries {
        flatten_errors(entry, out);
    }
}

/// Measure `path` to `depth`: `None` walks the whole subtree, `Some(0)` stats
/// only the directory itself, `Some(n)` includes `n` levels of children.
pub fn measure(path: &Path, options: Options, depth: Option<usize>) -> Result<WalkResult> {
    let stat = fsmeta::lstat(path)?;
    if !stat.is_dir() {
        return Err(Error::NotADirectory(path.to_path_buf()));
    }
    let mut walk = Walk::new(options, stat.dev);
    let root = walk.node(path.to_path_buf(), stat, depth, true);
    let mut errors = Vec::new();
    flatten_errors(&root, &mut errors);
    Ok(WalkResult {
        skipped: root.skipped_other_filesystems,
        root,
        errors,
    })
}

/// List a directory's immediate children and measure each child's whole
/// subtree. One shared hard-link ledger covers all siblings, so a file linked
/// into two of them is counted once.
pub fn measure_children(dir: &Path, options: Options) -> Result<ChildrenResult> {
    let dir_stat = fsmeta::lstat(dir)?;
    if !dir_stat.is_dir() {
        return Err(Error::NotADirectory(dir.to_path_buf()));
    }
    let mut walk = Walk::new(options, dir_stat.dev);
    let mut result = ChildrenResult::default();

    let directory = match fs::read_dir(dir) {
        Ok(directory) => directory,
        Err(error) => {
            result.errors.push(ErrorNote {
                path: dir.to_path_buf(),
                message: error.to_string(),
            });
            return Ok(result);
        }
    };

    for entry in directory {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                result.errors.push(ErrorNote {
                    path: dir.to_path_buf(),
                    message: error.to_string(),
                });
                continue;
            }
        };
        let path = entry.path();
        let stat = match fsmeta::lstat(&path) {
            Ok(stat) => stat,
            Err(error) => {
                result.errors.push(ErrorNote {
                    path,
                    message: error.to_string(),
                });
                continue;
            }
        };
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());

        if stat.dev != walk.root_dev && !options.cross {
            result.skipped += 1;
            result.children.push(ChildSize {
                name,
                path,
                kind: Kind::from_mode(stat.mode),
                size: Size::empty(),
                source: SizeSource::Statx,
                complete: false,
                skipped_other_filesystem: true,
                errors: Vec::new(),
            });
            continue;
        }

        let node = walk.node(path, stat, None, true);
        result.skipped += node.skipped_other_filesystems;
        let errors = {
            let mut errors = Vec::new();
            flatten_errors(&node, &mut errors);
            errors
        };
        result.children.push(ChildSize {
            name: node.name,
            path: node.path,
            kind: node.kind,
            size: node.size,
            source: node.source,
            complete: node.complete,
            skipped_other_filesystem: false,
            errors,
        });
    }

    sort_children(&mut result.children, options.allocated);
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;

    fn tree(files: &[(&str, usize)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("temp tree");
        for (path, len) in files {
            let full = dir.path().join(path);
            if let Some(parent) = full.parent() {
                fs::create_dir_all(parent).expect("parent");
            }
            let mut file = File::create(&full).expect("file");
            file.write_all(&vec![b'x'; *len]).expect("write");
        }
        dir
    }

    fn file_apparent(node: &Node) -> u64 {
        let own = if node.kind == Kind::File {
            node.size.apparent.unwrap_or(0)
        } else {
            0
        };
        own + node.entries.iter().map(file_apparent).sum::<u64>()
    }

    #[test]
    fn aggregation_sums_nested_files() {
        let dir = tree(&[
            ("a.txt", 1000),
            ("sub/b.txt", 2000),
            ("sub/deep/c.txt", 3000),
        ]);
        let result = measure(dir.path(), Options::default(), None).expect("walk");
        assert_eq!(file_apparent(&result.root), 6000);
        assert!(result.root.size.allocated >= 6000);
        assert_eq!(result.root.entries.len(), 2);
        assert!(result.root.complete);
    }

    #[test]
    fn hard_links_are_counted_once() {
        let dir = tree(&[("one.bin", 1024 * 1024), ("other/.keep", 0)]);
        fs::hard_link(
            dir.path().join("one.bin"),
            dir.path().join("other/link.bin"),
        )
        .expect("hard link");

        let result = measure(dir.path(), Options::default(), None).expect("walk");
        // One 1 MiB inode behind two names. A duplicate would add another MiB;
        // directory inodes cannot account for that much.
        assert_eq!(file_apparent(&result.root), 1024 * 1024);
        assert!(
            result.root.size.apparent.unwrap_or(0) < 1024 * 1024 + 64 * 1024,
            "hard link duplicated: {:?}",
            result.root.size
        );
    }

    #[test]
    fn sparse_files_report_less_allocated_than_apparent() {
        let dir = tempfile::tempdir().expect("temp tree");
        let path = dir.path().join("sparse.bin");
        let mut file = File::create(&path).expect("file");
        file.write_all(&[0u8; 4096]).expect("write");
        file.set_len(8 * 1024 * 1024).expect("truncate");
        drop(file);

        let result = measure(dir.path(), Options::default(), None).expect("walk");
        let sparse = result
            .root
            .entries
            .iter()
            .find(|node| node.name == "sparse.bin")
            .expect("sparse entry");
        assert_eq!(sparse.size.apparent, Some(8 * 1024 * 1024));
        assert!(
            sparse.size.allocated < 8 * 1024 * 1024,
            "allocated {} should be below apparent",
            sparse.size.allocated
        );
    }

    #[test]
    fn depth_limits_what_is_walked() {
        let dir = tree(&[("a/b/c/deep.txt", 100)]);
        let root_only = measure(dir.path(), Options::default(), Some(0)).expect("walk");
        assert!(root_only.root.entries.is_empty());
        assert!(!root_only.root.complete);

        let one = measure(dir.path(), Options::default(), Some(1)).expect("walk");
        assert_eq!(one.root.entries.len(), 1);
        assert!(one.root.entries[0].entries.is_empty());
        assert_eq!(one.root.entries[0].name, "a");

        let two = measure(dir.path(), Options::default(), Some(2)).expect("walk");
        assert_eq!(two.root.entries[0].entries.len(), 1);
        assert_eq!(two.root.entries[0].entries[0].name, "b");
    }

    #[test]
    fn entries_sort_by_size_descending() {
        let dir = tree(&[
            ("small.txt", 1),
            ("large.txt", 100_000),
            ("medium.txt", 10_000),
        ]);
        let result = measure(dir.path(), Options::default(), None).expect("walk");
        let names: Vec<&str> = result
            .root
            .entries
            .iter()
            .map(|n| n.name.as_str())
            .collect();
        assert_eq!(names, vec!["large.txt", "medium.txt", "small.txt"]);
    }

    #[test]
    fn apparent_sort_uses_apparent_bytes() {
        let dir = tree(&[("a.txt", 10), ("b.txt", 100)]);
        let options = Options {
            allocated: false,
            cross: false,
        };
        let result = measure(dir.path(), options, None).expect("walk");
        assert_eq!(result.root.entries[0].name, "b.txt");
    }

    #[test]
    fn symlinks_are_not_followed() {
        let dir = tree(&[("real/file.bin", 10_000)]);
        std::os::unix::fs::symlink(dir.path().join("real"), dir.path().join("link"))
            .expect("symlink");
        let result = measure(dir.path(), Options::default(), None).expect("walk");
        let link = result
            .root
            .entries
            .iter()
            .find(|n| n.name == "link")
            .expect("link entry");
        assert_eq!(link.kind, Kind::Symlink);
        assert!(link.size.apparent.unwrap_or(0) < 10_000);
        let real = result
            .root
            .entries
            .iter()
            .find(|n| n.name == "real")
            .expect("real entry");
        let file = real
            .entries
            .iter()
            .find(|n| n.name == "file.bin")
            .expect("file entry");
        assert_eq!(file.size.apparent, Some(10_000));
    }

    #[test]
    fn unreadable_file_is_measured_without_reading_it() {
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let dir = tree(&[("secret.bin", 8192)]);
        let path = dir.path().join("secret.bin");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).expect("chmod");
        let result = measure(dir.path(), Options::default(), None).expect("walk");
        assert!(result.root.size.apparent.unwrap_or(0) >= 8192);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).expect("chmod");
    }

    #[test]
    fn permission_denied_on_a_directory_is_reported_and_walk_continues() {
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let dir = tree(&[("open.txt", 100), ("closed/hidden.txt", 100)]);
        let closed = dir.path().join("closed");
        fs::set_permissions(&closed, fs::Permissions::from_mode(0o000)).expect("chmod");
        let result = measure(dir.path(), Options::default(), None).expect("walk");
        assert!(!result.errors.is_empty());
        assert!(result.errors.iter().any(|e| e.path == closed));
        let open = result
            .root
            .entries
            .iter()
            .find(|n| n.name == "open.txt")
            .expect("open entry");
        assert_eq!(open.size.apparent, Some(100));
        fs::set_permissions(&closed, fs::Permissions::from_mode(0o755)).expect("chmod");
    }

    #[test]
    fn measure_children_shares_the_hard_link_ledger() {
        let dir = tree(&[("a/one.bin", 1024 * 1024)]);
        fs::create_dir_all(dir.path().join("b")).expect("dir");
        fs::hard_link(dir.path().join("a/one.bin"), dir.path().join("b/two.bin"))
            .expect("hard link");
        let result = measure_children(dir.path(), Options::default()).expect("children");
        let total: u64 = result.children.iter().filter_map(|c| c.size.apparent).sum();
        assert!(
            total < 1024 * 1024 + 64 * 1024,
            "hard link duplicated across siblings: {total}"
        );
    }
}
