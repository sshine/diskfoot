use std::io;
use std::path::{Path, PathBuf};

/// Everything `diskfoot` can fail with before it starts drawing.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("path contains a NUL byte: {0}")]
    InvalidPath(PathBuf),

    #[error("I/O error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    #[error("not a directory: {0}")]
    NotADirectory(PathBuf),

    #[error("cannot read the mount table: {0}")]
    MountTable(String),

    #[error("JSON serialization failed: {0}")]
    Json(#[from] serde_json::Error),
}

impl Error {
    pub fn io(path: &Path, source: io::Error) -> Self {
        Self::Io {
            path: path.to_path_buf(),
            source,
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;
