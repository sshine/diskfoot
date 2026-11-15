//! Stable, non-interactive JSON output.

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::error::Result;
use crate::fs::{self, FilesystemSummary};
use crate::walk::{self, ErrorNote, Kind, Node, Options};

pub const SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Serialize)]
pub struct JsonReport {
    pub schema_version: u32,
    pub path: PathBuf,
    pub depth: usize,
    pub options: JsonOptions,
    pub filesystem: FilesystemSummary,
    /// Allocated bytes for the root at the requested depth (`st_blocks * 512`).
    pub allocated_bytes: u64,
    /// Apparent bytes, or `null` when a filesystem fast path supplied only used bytes.
    pub apparent_bytes: Option<u64>,
    pub size_source: &'static str,
    pub complete: bool,
    pub entries: Vec<JsonEntry>,
    pub errors: Vec<ErrorNote>,
    pub skipped_other_filesystems: u64,
    pub state: &'static str,
}

#[derive(Debug, Serialize)]
pub struct JsonOptions {
    pub cross_filesystems: bool,
}

#[derive(Debug, Serialize)]
pub struct JsonEntry {
    pub name: String,
    pub path: PathBuf,
    pub kind: Kind,
    pub allocated_bytes: u64,
    pub apparent_bytes: Option<u64>,
    pub size_source: &'static str,
    pub complete: bool,
    pub entries: Vec<JsonEntry>,
    pub errors: Vec<ErrorNote>,
}

/// Compute `depth` levels synchronously. `depth = 0` reports the directory
/// itself only: `entries` is empty and the totals are the directory inode's.
pub fn report(path: &Path, depth: usize, options: Options) -> Result<JsonReport> {
    let result = walk::measure(path, options, Some(depth))?;
    let filesystem = fs::filesystem_summary(path)?;
    Ok(JsonReport {
        schema_version: SCHEMA_VERSION,
        path: path.to_path_buf(),
        depth,
        options: JsonOptions {
            cross_filesystems: options.cross,
        },
        filesystem,
        allocated_bytes: result.root.size.allocated,
        apparent_bytes: result.root.size.apparent,
        size_source: result.root.source.label(),
        complete: result.root.complete,
        entries: result.root.entries.iter().map(json_entry).collect(),
        errors: result.errors,
        skipped_other_filesystems: result.skipped,
        state: "computed",
    })
}

fn json_entry(node: &Node) -> JsonEntry {
    JsonEntry {
        name: node.name.clone(),
        path: node.path.clone(),
        kind: node.kind,
        allocated_bytes: node.size.allocated,
        apparent_bytes: node.size.apparent,
        size_source: node.source.label(),
        complete: node.complete,
        entries: node.entries.iter().map(json_entry).collect(),
        errors: node.errors.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::io::Write;
    use std::path::Path;

    fn fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("temp tree");
        std::fs::create_dir_all(dir.path().join("a/b")).expect("dir");
        let mut file = File::create(dir.path().join("a/b/data.bin")).expect("file");
        file.write_all(&[b'x'; 2048]).expect("write");
        std::fs::write(dir.path().join("top.txt"), b"hello").expect("write");
        dir
    }

    #[test]
    fn report_has_a_stable_shape() {
        let dir = fixture();
        let report = report(dir.path(), 2, Options::default()).expect("report");
        let value = serde_json::to_value(&report).expect("json");
        for key in [
            "schema_version",
            "path",
            "depth",
            "options",
            "filesystem",
            "allocated_bytes",
            "apparent_bytes",
            "size_source",
            "complete",
            "entries",
            "errors",
            "skipped_other_filesystems",
            "state",
        ] {
            assert!(value.get(key).is_some(), "missing key {key}");
        }
        assert_eq!(value["schema_version"], 1);
        assert_eq!(value["depth"], 2);
        assert_eq!(value["state"], "computed");
    }

    #[test]
    fn depth_zero_reports_only_the_directory() {
        let dir = fixture();
        let report = report(dir.path(), 0, Options::default()).expect("report");
        assert!(report.entries.is_empty());
        assert!(!report.complete);
        assert!(report.allocated_bytes > 0);
    }

    #[test]
    fn depth_limits_nested_entries() {
        let dir = fixture();
        let one = report(dir.path(), 1, Options::default()).expect("report");
        assert!(one.entries.iter().all(|entry| entry.entries.is_empty()));

        let two = report(dir.path(), 2, Options::default()).expect("report");
        let a = two
            .entries
            .iter()
            .find(|entry| entry.name == "a")
            .expect("a entry");
        assert!(!a.entries.is_empty());
        assert!(a.entries[0].entries.is_empty());
    }

    #[test]
    fn json_is_serializable_and_reparsable() {
        let dir = fixture();
        let report = report(dir.path(), 2, Options::default()).expect("report");
        let text = serde_json::to_string_pretty(&report).expect("serialize");
        let value: serde_json::Value = serde_json::from_str(&text).expect("reparse");
        assert_eq!(value["path"], Path::new(dir.path()).display().to_string());
        assert!(value["filesystem"]["total_bytes"].as_u64().unwrap_or(0) > 0);
    }
}
