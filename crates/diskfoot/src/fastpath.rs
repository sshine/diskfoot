//! Filesystem-level accounting for btrfs, ZFS and XFS.
//!
//! These are not recursive directory scans. Each fast path reports a number the
//! filesystem already maintains for a specific boundary, and each is usable only
//! at that boundary:
//!
//! - btrfs: a qgroup row for a **subvolume root**, and only when quotas are
//!   enabled. `rfer` is the referenced bytes, which includes shared extents.
//! - ZFS: the `used` property of a **dataset**, usable only at its mount point.
//! - XFS: a **project quota's** used blocks, which cover the whole project, not
//!   an arbitrary directory subtree.
//!
//! Everywhere else — ext4, a directory inside a subvolume or dataset, a btrfs
//! filesystem without quota groups, a missing tool, a non-zero exit, or output
//! that does not parse — `measure` returns `None` and the exact generic `statx`
//! walk runs.
//!
//! The fast paths supply allocated bytes only. None of them exposes a
//! directory's apparent size without walking it, so `walk::Size.apparent` is
//! `None` for a node measured this way; the TUI shows `—` and the JSON schema
//! allows `null`.

use std::ffi::CString;
use std::path::{Path, PathBuf};

use crate::fs::{self as fsmeta, Mounts};
use crate::walk::SizeSource;

/// The inode number of a btrfs subvolume root (`BTRFS_FIRST_FREE_OBJECTID`).
/// A directory inside a subvolume has a different inode, so this is what makes
/// "the target is the subvolume root" checkable without interpreting the
/// command's free-form first line.
pub const BTRFS_FIRST_FREE_OBJECTID: u64 = 256;

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
}

/// What is known about the target before a command runs.
#[derive(Debug, Clone, Default)]
pub struct PlanContext {
    /// The subvolume id reported by `btrfs subvolume show`, present only when
    /// the target inode is the subvolume root.
    pub btrfs_subvolume_id: Option<u64>,
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
/// A btrfs fast path requires a parsed subvolume id for the target itself; a
/// ZFS fast path requires the path to be the dataset root (a subdirectory is not
/// the dataset); an XFS fast path requires a non-zero project id, and its number
/// covers the whole project.
pub fn plan_for(kind: FsKind, path: &Path, context: &PlanContext) -> Option<Plan> {
    match kind {
        FsKind::Btrfs => {
            context.btrfs_subvolume_id?;
            Some(Plan {
                source: SizeSource::Btrfs,
                argv: vec![
                    "btrfs".to_string(),
                    "qgroup".to_string(),
                    "show".to_string(),
                    "-reF".to_string(),
                    "--raw".to_string(),
                    path.display().to_string(),
                ],
            })
        }
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

/// Parse the `Subvolume ID` line from `btrfs subvolume show`.
pub fn parse_subvolume_show(output: &str) -> Result<u64, FastPathError> {
    for line in output.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("Subvolume ID:") {
            let id = rest
                .trim()
                .parse::<u64>()
                .map_err(|_| FastPathError::Parse {
                    command: "btrfs subvolume show",
                    line: line.to_string(),
                })?;
            if id == 0 {
                return Err(FastPathError::Parse {
                    command: "btrfs subvolume show",
                    line: line.to_string(),
                });
            }
            return Ok(id);
        }
    }
    Err(FastPathError::Empty("btrfs subvolume show"))
}

/// Combine the target's inode with `btrfs subvolume show` output.
///
/// A subvolume root has inode 256. `btrfs subvolume show` also succeeds for a
/// path *inside* a subvolume, reporting the enclosing subvolume, so the inode
/// check is what rejects a directory that is not the root.
pub fn btrfs_subvolume_id(inode: u64, show_output: Option<&str>) -> Option<u64> {
    if inode != BTRFS_FIRST_FREE_OBJECTID {
        return None;
    }
    parse_subvolume_show(show_output?).ok()
}

/// Parse `btrfs qgroup show -reF --raw`, selecting qgroup `0/<subvolume_id>`
/// and reading its `rfer` column.
pub fn parse_qgroup(output: &str, subvolume_id: u64) -> Result<u64, FastPathError> {
    let wanted = format!("0/{subvolume_id}");
    let mut rfer = None;
    for line in output.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.is_empty() {
            continue;
        }
        if fields[0] == "qgroupid" {
            rfer = fields.iter().position(|field| *field == "rfer");
            continue;
        }
        if fields[0] != wanted {
            continue;
        }
        let index = rfer.ok_or_else(|| FastPathError::Parse {
            command: "btrfs qgroup show",
            line: line.to_string(),
        })?;
        let value = fields.get(index).ok_or_else(|| FastPathError::Parse {
            command: "btrfs qgroup show",
            line: line.to_string(),
        })?;
        return value.parse::<u64>().map_err(|_| FastPathError::Parse {
            command: "btrfs qgroup show",
            line: line.to_string(),
        });
    }
    Err(FastPathError::Empty("btrfs qgroup show"))
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

/// Ask the filesystem for the number it already maintains at its own boundary:
/// a btrfs subvolume qgroup, a ZFS dataset, or an XFS project quota.
///
/// This is deliberately not a recursive scan, and it is not a general
/// arbitrary-directory total. Every failure — an unsupported boundary, an
/// unknown filesystem, quotas disabled, a missing tool, a command that exits
/// non-zero, output that does not parse — becomes `None`, and the exact generic
/// `statx` walk takes over.
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
            let show = command_output(&argv);
            let inode = fsmeta::lstat(path).ok()?.ino;
            context.btrfs_subvolume_id = btrfs_subvolume_id(inode, show.as_deref());
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
        SizeSource::Btrfs => parse_qgroup(&output, context.btrfs_subvolume_id?).ok()?,
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
    }

    #[test]
    fn btrfs_requires_a_subvolume_root_and_uses_qgroups() {
        let path = Path::new("/mnt/data/sub");
        assert!(plan_for(FsKind::Btrfs, path, &context()).is_none());

        let mut ctx = context();
        ctx.btrfs_subvolume_id = Some(256);
        let plan = plan_for(FsKind::Btrfs, path, &ctx).expect("plan");
        assert_eq!(plan.source, SizeSource::Btrfs);
        assert_eq!(
            &plan.argv[..5],
            ["btrfs", "qgroup", "show", "-reF", "--raw"]
        );
        assert_eq!(plan.argv[5], "/mnt/data/sub");
        assert!(!plan.argv.iter().any(|arg| arg == "du"));
    }

    const SUBVOLUME_SHOW: &str = r#"subvol
	Name: 			subvol
	UUID: 			11111111-2222-3333-4444-555555555555
	Subvolume ID: 		256
	Parent ID: 		5
"#;

    const QGROUPS: &str = r#"qgroupid         rfer         excl     max_rfer     max_excl
--------         ----         ----     --------     --------
0/5          16384        16384            0            0
0/256      8388608      6291456            0            0
"#;

    #[test]
    fn a_directory_inside_a_subvolume_is_not_the_subvolume_root() {
        assert_eq!(
            btrfs_subvolume_id(BTRFS_FIRST_FREE_OBJECTID, Some(SUBVOLUME_SHOW)),
            Some(256)
        );
        // `btrfs subvolume show` also succeeds for a directory inside the
        // subvolume, but only the root has inode 256.
        assert_eq!(btrfs_subvolume_id(257, Some(SUBVOLUME_SHOW)), None);
        assert_eq!(btrfs_subvolume_id(BTRFS_FIRST_FREE_OBJECTID, None), None);

        let ctx = PlanContext {
            btrfs_subvolume_id: None,
            ..context()
        };
        assert!(plan_for(FsKind::Btrfs, Path::new("/mnt/data/sub"), &ctx).is_none());
    }

    #[test]
    fn parses_btrfs_subvolume_and_qgroup_fixtures() {
        assert_eq!(
            parse_subvolume_show(SUBVOLUME_SHOW).expect("subvolume id"),
            256
        );
        // The qgroup for the subvolume, not for the top-level 0/5.
        assert_eq!(parse_qgroup(QGROUPS, 256).expect("rfer"), 8_388_608);
        assert_eq!(parse_qgroup(QGROUPS, 5).expect("rfer"), 16_384);
        assert!(parse_qgroup(QGROUPS, 999).is_err());
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
    fn parses_xfs_project_quota_fixture_in_blocks() {
        let fake = "2048 0 0 00 [--------]\n";
        assert_eq!(parse_xfs(fake, 4096).expect("parse"), 8_388_608);
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
    fn bad_output_is_an_error_not_a_panic() {
        assert!(parse_subvolume_show("no id here").is_err());
        assert!(parse_subvolume_show("Subvolume ID: not-a-number").is_err());
        assert!(parse_subvolume_show("Subvolume ID: 0").is_err());
        assert!(parse_qgroup("garbage", 256).is_err());
        assert!(parse_qgroup("qgroupid rfer\n0/256 not-a-number", 256).is_err());
        assert!(parse_zfs("garbage", Path::new("/")).is_err());
        assert!(parse_xfs("garbage", 4096).is_err());
        assert!(parse_qgroup("", 256).is_err());
    }
}
