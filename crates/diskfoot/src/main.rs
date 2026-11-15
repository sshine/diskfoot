// These docs are spliced into README.md as its CLI section; the prose around
// it is hand-written in README.tpl at the repository root.

//! `df`, `du` and `ncdu` in one lazy disk-usage tool.
//!
//! The first screen is cheap: `statvfs` fills the filesystem header and a
//! directory read plus `statx` fills the child rows. A child's subtree is walked
//! only when you expand it, in a background worker, and the result is cached per
//! directory and option set.
//!
//! ## Usage
//!
//! ```console
//! $ diskfoot              # interactive TUI for the current directory
//! $ diskfoot /var         # interactive TUI for /var
//! $ diskfoot --json --depth 2 /var
//! ```
//!
//! `--json` is the non-interactive mode: it computes synchronously, emits a
//! stable object (see the JSON schema in the README), and never starts the TUI.
//! `--depth 0` reports the directory itself only; `--depth 1` adds its immediate
//! children; and so on. `--cross` includes filesystems other than the one the
//! target lives on; the default is one filesystem.
//!
//! ## TUI keys
//!
//! | Key | Action |
//! | --- | --- |
//! | `q` | quit (or close help) |
//! | `?` | help |
//! | `enter`, `→`, `l` | expand the selected directory |
//! | `←`, `h` | collapse the selected directory, or move to its parent |
//! | `↑` `↓`, `k` `j` | move the selection |
//! | `r` | refresh: clear the cache, re-list and re-measure |
//! | `a` | toggle allocated (`st_blocks`) and apparent (`st_size`) sizes |
//! | `x` | toggle crossing filesystem boundaries |
//!
//! Rows begin as `pending`. A directory that has not been expanded has not been
//! walked; a `partial` row could not be fully measured, whether because the JSON
//! depth limit was reached, an entry lived on another filesystem, or a read
//! failed; an `error` row could not be read, and the reason appears in the
//! status line.
//!
//! ## Size engine
//!
//! Sizes come from metadata only: `st_blocks * 512` for allocated bytes and
//! `st_size` for apparent bytes. File contents are never read, symlinks are
//! never followed, sparse files report their real allocation, and hard links
//! are counted once per `(dev, ino)` within a walk.
//!
//! ## Exit codes
//!
//! `0` on success, `1` on an I/O or mount-table error, `2` when the arguments
//! do not parse.

use std::process::ExitCode;

use clap::Parser;
use diskfoot::cli::Args;
use diskfoot::error::{Error, Result};
use diskfoot::fs;
use diskfoot::json;
use diskfoot::tui;
use diskfoot::walk::Options;

fn main() -> ExitCode {
    let args = Args::parse();
    match run(args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("diskfoot: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: Args) -> Result<()> {
    let path = std::fs::canonicalize(&args.path).map_err(|error| Error::io(&args.path, error))?;
    let stat = fs::lstat(&path)?;
    if !stat.is_dir() {
        return Err(Error::NotADirectory(path));
    }

    let options = Options {
        allocated: true,
        cross: args.cross,
    };

    if args.json {
        let report = json::report(&path, args.depth, options)?;
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        tui::run(&path, options)?;
    }
    Ok(())
}
