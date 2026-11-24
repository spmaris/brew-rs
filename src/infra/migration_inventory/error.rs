//! Typed failures of the Homebrew inventory entry point.

use std::io;
use std::path::PathBuf;

/// Why the supplied Homebrew prefix cannot be inventoried.
#[derive(Debug, thiserror::Error)]
pub(crate) enum InventoryError {
    #[error("Homebrew inventory requires an absolute prefix; got {}", .path.display())]
    NotAbsolute { path: PathBuf },
    #[error("Homebrew inventory requires a prefix below the filesystem root")]
    Root,
    #[error(
        "Homebrew inventory requires a normalized prefix without `..` components: {}",
        .path.display()
    )]
    NotNormalized { path: PathBuf },
    #[error("Could not read the Homebrew prefix {}", .path.display())]
    Unreadable {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("Homebrew prefix is not a directory: {}", .path.display())]
    NotDirectory { path: PathBuf },
}
