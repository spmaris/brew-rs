//! Read-only inventory of an existing Homebrew installation for `brewrs migrate`.
//!
//! The caller supplies one explicit absolute Homebrew prefix and the launchd
//! directories it wants inspected. Nothing here runs `brew`, `launchctl` or any
//! other process, writes to the filesystem, or uses the network. Only these
//! fixed locations are read:
//!
//! * `Cellar/<formula>/<version>/INSTALL_RECEIPT.json`
//! * `Caskroom/<token>/<version>`
//! * `Library/Taps/<owner>/homebrew-<repo>`
//! * direct `homebrew.mxcl.*.plist` filenames of supplied service directories
//!
//! Every scanned entry must be a real, non-symlink filesystem object at a
//! fixed depth; symlinks are never followed. Malformed, foreign or incomplete
//! data is skipped and reported through [`Inventory::warnings`] instead of
//! being guessed at or silently treated as absent. Warning and record counts
//! are bounded so a hostile or damaged prefix cannot exhaust memory.
//!
//! Limits of the inventory, by design:
//!
//! * A cask record is only a `Caskroom` directory and the version directories
//!   below it. Installed applications, artifacts and cask receipts are not
//!   inspected, so a cask record does not mean the app is installed.
//! * A service record is only a plist filename. The plist is not parsed and no
//!   launchd state is queried, so it does not mean the service is loaded or
//!   running.
//! * Multiple Cellar versions are kept. When their receipts disagree about a
//!   nonempty tap or about `installed_on_request`, that field stays unknown and
//!   the disagreement is reported. A readiness check must never see a
//!   mixed-source package as definitely official. Receipts without
//!   `installed_on_request` stay unknown rather than being inferred from older
//!   dependency fields.
//! * Tap directories are reduced to Homebrew tap names (`owner/repo`); the
//!   `homebrew-` directory prefix is dropped.

mod cellar;
mod fs_scan;
mod limits;
mod records;
mod warnings;

use std::fs;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};

use self::cellar::cellar;
use self::records::{caskroom, services, taps};
use self::warnings::Warnings;

/// Observed Homebrew state for migration planning.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct Inventory {
    pub formulae: Vec<Formula>,
    pub casks: Vec<Cask>,
    pub taps: Vec<String>,
    pub services: Vec<Service>,
    pub warnings: Vec<String>,
}

/// One formula directory in `Cellar`, with every observed version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Formula {
    pub name: String,
    pub versions: Vec<String>,
    pub tap: Option<String>,
    pub requested: Option<bool>,
}

/// One cask directory in `Caskroom`. This does not imply an installed app.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Cask {
    pub token: String,
    pub versions: Vec<String>,
}

/// One plist filename in a caller-supplied service directory. This does not
/// imply a loaded or running service.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Service {
    pub label: String,
    pub path: PathBuf,
}

/// Collect the migration inventory for `prefix` plus the given service
/// directories.
///
/// `prefix` must be an absolute, normalized directory. It may itself be a
/// symlink because the caller named it explicitly; scanned descendants must be
/// real directories and files. A missing optional directory (`Cellar`,
/// `Caskroom`, `Library/Taps`) is normal and produces no warning.
pub(crate) fn collect(prefix: &Path, service_dirs: &[PathBuf]) -> Result<Inventory> {
    validate_prefix(prefix)?;
    let mut warnings = Warnings::new();
    let formulae = cellar(prefix, &mut warnings);
    let casks = caskroom(prefix, &mut warnings);
    let taps = taps(prefix, &mut warnings);
    let services = services(service_dirs, &mut warnings);
    Ok(Inventory {
        formulae,
        casks,
        taps,
        services,
        warnings: warnings.finish(),
    })
}

fn validate_prefix(prefix: &Path) -> Result<()> {
    if !prefix.is_absolute() {
        bail!(
            "Homebrew inventory requires an absolute prefix; got {}",
            prefix.display()
        );
    }
    if prefix == Path::new("/") {
        bail!("Homebrew inventory requires a prefix below the filesystem root");
    }
    if prefix
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        bail!(
            "Homebrew inventory requires a normalized prefix without `..` components: {}",
            prefix.display()
        );
    }
    let metadata = fs::metadata(prefix)
        .with_context(|| format!("Could not read the Homebrew prefix {}", prefix.display()))?;
    if !metadata.is_dir() {
        bail!("Homebrew prefix is not a directory: {}", prefix.display());
    }
    Ok(())
}

#[cfg(test)]
mod tests;
