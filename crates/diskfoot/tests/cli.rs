use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

use serde_json::Value;

fn diskfoot() -> Command {
    Command::new(env!("CARGO_BIN_EXE_diskfoot"))
}

fn run_json(root: &Path, depth: usize) -> Value {
    let output = diskfoot()
        .args(["--json", "--depth", &depth.to_string()])
        .arg(root)
        .output()
        .expect("run diskfoot");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("valid JSON")
}

fn entry<'a>(value: &'a Value, name: &str) -> &'a Value {
    value["entries"]
        .as_array()
        .expect("entries array")
        .iter()
        .find(|entry| entry["name"] == name)
        .unwrap_or_else(|| panic!("entry {name} missing"))
}

#[test]
fn json_walks_a_real_tree_with_a_sparse_file_and_a_hard_link() {
    let dir = tempfile::tempdir().expect("temp tree");
    fs::create_dir_all(dir.path().join("big/empty")).expect("dirs");

    let sparse = dir.path().join("big/sparse.bin");
    fs::write(&sparse, vec![0u8; 4096]).expect("write");
    let file = fs::OpenOptions::new()
        .write(true)
        .open(&sparse)
        .expect("open");
    file.set_len(16 * 1024 * 1024).expect("truncate");
    drop(file);

    fs::write(dir.path().join("big/hard.bin"), vec![b'h'; 1024 * 1024]).expect("write");
    for index in 0..8 {
        let target = dir.path().join(format!("link{index}"));
        fs::create_dir(&target).expect("dir");
        fs::hard_link(dir.path().join("big/hard.bin"), target.join("hard.bin")).expect("hard link");
    }
    fs::write(dir.path().join("small.txt"), b"small").expect("write");

    let report = run_json(dir.path(), 2);
    assert_eq!(report["schema_version"], 1);
    assert_eq!(report["state"], "computed");
    assert_eq!(report["depth"], 2);
    assert_eq!(
        report["path"],
        fs::canonicalize(dir.path())
            .expect("canonical")
            .display()
            .to_string()
    );

    let apparent = report["apparent_bytes"].as_u64().expect("apparent");
    let allocated = report["allocated_bytes"].as_u64().expect("allocated");
    assert!(apparent >= 16 * 1024 * 1024);
    assert!(
        allocated < apparent,
        "allocated {allocated} should be below apparent {apparent}"
    );

    let big = entry(&report, "big");
    let big_apparent = big["apparent_bytes"].as_u64().expect("big apparent");
    let big_allocated = big["allocated_bytes"].as_u64().expect("big allocated");
    assert!(big_apparent >= 16 * 1024 * 1024);
    assert!(big_allocated < big_apparent);

    // Nine names share big/hard.bin's inode. If the walk counted each one, the
    // total would be 8 MiB larger; directory inodes cannot hide that.
    assert!(
        apparent < 16 * 1024 * 1024 + 1024 * 1024 + 2 * 1024 * 1024,
        "hard links look duplicated: apparent {apparent}"
    );

    let empty = &big["entries"]
        .as_array()
        .expect("big entries")
        .iter()
        .find(|entry| entry["name"] == "empty")
        .expect("empty");
    assert!(empty["entries"].as_array().expect("entries").is_empty());
}

#[test]
fn depth_zero_reports_no_entries() {
    let dir = tempfile::tempdir().expect("temp tree");
    fs::write(dir.path().join("a.txt"), b"a").expect("write");
    let report = run_json(dir.path(), 0);
    assert_eq!(report["entries"].as_array().expect("entries").len(), 0);
    assert_eq!(report["complete"], false);
}

#[test]
fn help_and_version_are_available() {
    let help = diskfoot().arg("--help").output().expect("help");
    assert!(help.status.success());
    let text = String::from_utf8_lossy(&help.stdout);
    assert!(text.contains("diskfoot"));
    assert!(text.contains("--json"));
    assert!(text.contains("--depth"));

    let version = diskfoot().arg("--version").output().expect("version");
    assert!(version.status.success());
    assert!(String::from_utf8_lossy(&version.stdout).contains(env!("CARGO_PKG_VERSION")));
}

#[test]
fn an_unreadable_file_is_measured_without_reading_it() {
    // Root bypasses permission bits; there is nothing to prove in that case.
    if unsafe { libc::geteuid() } == 0 {
        return;
    }
    let dir = tempfile::tempdir().expect("temp tree");
    let secret = dir.path().join("secret.bin");
    fs::write(&secret, vec![b'x'; 4096]).expect("write");
    fs::set_permissions(&secret, fs::Permissions::from_mode(0o000)).expect("chmod");

    let report = run_json(dir.path(), 1);
    let measured = entry(&report, "secret.bin");
    assert_eq!(measured["apparent_bytes"].as_u64(), Some(4096));
    assert!(measured["allocated_bytes"].as_u64().unwrap_or(0) >= 4096);

    fs::set_permissions(&secret, fs::Permissions::from_mode(0o644)).expect("chmod");
}

#[test]
fn a_missing_path_is_a_clean_error() {
    let output = diskfoot()
        .args(["--json", "/definitely/not/a/real/path"])
        .output()
        .expect("run");
    assert!(!output.status.success());
    assert!(!output.stderr.is_empty());
}
