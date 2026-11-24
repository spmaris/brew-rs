//! Prefix path helpers and directory fsync.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use super::{BIN, CELLAR, MARKER_FILE, META, OPT, RECEIPTS, RETAINED, SBIN, STAGES, STATE};

pub(crate) fn sync_directory(directory: &Path) {
    #[cfg(unix)]
    if let Ok(handle) = File::open(directory)
        && let Err(error) = handle.sync_all()
    {
        log::debug!("failed to flush '{}': {error}", directory.display());
    }
    #[cfg(not(unix))]
    let _ = directory;
}

pub(in crate::infra) fn monotonic_nonce() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}

pub(super) fn marker_path(prefix: &Path) -> PathBuf {
    prefix.join(MARKER_FILE)
}

pub(in crate::infra) fn meta_dir(prefix: &Path) -> PathBuf {
    prefix.join(META)
}

pub(crate) fn state_dir(prefix: &Path) -> PathBuf {
    meta_dir(prefix).join(STATE)
}

pub(super) fn receipts_dir(prefix: &Path) -> PathBuf {
    state_dir(prefix).join(RECEIPTS)
}

/// Records of kegs retained because a surviving receipt still pins their exact
/// version and digest: `<retained>/<name>/<version>.json`.
pub(super) fn retained_dir(prefix: &Path) -> PathBuf {
    state_dir(prefix).join(RETAINED)
}

pub(crate) fn stages_dir(prefix: &Path) -> PathBuf {
    meta_dir(prefix).join(STAGES)
}

pub(super) fn cellar_dir(prefix: &Path) -> PathBuf {
    prefix.join(CELLAR)
}

pub(in crate::infra) fn bin_dir(prefix: &Path) -> PathBuf {
    prefix.join(BIN)
}

pub(super) fn sbin_dir(prefix: &Path) -> PathBuf {
    prefix.join(SBIN)
}

pub(super) fn opt_dir(prefix: &Path) -> PathBuf {
    prefix.join(OPT)
}
