//! Typed failures of staged-keg relocation.
//!
//! Mach-O, text and path-policy failures live in their own enums and are
//! wrapped transparently, so the displayed text is the leaf message.

use std::io;
use std::path::{Path, PathBuf, StripPrefixError};

use super::macho::MachoError;
use super::paths::PathsError;
use super::text::TextError;

/// Why a staged keg could not be relocated.
#[derive(Debug, thiserror::Error)]
pub(crate) enum RelocateError {
    #[error(transparent)]
    Macho(#[from] MachoError),
    #[error(transparent)]
    Text(#[from] TextError),
    #[error(transparent)]
    Paths(#[from] PathsError),

    #[error("failed to {action} '{}'", path.display())]
    Io {
        action: &'static str,
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error(transparent)]
    StripPrefix(#[from] StripPrefixError),

    #[error("Bottle relocation requires macOS")]
    MacosRequired,
    #[error("Bottle relocation requires {tool}; install the Apple Command Line Tools and retry")]
    ToolMissing { tool: &'static str },
    #[error("Could not resolve the relocation prefix")]
    PrefixResolve {
        #[source]
        source: io::Error,
    },
    #[error("Duplicate staged keg for {name} {version}")]
    DuplicateStage { name: String, version: String },
    #[error("Could not relocate {name} {version}")]
    Keg {
        name: String,
        version: String,
        #[source]
        source: Box<RelocateError>,
    },
    #[error("In staged file {}", .path.display())]
    StagedFile {
        path: PathBuf,
        #[source]
        source: Box<RelocateError>,
    },
    #[error("Unexpected entry in staged keg: {}", .path.display())]
    UnexpectedEntry { path: PathBuf },
    #[error("File exceeds the relocation size limit ({limit} bytes)")]
    FileTooLarge { limit: u64 },
    #[error(
        "Mach-O @rpath dependency {old:?} in {} does not resolve inside a pinned keg LC_RPATH",
        .image.display()
    )]
    RpathUnresolved { old: String, image: PathBuf },
    #[error("A relocated Mach-O reference must name a pinned keg target")]
    NotPinnedKeg,
    #[error("A relocated Mach-O reference must name a file within a pinned keg")]
    NotFileInKeg,
    #[error("Relocated library target is not inside a real managed keg: {}", .target.display())]
    TargetNotManagedKeg { target: PathBuf },
    #[error("Missing relocated library target {}", .target.display())]
    TargetMissing {
        target: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error(
        "Relocated library target is not a {kind} inside its pinned keg: {}",
        .target.display()
    )]
    TargetWrongKind { kind: &'static str, target: PathBuf },
}

impl RelocateError {
    /// A `map_err` adapter that names the action and path of a failed I/O call.
    pub(super) fn io(action: &'static str, path: &Path) -> impl FnOnce(io::Error) -> Self {
        let path = path.to_path_buf();
        move |source| Self::Io {
            action,
            path,
            source,
        }
    }
}
