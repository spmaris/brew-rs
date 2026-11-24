//! Typed failures of the on-disk bottle and cask cache cleanup.

use std::io;
use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum CacheError {
    #[error("Failed to read cache directory: {}", path.display())]
    ReadDir { path: PathBuf, source: io::Error },
    #[error("Failed to read an entry in cache directory: {}", path.display())]
    ReadEntry { path: PathBuf, source: io::Error },
    #[error("Failed to inspect cache entry: {}", path.display())]
    InspectEntry { path: PathBuf, source: io::Error },
    #[error("Failed to remove cache file: {}", path.display())]
    RemoveFile { path: PathBuf, source: io::Error },
    #[error("Failed to inspect cache directory: {}", path.display())]
    InspectDir { path: PathBuf, source: io::Error },
    #[error(
        "Refusing to clean '{}': the bottles directory is a symlink. \
         Remove the link or point storage.cache_dir at a real directory.",
        path.display()
    )]
    SymlinkedDir { path: PathBuf },
    #[error("Failed to inspect cache path: {}", path.display())]
    InspectPath { path: PathBuf, source: io::Error },
}
