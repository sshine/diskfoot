use std::path::PathBuf;

use clap::Parser;

#[derive(Debug, Parser)]
#[command(
    name = "diskfoot",
    version,
    about = "df, du and ncdu in one lazy disk-usage tool",
    long_about = None
)]
pub struct Args {
    /// Directory to inspect (default: the current directory)
    #[arg(default_value = ".")]
    pub path: PathBuf,

    /// Print a stable JSON report instead of starting the TUI
    #[arg(long)]
    pub json: bool,

    /// Recursion depth for --json; 0 is the directory itself only
    #[arg(long, default_value_t = 1, value_name = "N")]
    pub depth: usize,

    /// Include entries on other filesystems
    #[arg(long)]
    pub cross: bool,
}
