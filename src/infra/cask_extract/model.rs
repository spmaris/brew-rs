//! Detected payload formats and the staged output types.

use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PayloadFormat {
    Naked,
    Zip,
    TarGz,
    TarBz2,
    TarXz,
    Dmg,
}

/// Output of staging a single verified cask download into a private stage dir.
#[derive(Debug)]
pub(crate) struct StagedCaskPayload {
    pub app_bundles: Vec<StagedAppBundle>,
    pub has_caskroom_files: bool,
    pub caskroom_files: Vec<String>,
    pub symlinks: Vec<StagedCaskLink>,
    pub wrappers: Vec<StagedWrapper>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StagedAppBundle {
    pub bundle_name: String,
    pub staged_dir: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StagedCaskLink {
    pub relative_dest: String,
    pub target: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StagedWrapper {
    pub relative_dest: String,
    pub staged_file: PathBuf,
    pub sha256: String,
}
