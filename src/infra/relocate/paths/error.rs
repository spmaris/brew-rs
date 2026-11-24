//! Typed failures of the placeholder-to-keg path policy.

use std::path::PathBuf;

/// Why a Homebrew load path or placeholder could not be mapped.
#[derive(Debug, thiserror::Error)]
pub(crate) enum PathsError {
    #[error("Bottle relocation requires a UTF-8 prefix")]
    PrefixNotUtf8,
    #[error("Bottle relocation requires a normalized absolute prefix with safe path characters")]
    PrefixNotNormalized,
    #[error("Unsafe @rpath library path {path:?}")]
    UnsafeRpathLibrary { path: String },
    #[error("Unsupported Cellar or opt LC_RPATH")]
    UnsupportedCellarRpath,
    #[error("Unsupported @loader_path LC_RPATH {path:?}")]
    UnsupportedLoaderRpath { path: String },
    #[error("Unsupported LC_RPATH {path:?}")]
    UnsupportedRpath { path: String },
    #[error("Unsafe system library path in bottle")]
    UnsafeSystemLibrary,
    #[error("Mach-O @loader_path {path:?} must resolve to a member inside the same keg")]
    LoaderPathOutsideKeg { path: String },
    #[error("Unsupported Mach-O path {path:?}: expected a pinned keg or macOS system library")]
    UnsupportedMachoPath { path: String },
    #[error("Invalid keg identity in relocation plan")]
    InvalidKegIdentity,
    #[error("Unsupported relative path for Mach-O image {}", .path.display())]
    UnsupportedImagePath { path: PathBuf },
    #[error("Mach-O image path is not valid UTF-8")]
    ImagePathNotUtf8,
    #[error("Unsafe Mach-O image path component {part:?}")]
    UnsafeImageComponent { part: String },
    #[error("Expected @loader_path prefix")]
    MissingLoaderPrefix,
    #[error("Empty path segment in @loader_path reference {raw:?}")]
    EmptyLoaderSegment { raw: String },
    #[error("Unsafe '..' after normal component in {raw:?}")]
    LoaderParentAfterNormal { raw: String },
    #[error("Mach-O @loader_path {raw:?} escapes the prefix")]
    LoaderEscapesPrefix { raw: String },
    #[error("Unsafe component {component:?} in @loader_path {raw:?}")]
    UnsafeLoaderComponent { component: String, raw: String },
    #[error("Mach-O @loader_path {raw:?} resolves to the prefix root")]
    LoaderResolvesToRoot { raw: String },
    #[error("Missing version in Cellar path")]
    MissingCellarVersion,
    #[error("Bottle references undeclared keg {name:?}")]
    UndeclaredKeg { name: String },
    #[error("Bottle requires {name} {original:?}, but the transaction pins {pinned:?}")]
    VersionMismatch {
        name: String,
        original: String,
        pinned: String,
    },
    #[error("Unsafe keg member path in bottle")]
    UnsafeKegMember,
}
