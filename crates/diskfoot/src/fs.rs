//! Linux metadata access: `statx`, `statvfs` and the mount table.
//!
//! Nothing here opens a file's contents. `lstat` asks the kernel for basic
//! attributes; `statvfs` asks for the filesystem counters `df` uses.

use std::ffi::CString;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::error::{Error, Result};

/// The subset of `struct statx` the size engine needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stat {
    pub dev: u64,
    pub ino: u64,
    pub nlink: u64,
    pub mode: u32,
    pub size: u64,
    /// 512-byte blocks actually allocated, as `du` reports them.
    pub blocks: u64,
}

impl Stat {
    pub const fn allocated_bytes(&self) -> u64 {
        self.blocks.saturating_mul(512)
    }

    pub fn is_dir(&self) -> bool {
        self.mode & libc::S_IFMT == libc::S_IFDIR
    }

    pub fn is_symlink(&self) -> bool {
        self.mode & libc::S_IFMT == libc::S_IFLNK
    }

    pub fn is_file(&self) -> bool {
        self.mode & libc::S_IFMT == libc::S_IFREG
    }
}

fn c_path(path: &Path) -> Result<CString> {
    CString::new(path.as_os_str().as_bytes()).map_err(|_| Error::InvalidPath(path.to_path_buf()))
}

/// `statx` with `AT_SYMLINK_NOFOLLOW`, falling back to `lstat` on kernels that
/// predate `statx`.
pub fn lstat(path: &Path) -> Result<Stat> {
    let raw = c_path(path)?;
    // SAFETY: `raw` is a valid NUL-terminated path and `buf` points to a
    // correctly sized, zeroed `statx` structure owned by this frame.
    let mut buf: libc::statx = unsafe { std::mem::zeroed() };
    let rc = unsafe {
        libc::statx(
            libc::AT_FDCWD,
            raw.as_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
            libc::STATX_BASIC_STATS,
            &mut buf,
        )
    };

    if rc == 0 {
        return Ok(Stat {
            dev: libc::makedev(buf.stx_dev_major, buf.stx_dev_minor),
            ino: buf.stx_ino,
            nlink: u64::from(buf.stx_nlink),
            mode: buf.stx_mode.into(),
            size: buf.stx_size,
            blocks: buf.stx_blocks,
        });
    }

    let err = std::io::Error::last_os_error();
    if err.raw_os_error() != Some(libc::ENOSYS) {
        return Err(Error::io(path, err));
    }

    let meta = fs::symlink_metadata(path).map_err(|e| Error::io(path, e))?;
    Ok(Stat {
        dev: meta.dev(),
        ino: meta.ino(),
        nlink: meta.nlink(),
        mode: meta.mode(),
        size: meta.size(),
        blocks: meta.blocks(),
    })
}

/// One line of `/proc/self/mounts`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountEntry {
    pub source: String,
    pub mount_point: PathBuf,
    pub fs_type: String,
}

#[derive(Debug, Clone, Default)]
pub struct Mounts {
    entries: Vec<MountEntry>,
}

impl Mounts {
    pub fn read() -> Result<Self> {
        let text = fs::read_to_string("/proc/self/mounts")
            .map_err(|e| Error::MountTable(e.to_string()))?;
        Ok(Self::parse(&text))
    }

    pub fn parse(text: &str) -> Self {
        let mut entries = Vec::new();
        for line in text.lines() {
            let fields: Vec<&str> = line.split_whitespace().collect();
            if fields.len() < 3 {
                continue;
            }
            entries.push(MountEntry {
                source: unescape_mount(fields[0]),
                mount_point: PathBuf::from(unescape_mount(fields[1])),
                fs_type: fields[2].to_string(),
            });
        }
        // Longest mount point first, so a nested mount beats its parent.
        entries.sort_by_key(|e| std::cmp::Reverse(e.mount_point.as_os_str().len()));
        Self { entries }
    }

    /// The mount containing `path`, chosen by longest matching mount point.
    pub fn find(&self, path: &Path) -> Option<&MountEntry> {
        self.entries
            .iter()
            .find(|entry| path.starts_with(&entry.mount_point))
    }
}

/// `/proc/self/mounts` escapes space, tab, newline and backslash as octal.
fn unescape_mount(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 3 < bytes.len() {
            let octal = &bytes[i + 1..i + 4];
            if octal.iter().all(|b| (b'0'..=b'7').contains(b)) {
                let value = octal
                    .iter()
                    .fold(0u8, |acc, b| acc.wrapping_mul(8).wrapping_add(b - b'0'));
                out.push(value);
                i += 4;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The `df -h` header, computed without walking anything.
#[derive(Debug, Clone, Serialize)]
pub struct FilesystemSummary {
    pub mount_point: PathBuf,
    pub fs_type: String,
    pub total_bytes: u64,
    pub used_bytes: u64,
    pub available_bytes: u64,
    pub use_percent: f64,
}

pub fn filesystem_summary(path: &Path) -> Result<FilesystemSummary> {
    let raw = c_path(path)?;
    // SAFETY: `raw` is a valid path and `stats` is a correctly sized, zeroed
    // `statvfs` structure owned by this frame.
    let mut stats: libc::statvfs = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::statvfs(raw.as_ptr(), &mut stats) };
    if rc != 0 {
        return Err(Error::io(path, std::io::Error::last_os_error()));
    }

    let fragment = if stats.f_frsize == 0 {
        stats.f_bsize as u64
    } else {
        stats.f_frsize as u64
    };
    let total = (stats.f_blocks as u64).saturating_mul(fragment);
    let free = (stats.f_bfree as u64).saturating_mul(fragment);
    let available = (stats.f_bavail as u64).saturating_mul(fragment);
    let used = total.saturating_sub(free);

    // df's denominator: space in use plus space a user may still allocate.
    let denominator = used.saturating_add(available);
    let percent = if denominator == 0 {
        0.0
    } else {
        ((used as f64 / denominator as f64) * 1000.0).round() / 10.0
    };

    let mounts = Mounts::read()?;
    let mount = mounts.find(path);

    Ok(FilesystemSummary {
        mount_point: mount
            .map(|m| m.mount_point.clone())
            .unwrap_or_else(|| PathBuf::from("/")),
        fs_type: mount
            .map(|m| m.fs_type.clone())
            .unwrap_or_else(|| "unknown".to_string()),
        total_bytes: total,
        used_bytes: used,
        available_bytes: available,
        use_percent: percent,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unescapes_spaces_in_mount_points() {
        assert_eq!(unescape_mount("/mnt/with\\040space"), "/mnt/with space");
        assert_eq!(unescape_mount("/plain"), "/plain");
    }

    #[test]
    fn longest_mount_point_wins() {
        let mounts = Mounts::parse(
            "/dev/sda1 / ext4 rw 0 0\n/dev/sdb1 /home ext4 rw 0 0\n/dev/sdc1 /home/user xfs rw 0 0\n",
        );
        let found = mounts.find(Path::new("/home/user/project"));
        assert_eq!(found.map(|m| m.fs_type.as_str()), Some("xfs"));
        let found = mounts.find(Path::new("/srv"));
        assert_eq!(found.map(|m| m.fs_type.as_str()), Some("ext4"));
    }

    #[test]
    fn root_mount_matches_everything() {
        let mounts = Mounts::parse("/dev/sda1 / ext4 rw 0 0\n");
        assert!(mounts.find(Path::new("/a/b/c")).is_some());
    }

    #[test]
    fn summary_of_current_directory_is_plausible() {
        let summary = filesystem_summary(Path::new(".")).expect("statvfs works");
        assert!(summary.total_bytes > 0);
        assert!(summary.used_bytes <= summary.total_bytes);
        assert!((0.0..=100.0).contains(&summary.use_percent));
    }
}
