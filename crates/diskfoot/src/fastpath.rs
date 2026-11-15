//! Filesystem-specific summaries for btrfs, ZFS and XFS.
//!
//! On ext4 (and every other filesystem without a fast path) `measure` returns
//! `None` and the generic `statx` walk runs. The detection and every parser are
//! pure functions so they can be tested with fixture output on a host that has
//! none of these filesystems.
//!
//! The fast paths supply allocated bytes only. ZFS and XFS do not expose a
//! directory's apparent size without walking it, and btrfs' `du` reports
//! allocated extents. `walk::Size.apparent` is therefore `None` for a node
//! measured this way; the TUI shows `—` and the JSON schema allows `null`.

use std::ffi::CString;
use std::path::{Path, PathBuf};

use crate::fs::Mounts;
use crate::walk::SizeSource;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FsKind {
    Btrfs,
    Zfs,
    Xfs,
}

impl FsKind {
    pub fn from_fstype(fstype: &str) -> Option<Self> {
        match fstype {
            "btrfs" => Some(Self::Btrfs),
            "zfs" => Some(Self::Zfs),
            "xfs" => Some(Self::Xfs),
            _ => None,
        }
    }

    pub const fn source(self) -> SizeSource {
        match self {
            Self::Btrfs => SizeSource::Btrfs,
            Self::Zfs => SizeSource::Zfs,
            Self::Xfs => SizeSource::Xfs,
        }
    }
}

/// What is known about the target before a command runs.
#[derive(Debug, Clone, Default)]
pub struct PlanContext {
    /// `btrfs subvolume show <path>` succeeded.
    pub btrfs_subvolume: bool,
    /// The ZFS dataset mount point, when `path` is exactly that dataset root.
    pub zfs_dataset_root: Option<PathBuf>,
    /// The XFS project id, when the inode belongs to a project quota.
    pub xfs_project_id: Option<u32>,
    pub mount_point: PathBuf,
    pub block_size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub source: SizeSource,
    pub argv: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Usage {
    pub allocated: u64,
    pub source: SizeSource,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum FastPathError {
    #[error("unexpected output from {command}: {line}")]
    Parse { command: &'static str, line: String },
    #[error("{0} reported no usable size")]
    Empty(&'static str),
}

/// Choose a command for a filesystem, or `None` to fall back to `statx`.
///
/// A btrfs fast path requires the path itself to be a subvolume; a ZFS fast
/// path requires the path to be the dataset root (a subdirectory is not the
/// dataset); an XFS fast path requires a non-zero project id.
pub fn plan_for(kind: FsKind, path: &Path, context: &PlanContext) -> Option<Plan> {
    match kind {
        FsKind::Btrfs if context.btrfs_subvolume => Some(Plan {
            source: SizeSource::Btrfs,
            argv: vec![
                "btrfs".to_string(),
                "filesystem".to_string(),
                "du".to_string(),
                "--raw".to_string(),
                path.display().to_string(),
            ],
        }),
        FsKind::Zfs if context.zfs_dataset_root.as_deref() == Some(path) => Some(Plan {
            source: SizeSource::Zfs,
            argv: vec![
                "zfs".to_string(),
                "list".to_string(),
                "-Hp".to_string(),
                "-o".to_string(),
                "name,used,avail,mountpoint".to_string(),
            ],
        }),
        FsKind::Xfs => {
            let project = context.xfs_project_id?;
            Some(Plan {
                source: SizeSource::Xfs,
                argv: vec![
                    "xfs_quota".to_string(),
                    "-x".to_string(),
                    "-c".to_string(),
                    format!("quota -p -N -n -b -v {project}"),
                    context.mount_point.display().to_string(),
                ],
            })
        }
        _ => None,
    }
}

/// Parse `btrfs filesystem du --raw`. The last data row's first column is the
/// subtree total in bytes.
pub fn parse_btrfs(output: &str) -> Result<u64, FastPathError> {
    let line = output
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .ok_or(FastPathError::Empty("btrfs filesystem du"))?;
    let field = line
        .split_whitespace()
        .next()
        .ok_or_else(|| FastPathError::Parse {
            command: "btrfs filesystem du",
            line: line.to_string(),
        })?;
    field.parse::<u64>().map_err(|_| FastPathError::Parse {
        command: "btrfs filesystem du",
        line: line.to_string(),
    })
}

/// Parse `zfs list -Hp -o name,used,avail,mountpoint`, choosing the dataset
/// whose mount point is the longest prefix of `path`.
pub fn parse_zfs(output: &str, path: &Path) -> Result<u64, FastPathError> {
    let mut best: Option<(usize, u64)> = None;
    for line in output.lines() {
        let fields: Vec<&str> = line.split('\t').collect();
        if fields.len() < 4 {
            continue;
        }
        let mount = Path::new(fields[3]);
        if !path.starts_with(mount) {
            continue;
        }
        let used = fields[1].parse::<u64>().map_err(|_| FastPathError::Parse {
            command: "zfs list",
            line: line.to_string(),
        })?;
        let depth = mount.as_os_str().len();
        if best.is_none_or(|(best_depth, _)| depth > best_depth) {
            best = Some((depth, used));
        }
    }
    best.map(|(_, used)| used)
        .ok_or(FastPathError::Empty("zfs list"))
}

/// Parse `xfs_quota -x -c 'quota -p -N -n -b -v <id>'`. The first column is
/// used blocks.
pub fn parse_xfs(output: &str, block_size: u64) -> Result<u64, FastPathError> {
    let line = output
        .lines()
        .find(|line| !line.trim().is_empty())
        .ok_or(FastPathError::Empty("xfs_quota"))?;
    let field = line
        .split_whitespace()
        .next()
        .ok_or_else(|| FastPathError::Parse {
            command: "xfs_quota",
            line: line.to_string(),
        })?;
    let blocks = field.parse::<u64>().map_err(|_| FastPathError::Parse {
        command: "xfs_quota",
        line: line.to_string(),
    })?;
    Ok(blocks.saturating_mul(block_size))
}

fn command_output(argv: &[String]) -> Option<String> {
    let (program, arguments) = argv.split_first()?;
    let output = std::process::Command::new(program)
        .args(arguments)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Ask the filesystem for a subtree's allocated bytes. Every failure — an
/// unknown filesystem, a missing tool, a command that exits non-zero, output
/// that does not parse — becomes `None`, and the generic walk takes over.
pub fn measure(path: &Path) -> Option<Usage> {
    let mounts = Mounts::read().ok()?;
    let mount = mounts.find(path)?;
    let kind = FsKind::from_fstype(&mount.fs_type)?;

    let mut context = PlanContext {
        mount_point: mount.mount_point.clone(),
        block_size: block_size(path)?,
        ..PlanContext::default()
    };

    match kind {
        FsKind::Btrfs => {
            let argv = vec![
                "btrfs".to_string(),
                "subvolume".to_string(),
                "show".to_string(),
                path.display().to_string(),
            ];
            context.btrfs_subvolume = command_output(&argv).is_some();
        }
        FsKind::Zfs => {
            if path == mount.mount_point {
                context.zfs_dataset_root = Some(mount.mount_point.clone());
            }
        }
        FsKind::Xfs => {
            context.xfs_project_id = xfs_project_id(path);
        }
    }

    let plan = plan_for(kind, path, &context)?;
    let output = command_output(&plan.argv)?;
    let allocated = match plan.source {
        SizeSource::Btrfs => parse_btrfs(&output).ok()?,
        SizeSource::Zfs => parse_zfs(&output, path).ok()?,
        SizeSource::Xfs => parse_xfs(&output, context.block_size).ok()?,
        SizeSource::Statx => return None,
    };
    Some(Usage {
        allocated,
        source: plan.source,
    })
}

fn block_size(path: &Path) -> Option<u64> {
    let raw = CString::new(std::os::unix::ffi::OsStrExt::as_bytes(path.as_os_str())).ok()?;
    // SAFETY: `raw` is a valid path and `stats` is a zeroed statvfs.
    let mut stats: libc::statvfs = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::statvfs(raw.as_ptr(), &mut stats) };
    if rc != 0 {
        return None;
    }
    Some(if stats.f_frsize == 0 {
        stats.f_bsize as u64
    } else {
        stats.f_frsize as u64
    })
}

#[repr(C)]
#[derive(Default)]
struct Fsxattr {
    fsx_xflags: u32,
    fsx_extsize: u32,
    fsx_nextents: u32,
    fsx_projid: u32,
    fsx_cowextsize: u32,
    fsx_pad: [u8; 8],
}

// _IOR('X', 31, struct fsxattr)
const FS_IOC_FSGETXATTR: libc::c_ulong = 0x8000_581f;

fn xfs_project_id(path: &Path) -> Option<u32> {
    let raw = CString::new(std::os::unix::ffi::OsStrExt::as_bytes(path.as_os_str())).ok()?;
    // SAFETY: `raw` is a valid path; the fd is closed below on every path.
    let fd = unsafe {
        libc::open(
            raw.as_ptr(),
            libc::O_RDONLY | libc::O_CLOEXEC | libc::O_DIRECTORY,
        )
    };
    if fd < 0 {
        return None;
    }
    let mut attribute = Fsxattr::default();
    // SAFETY: FS_IOC_FSGETXATTR writes a struct fsxattr into `attribute`.
    let rc = unsafe { libc::ioctl(fd, FS_IOC_FSGETXATTR, &mut attribute) };
    // SAFETY: `fd` was opened above and is not used afterwards.
    let _ = unsafe { libc::close(fd) };
    if rc == 0 && attribute.fsx_projid != 0 {
        Some(attribute.fsx_projid)
    } else {
        None
    }
}

/// The mounted filesystem type for a path, or `None` if the mount table does
/// not cover it.
pub fn fstype(path: &Path) -> Option<String> {
    let mounts = Mounts::read().ok()?;
    mounts.find(path).map(|m| m.fs_type.clone())
}

/// Exposed for diagnostics: whether a generic walk would be used.
pub fn has_fast_path(path: &Path) -> bool {
    fstype(path)
        .as_deref()
        .and_then(FsKind::from_fstype)
        .is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context() -> PlanContext {
        PlanContext {
            mount_point: PathBuf::from("/mnt/data"),
            block_size: 4096,
            ..PlanContext::default()
        }
    }

    #[test]
    fn detects_the_three_filesystems_and_falls_back_for_ext4() {
        assert_eq!(FsKind::from_fstype("btrfs"), Some(FsKind::Btrfs));
        assert_eq!(FsKind::from_fstype("zfs"), Some(FsKind::Zfs));
        assert_eq!(FsKind::from_fstype("xfs"), Some(FsKind::Xfs));
        assert_eq!(FsKind::from_fstype("ext4"), None);
        assert!(!has_fast_path(Path::new(".")));
    }

    #[test]
    fn btrfs_requires_a_subvolume_and_prefers_the_subvolume_du() {
        let path = Path::new("/mnt/data/sub");
        assert!(plan_for(FsKind::Btrfs, path, &context()).is_none());

        let mut ctx = context();
        ctx.btrfs_subvolume = true;
        let plan = plan_for(FsKind::Btrfs, path, &ctx).expect("plan");
        assert_eq!(plan.source, SizeSource::Btrfs);
        assert_eq!(plan.argv[0], "btrfs");
        assert!(plan.argv.contains(&"--raw".to_string()));
    }

    #[test]
    fn zfs_requires_the_dataset_root() {
        let path = Path::new("/mnt/data");
        let mut ctx = context();
        ctx.zfs_dataset_root = Some(path.to_path_buf());
        assert!(plan_for(FsKind::Zfs, path, &ctx).is_some());

        let child = Path::new("/mnt/data/sub");
        assert!(plan_for(FsKind::Zfs, child, &ctx).is_none());
    }

    #[test]
    fn xfs_requires_a_project_id() {
        let path = Path::new("/mnt/data");
        assert!(plan_for(FsKind::Xfs, path, &context()).is_none());

        let mut ctx = context();
        ctx.xfs_project_id = Some(42);
        let plan = plan_for(FsKind::Xfs, path, &ctx).expect("plan");
        assert!(plan.argv[3].contains("42"));
        assert_eq!(plan.argv[4], "/mnt/data");
    }

    #[test]
    fn parses_btrfs_raw_du_fixture() {
        let fake = "Total\tExclusive\tSet shared\tFilename\n\
                    8388608\t6291456\t2097152\t/mnt/data/sub\n";
        assert_eq!(parse_btrfs(fake).expect("parse"), 8_388_608);
    }

    #[test]
    fn parses_zfs_fixture_and_picks_the_longest_mount_point() {
        let fake = "tank\t1000\t2000\t/tank\n\
                    tank/data\t3000\t4000\t/tank/data\n";
        assert_eq!(
            parse_zfs(fake, Path::new("/tank/data/project")).expect("parse"),
            3000
        );
        assert_eq!(
            parse_zfs(fake, Path::new("/tank/other")).expect("parse"),
            1000
        );
    }

    #[test]
    fn parses_xfs_project_quota_fixture_in_blocks() {
        let fake = "2048 0 0 00 [--------]\n";
        assert_eq!(parse_xfs(fake, 4096).expect("parse"), 8_388_608);
    }

    #[test]
    fn bad_output_is_an_error_not_a_panic() {
        assert!(parse_btrfs("no numbers here").is_err());
        assert!(parse_zfs("garbage", Path::new("/")).is_err());
        assert!(parse_xfs("garbage", 4096).is_err());
        assert!(parse_btrfs("").is_err());
    }
}
