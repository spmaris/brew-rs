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

use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io::Read;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

/// Longest formula, cask, version, tap component or service label accepted.
const MAX_NAME_LEN: usize = 128;
/// Most entries examined in one directory.
const MAX_DIRECTORY_ENTRIES: usize = 4_096;
/// Most formulae inventoried.
const MAX_FORMULAE: usize = 4_096;
/// Most installed versions kept for one formula or cask.
const MAX_VERSIONS_PER_PACKAGE: usize = 64;
/// Most casks inventoried.
const MAX_CASKS: usize = 4_096;
/// Most taps inventoried.
const MAX_TAPS: usize = 512;
/// Most services inventoried.
const MAX_SERVICES: usize = 1_024;
/// Largest install receipt read; larger files are skipped as incomplete.
const MAX_RECEIPT_BYTES: u64 = 1024 * 1024;
/// Most warnings retained before one summary warning is added.
const MAX_WARNINGS: usize = 200;

/// Observed Homebrew state for migration planning.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(super) struct Inventory {
    pub formulae: Vec<Formula>,
    pub casks: Vec<Cask>,
    pub taps: Vec<String>,
    pub services: Vec<Service>,
    pub warnings: Vec<String>,
}

/// One formula directory in `Cellar`, with every observed version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Formula {
    pub name: String,
    pub versions: Vec<String>,
    pub tap: Option<String>,
    pub requested: Option<bool>,
}

/// One cask directory in `Caskroom`. This does not imply an installed app.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Cask {
    pub token: String,
    pub versions: Vec<String>,
}

/// One plist filename in a caller-supplied service directory. This does not
/// imply a loaded or running service.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Service {
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
pub(super) fn collect(prefix: &Path, service_dirs: &[PathBuf]) -> Result<Inventory> {
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

fn cellar(prefix: &Path, warnings: &mut Warnings) -> Vec<Formula> {
    let root = prefix.join("Cellar");
    if !real_directory(&root, "Cellar", false, warnings) {
        return Vec::new();
    }
    let Some(entries) = directory_entries(&root, "Cellar", warnings) else {
        return Vec::new();
    };
    let mut formulae = Vec::new();
    for entry in entries {
        if formulae.len() == MAX_FORMULAE {
            warnings.push(format!(
                "Cellar: inventory truncated after {MAX_FORMULAE} formulae"
            ));
            break;
        }
        let Some(name) = entry_name(&entry, "Cellar", warnings) else {
            continue;
        };
        if is_hidden(&name) {
            continue;
        }
        if !plausible_component(&name) {
            warnings.push(format!(
                "Cellar: ignoring implausible formula directory {name:?}"
            ));
            continue;
        }
        match entry.file_type() {
            Ok(kind) if kind.is_dir() => {}
            Ok(kind) if kind.is_symlink() => {
                warnings.push(format!(
                    "Cellar: ignoring symlinked formula directory {}",
                    entry.path().display()
                ));
                continue;
            }
            Ok(_) => {
                warnings.push(format!(
                    "Cellar: ignoring non-directory formula entry {}",
                    entry.path().display()
                ));
                continue;
            }
            Err(error) => {
                warnings.push(format!(
                    "Cellar: could not inspect {}: {error}",
                    entry.path().display()
                ));
                continue;
            }
        }
        let directory = entry.path();
        let mut versions = package_versions(&directory, "Cellar", &name, warnings);
        if versions.is_empty() {
            warnings.push(format!(
                "Cellar: formula {name:?} has no version directories"
            ));
            continue;
        }
        versions.sort_unstable();

        // A formula can retain several installed versions. Receipt facts that
        // do not agree across them stay unknown: readiness must not treat a
        // mixed-source package as definitely official. Unknown receipt values
        // are not contrary evidence and therefore never create a conflict.
        let mut taps = BTreeSet::new();
        let mut requested_true = false;
        let mut requested_false = false;
        for version in &versions {
            let receipt = read_receipt(
                &directory.join(version).join("INSTALL_RECEIPT.json"),
                &name,
                version,
                warnings,
            );
            if let Some(value) = receipt.tap {
                taps.insert(value);
            }
            match receipt.requested {
                Some(true) => requested_true = true,
                Some(false) => requested_false = true,
                None => {}
            }
        }
        let tap = if taps.len() > 1 {
            warnings.push(format!(
                "Cellar: versions of {name:?} name different source taps {taps:?}; leaving it unknown"
            ));
            None
        } else {
            taps.into_iter().next()
        };
        let requested = match (requested_true, requested_false) {
            (true, false) => Some(true),
            (false, true) => Some(false),
            (true, true) => {
                warnings.push(format!(
                    "Cellar: versions of {name:?} disagree about installed_on_request; leaving it unknown"
                ));
                None
            }
            (false, false) => None,
        };
        formulae.push(Formula {
            name,
            versions,
            tap,
            requested,
        });
    }
    formulae.sort_unstable_by(|left, right| left.name.cmp(&right.name));
    formulae
}

fn caskroom(prefix: &Path, warnings: &mut Warnings) -> Vec<Cask> {
    let root = prefix.join("Caskroom");
    if !real_directory(&root, "Caskroom", false, warnings) {
        return Vec::new();
    }
    let Some(entries) = directory_entries(&root, "Caskroom", warnings) else {
        return Vec::new();
    };
    let mut casks = Vec::new();
    for entry in entries {
        if casks.len() == MAX_CASKS {
            warnings.push(format!(
                "Caskroom: inventory truncated after {MAX_CASKS} casks"
            ));
            break;
        }
        let Some(token) = entry_name(&entry, "Caskroom", warnings) else {
            continue;
        };
        if is_hidden(&token) {
            continue;
        }
        if !plausible_component(&token) {
            warnings.push(format!(
                "Caskroom: ignoring implausible cask directory {token:?}"
            ));
            continue;
        }
        match entry.file_type() {
            Ok(kind) if kind.is_dir() => {}
            Ok(kind) if kind.is_symlink() => {
                warnings.push(format!(
                    "Caskroom: ignoring symlinked cask directory {}",
                    entry.path().display()
                ));
                continue;
            }
            Ok(_) => {
                warnings.push(format!(
                    "Caskroom: ignoring non-directory cask entry {}",
                    entry.path().display()
                ));
                continue;
            }
            Err(error) => {
                warnings.push(format!(
                    "Caskroom: could not inspect {}: {error}",
                    entry.path().display()
                ));
                continue;
            }
        }
        let versions = package_versions(&entry.path(), "Caskroom", &token, warnings);
        if versions.is_empty() {
            warnings.push(format!(
                "Caskroom: cask {token:?} has no version directories"
            ));
            continue;
        }
        casks.push(Cask { token, versions });
    }
    casks.sort_unstable_by(|left, right| left.token.cmp(&right.token));
    casks
}

fn taps(prefix: &Path, warnings: &mut Warnings) -> Vec<String> {
    let library = prefix.join("Library");
    if !real_directory(&library, "Taps", false, warnings) {
        return Vec::new();
    }
    let root = library.join("Taps");
    if !real_directory(&root, "Taps", false, warnings) {
        return Vec::new();
    }
    let Some(owners) = directory_entries(&root, "Taps", warnings) else {
        return Vec::new();
    };
    let mut taps = BTreeSet::new();
    'owners: for owner_entry in owners {
        let Some(owner) = entry_name(&owner_entry, "Taps", warnings) else {
            continue;
        };
        if is_hidden(&owner) {
            continue;
        }
        if !plausible_component(&owner) {
            warnings.push(format!("Taps: ignoring implausible tap owner {owner:?}"));
            continue;
        }
        match owner_entry.file_type() {
            Ok(kind) if kind.is_dir() => {}
            Ok(kind) if kind.is_symlink() => {
                warnings.push(format!(
                    "Taps: ignoring symlinked tap owner directory {}",
                    owner_entry.path().display()
                ));
                continue;
            }
            Ok(_) => {
                warnings.push(format!(
                    "Taps: ignoring non-directory tap owner entry {}",
                    owner_entry.path().display()
                ));
                continue;
            }
            Err(error) => {
                warnings.push(format!(
                    "Taps: could not inspect {}: {error}",
                    owner_entry.path().display()
                ));
                continue;
            }
        }
        let Some(repos) = directory_entries(&owner_entry.path(), "Taps", warnings) else {
            continue;
        };
        for repo_entry in repos {
            if taps.len() == MAX_TAPS {
                warnings.push(format!("Taps: inventory truncated after {MAX_TAPS} taps"));
                break 'owners;
            }
            let Some(repo) = entry_name(&repo_entry, "Taps", warnings) else {
                continue;
            };
            if is_hidden(&repo) {
                continue;
            }
            let Some(repo) = repo.strip_prefix("homebrew-") else {
                continue;
            };
            if repo.is_empty() || !plausible_component(repo) {
                warnings.push(format!(
                    "Taps: ignoring implausible tap repository directory {:?}",
                    repo_entry.file_name()
                ));
                continue;
            }
            match repo_entry.file_type() {
                Ok(kind) if kind.is_dir() => {}
                Ok(kind) if kind.is_symlink() => {
                    warnings.push(format!(
                        "Taps: ignoring symlinked tap repository directory {}",
                        repo_entry.path().display()
                    ));
                    continue;
                }
                Ok(_) => {
                    warnings.push(format!(
                        "Taps: ignoring non-directory tap repository entry {}",
                        repo_entry.path().display()
                    ));
                    continue;
                }
                Err(error) => {
                    warnings.push(format!(
                        "Taps: could not inspect {}: {error}",
                        repo_entry.path().display()
                    ));
                    continue;
                }
            }
            taps.insert(format!("{owner}/{repo}"));
        }
    }
    taps.into_iter().collect()
}

fn services(directories: &[PathBuf], warnings: &mut Warnings) -> Vec<Service> {
    let mut services = Vec::new();
    let mut seen = BTreeSet::new();
    'directories: for directory in directories {
        if !seen.insert(directory.clone()) {
            continue;
        }
        if !real_directory(directory, "Services", true, warnings) {
            continue;
        }
        let Some(entries) = directory_entries(directory, "Services", warnings) else {
            continue;
        };
        for entry in entries {
            if services.len() == MAX_SERVICES {
                warnings.push(format!(
                    "Services: inventory truncated after {MAX_SERVICES} plists"
                ));
                break 'directories;
            }
            let Some(name) = entry_name(&entry, "Services", warnings) else {
                continue;
            };
            if is_hidden(&name) {
                continue;
            }
            let path = Path::new(&name);
            let is_plist = path
                .extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("plist"));
            if !is_plist {
                continue;
            }
            let Some(label) = path.file_stem().and_then(|stem| stem.to_str()) else {
                warnings.push(format!(
                    "Services: ignoring plist without a UTF-8 label: {}",
                    entry.path().display()
                ));
                continue;
            };
            if !label.starts_with("homebrew.mxcl.") {
                continue;
            }
            if !plausible_component(label) {
                warnings.push(format!(
                    "Services: ignoring plist with an implausible label: {}",
                    entry.path().display()
                ));
                continue;
            }
            match entry.file_type() {
                Ok(kind) if kind.is_file() => {}
                Ok(kind) if kind.is_symlink() => {
                    warnings.push(format!(
                        "Services: ignoring symlinked service plist {}",
                        entry.path().display()
                    ));
                    continue;
                }
                Ok(_) => {
                    warnings.push(format!(
                        "Services: ignoring non-file plist entry {}",
                        entry.path().display()
                    ));
                    continue;
                }
                Err(error) => {
                    warnings.push(format!(
                        "Services: could not inspect {}: {error}",
                        entry.path().display()
                    ));
                    continue;
                }
            }
            services.push(Service {
                label: label.to_owned(),
                path: directory.join(&name),
            });
        }
    }
    services.sort_unstable_by(|left, right| {
        left.label
            .cmp(&right.label)
            .then_with(|| left.path.cmp(&right.path))
    });
    services
}

fn package_versions(
    directory: &Path,
    area: &str,
    label: &str,
    warnings: &mut Warnings,
) -> Vec<String> {
    let Some(entries) = directory_entries(directory, area, warnings) else {
        return Vec::new();
    };
    let mut versions = Vec::new();
    for entry in entries {
        if versions.len() == MAX_VERSIONS_PER_PACKAGE {
            warnings.push(format!(
                "{area}: keeping only {MAX_VERSIONS_PER_PACKAGE} versions of {label:?}"
            ));
            break;
        }
        let Some(version) = entry_name(&entry, area, warnings) else {
            continue;
        };
        if is_hidden(&version) {
            continue;
        }
        if !plausible_component(&version) {
            warnings.push(format!(
                "{area}: ignoring implausible version directory {version:?} in {label:?}"
            ));
            continue;
        }
        match entry.file_type() {
            Ok(kind) if kind.is_dir() => versions.push(version),
            Ok(kind) if kind.is_symlink() => {
                warnings.push(format!(
                    "{area}: ignoring symlinked version directory {}",
                    entry.path().display()
                ));
            }
            Ok(_) => {
                warnings.push(format!(
                    "{area}: ignoring non-directory version entry {}",
                    entry.path().display()
                ));
            }
            Err(error) => {
                warnings.push(format!(
                    "{area}: could not inspect {}: {error}",
                    entry.path().display()
                ));
            }
        }
    }
    versions.sort_unstable();
    versions
}

fn directory_entries(
    directory: &Path,
    area: &str,
    warnings: &mut Warnings,
) -> Option<Vec<fs::DirEntry>> {
    let reader = match fs::read_dir(directory) {
        Ok(reader) => reader,
        Err(error) => {
            warnings.push(format!(
                "{area}: could not read {}: {error}",
                directory.display()
            ));
            return None;
        }
    };
    let mut entries = Vec::new();
    for entry in reader {
        if entries.len() == MAX_DIRECTORY_ENTRIES {
            warnings.push(format!(
                "{area}: examined only the first {MAX_DIRECTORY_ENTRIES} entries in {}",
                directory.display()
            ));
            break;
        }
        match entry {
            Ok(entry) => entries.push(entry),
            Err(error) => warnings.push(format!(
                "{area}: could not read an entry in {}: {error}",
                directory.display()
            )),
        }
    }
    entries.sort_unstable_by_key(|entry| entry.file_name());
    Some(entries)
}

/// Accept only a real, non-symlink directory at `path`.
///
/// A missing optional directory is normal and is not reported. Explicit inputs
/// such as service directories pass `missing_is_warning` so a stale path is
/// visible to the caller instead of looking like an empty directory.
fn real_directory(
    path: &Path,
    area: &str,
    missing_is_warning: bool,
    warnings: &mut Warnings,
) -> bool {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            warnings.push(format!(
                "{area}: ignoring symlinked directory {}",
                path.display()
            ));
            false
        }
        Ok(metadata) if metadata.is_dir() => true,
        Ok(_) => {
            warnings.push(format!(
                "{area}: ignoring non-directory path {}",
                path.display()
            ));
            false
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if missing_is_warning {
                warnings.push(format!(
                    "{area}: directory does not exist: {}",
                    path.display()
                ));
            }
            false
        }
        Err(error) => {
            warnings.push(format!(
                "{area}: could not inspect {}: {error}",
                path.display()
            ));
            false
        }
    }
}

fn entry_name(entry: &fs::DirEntry, area: &str, warnings: &mut Warnings) -> Option<String> {
    match entry.file_name().into_string() {
        Ok(name) => Some(name),
        Err(_) => {
            warnings.push(format!(
                "{area}: ignoring a non-UTF-8 entry name in {}",
                entry.path().parent().unwrap_or(Path::new("")).display()
            ));
            None
        }
    }
}

fn is_hidden(name: &str) -> bool {
    name.starts_with('.')
}

/// Conservative names for formulae, casks, versions, tap parts and labels.
///
/// These characters cover every real Homebrew name this inventory reports
/// (`python@3.12`, `libpng`, `1.2.3_1`, `homebrew.mxcl.ripgrep`) while keeping
/// separators, control bytes, spaces and traversal components out.
fn plausible_component(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_NAME_LEN
        && !value.starts_with('.')
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_' | b'+' | b'@')
        })
}

fn valid_tap_name(value: &str) -> bool {
    if value.len() > MAX_NAME_LEN * 2 + 1 {
        return false;
    }
    let mut parts = value.split('/');
    let Some(owner) = parts.next() else {
        return false;
    };
    let Some(repo) = parts.next() else {
        return false;
    };
    parts.next().is_none() && plausible_component(owner) && plausible_component(repo)
}

#[derive(Debug, Default, Deserialize)]
struct InstallReceipt {
    #[serde(default)]
    installed_on_request: Option<bool>,
    #[serde(default)]
    source: Option<ReceiptSource>,
}

#[derive(Debug, Default, Deserialize)]
struct ReceiptSource {
    #[serde(default)]
    tap: Option<String>,
}

#[derive(Debug, Default)]
struct ReceiptFacts {
    tap: Option<String>,
    requested: Option<bool>,
}

fn read_receipt(
    path: &Path,
    formula: &str,
    version: &str,
    warnings: &mut Warnings,
) -> ReceiptFacts {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            warnings.push(format!(
                "Cellar: missing INSTALL_RECEIPT.json for {formula} {version}"
            ));
            return ReceiptFacts::default();
        }
        Err(error) => {
            warnings.push(format!(
                "Cellar: could not inspect the install receipt for {formula} {version}: {error}"
            ));
            return ReceiptFacts::default();
        }
    };
    if metadata.file_type().is_symlink() {
        warnings.push(format!(
            "Cellar: ignoring symlinked install receipt for {formula} {version}"
        ));
        return ReceiptFacts::default();
    }
    if !metadata.is_file() {
        warnings.push(format!(
            "Cellar: install receipt for {formula} {version} is not a regular file"
        ));
        return ReceiptFacts::default();
    }
    if metadata.len() > MAX_RECEIPT_BYTES {
        warnings.push(format!(
            "Cellar: install receipt for {formula} {version} exceeds the {MAX_RECEIPT_BYTES}-byte limit"
        ));
        return ReceiptFacts::default();
    }
    let mut bytes = Vec::new();
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) => {
            warnings.push(format!(
                "Cellar: could not open the install receipt for {formula} {version}: {error}"
            ));
            return ReceiptFacts::default();
        }
    };
    if let Err(error) = file.take(MAX_RECEIPT_BYTES + 1).read_to_end(&mut bytes) {
        warnings.push(format!(
            "Cellar: could not read the install receipt for {formula} {version}: {error}"
        ));
        return ReceiptFacts::default();
    }
    if bytes.len() as u64 > MAX_RECEIPT_BYTES {
        warnings.push(format!(
            "Cellar: install receipt for {formula} {version} exceeds the {MAX_RECEIPT_BYTES}-byte limit"
        ));
        return ReceiptFacts::default();
    }
    let receipt: InstallReceipt = match serde_json::from_slice(&bytes) {
        Ok(receipt) => receipt,
        Err(error) => {
            warnings.push(format!(
                "Cellar: ignoring malformed INSTALL_RECEIPT.json for {formula} {version}: {error}"
            ));
            return ReceiptFacts::default();
        }
    };
    let tap = receipt
        .source
        .and_then(|source| source.tap)
        .and_then(|tap| {
            if valid_tap_name(&tap) {
                Some(tap)
            } else {
                warnings.push(format!(
                    "Cellar: ignoring implausible source tap {tap:?} for {formula} {version}"
                ));
                None
            }
        });
    ReceiptFacts {
        tap,
        requested: receipt.installed_on_request,
    }
}

/// Bounded warning accumulator: at most [`MAX_WARNINGS`] messages plus one
/// summary line naming the number omitted.
struct Warnings {
    messages: Vec<String>,
    omitted: usize,
}

impl Warnings {
    fn new() -> Self {
        Self {
            messages: Vec::new(),
            omitted: 0,
        }
    }

    fn push(&mut self, message: impl Into<String>) {
        if self.messages.len() == MAX_WARNINGS {
            self.omitted += 1;
        } else {
            self.messages.push(message.into());
        }
    }

    fn finish(mut self) -> Vec<String> {
        if self.omitted > 0 {
            self.messages
                .push(format!("Inventory: {} more warnings omitted", self.omitted));
        }
        self.messages
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write_receipt(path: &Path, tap: Option<&str>, requested: Option<bool>) {
        write_receipt_value(path, tap.map(serde_json::Value::from), requested);
    }

    fn write_receipt_value(path: &Path, tap: Option<serde_json::Value>, requested: Option<bool>) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut source = serde_json::Map::new();
        if let Some(tap) = tap {
            source.insert("tap".into(), tap);
        }
        let mut receipt = serde_json::Map::new();
        if let Some(requested) = requested {
            receipt.insert("installed_on_request".into(), requested.into());
        }
        receipt.insert("source".into(), source.into());
        fs::write(
            path,
            serde_json::to_vec(&serde_json::Value::Object(receipt)).unwrap(),
        )
        .unwrap();
    }

    fn formula<'a>(inventory: &'a Inventory, name: &str) -> &'a Formula {
        inventory
            .formulae
            .iter()
            .find(|formula| formula.name == name)
            .unwrap_or_else(|| panic!("missing formula {name}"))
    }

    #[test]
    fn inventories_cellar_casks_taps_and_services() {
        let sandbox = tempfile::tempdir().unwrap();
        // A prefix with a space proves path joining does not tokenize.
        let prefix = sandbox.path().join("Homebrew Prefix");
        fs::create_dir_all(&prefix).unwrap();

        write_receipt(
            &prefix.join("Cellar/ripgrep/16.0.0/INSTALL_RECEIPT.json"),
            Some("homebrew/core"),
            Some(true),
        );
        write_receipt(
            &prefix.join("Cellar/ripgrep/15.1.0/INSTALL_RECEIPT.json"),
            Some("homebrew/core"),
            Some(true),
        );
        // Conflicting receipts leave the whole formula's field unknown: 9.0 is
        // in core and was requested, 10.0 came from a user tap as a dependency.
        write_receipt(
            &prefix.join("Cellar/mixed/9.0/INSTALL_RECEIPT.json"),
            Some("homebrew/core"),
            Some(true),
        );
        write_receipt(
            &prefix.join("Cellar/mixed/10.0/INSTALL_RECEIPT.json"),
            Some("acme/tools"),
            Some(false),
        );
        // Older receipts may omit both fields.
        write_receipt(
            &prefix.join("Cellar/pcre2@10/10.49/INSTALL_RECEIPT.json"),
            None,
            None,
        );
        // A missing receipt and a malformed receipt keep the version with
        // unknown receipt facts and one warning each.
        fs::create_dir_all(prefix.join("Cellar/bzip2/1.0.8")).unwrap();
        fs::create_dir_all(prefix.join("Cellar/broken/1.0")).unwrap();
        fs::write(
            prefix.join("Cellar/broken/1.0/INSTALL_RECEIPT.json"),
            b"{not json",
        )
        .unwrap();
        // A wrong field type is treated as a malformed receipt, not guessed.
        write_receipt_value(
            &prefix.join("Cellar/typed/1.0/INSTALL_RECEIPT.json"),
            Some("homebrew/core".into()),
            None,
        );
        let mut receipt = serde_json::Map::new();
        receipt.insert("installed_on_request".into(), "yes".into());
        fs::write(
            prefix.join("Cellar/typed/1.0/INSTALL_RECEIPT.json"),
            serde_json::to_vec(&serde_json::Value::Object(receipt)).unwrap(),
        )
        .unwrap();

        fs::create_dir_all(prefix.join("Caskroom/firefox/129.0")).unwrap();
        fs::create_dir_all(prefix.join("Caskroom/firefox/128.0")).unwrap();
        fs::create_dir_all(prefix.join("Caskroom/temurin@17/17.0.2")).unwrap();

        fs::create_dir_all(prefix.join("Library/Taps/homebrew/homebrew-core")).unwrap();
        fs::create_dir_all(prefix.join("Library/Taps/acme/homebrew-tools")).unwrap();
        fs::create_dir_all(prefix.join("Library/Taps/acme/foreign-repo")).unwrap();
        fs::write(prefix.join("Library/Taps/README.md"), b"foreign").unwrap();

        let agents = sandbox.path().join("LaunchAgents");
        fs::create_dir_all(&agents).unwrap();
        fs::write(agents.join("homebrew.mxcl.ripgrep.plist"), b"<plist/>").unwrap();
        fs::write(agents.join("com.example.other.PLIST"), b"<plist/>").unwrap();
        fs::write(agents.join("notes.txt"), b"not a service").unwrap();
        let other_agents = sandbox.path().join("Other Launch Agents");
        fs::create_dir_all(&other_agents).unwrap();
        fs::write(other_agents.join("homebrew.mxcl.pcre2.plist"), b"<plist/>").unwrap();

        let inventory = collect(&prefix, &[agents.clone(), other_agents.clone()]).unwrap();

        assert_eq!(
            inventory.formulae,
            vec![
                Formula {
                    name: "broken".into(),
                    versions: vec!["1.0".into()],
                    tap: None,
                    requested: None,
                },
                Formula {
                    name: "bzip2".into(),
                    versions: vec!["1.0.8".into()],
                    tap: None,
                    requested: None,
                },
                Formula {
                    name: "mixed".into(),
                    versions: vec!["10.0".into(), "9.0".into()],
                    tap: None,
                    requested: None,
                },
                Formula {
                    name: "pcre2@10".into(),
                    versions: vec!["10.49".into()],
                    tap: None,
                    requested: None,
                },
                Formula {
                    name: "ripgrep".into(),
                    versions: vec!["15.1.0".into(), "16.0.0".into()],
                    tap: Some("homebrew/core".into()),
                    requested: Some(true),
                },
                Formula {
                    name: "typed".into(),
                    versions: vec!["1.0".into()],
                    tap: None,
                    requested: None,
                },
            ]
        );
        assert_eq!(
            inventory.casks,
            vec![
                Cask {
                    token: "firefox".into(),
                    versions: vec!["128.0".into(), "129.0".into()],
                },
                Cask {
                    token: "temurin@17".into(),
                    versions: vec!["17.0.2".into()],
                },
            ]
        );
        assert_eq!(inventory.taps, vec!["acme/tools", "homebrew/core"]);
        assert_eq!(
            inventory.services,
            vec![
                Service {
                    label: "homebrew.mxcl.pcre2".into(),
                    path: other_agents.join("homebrew.mxcl.pcre2.plist"),
                },
                Service {
                    label: "homebrew.mxcl.ripgrep".into(),
                    path: agents.join("homebrew.mxcl.ripgrep.plist"),
                },
            ]
        );

        for expected in [
            "missing INSTALL_RECEIPT.json for bzip2 1.0.8",
            "malformed INSTALL_RECEIPT.json for broken 1.0",
            "malformed INSTALL_RECEIPT.json for typed 1.0",
            "versions of \"mixed\" name different source taps",
            "versions of \"mixed\" disagree about installed_on_request",
        ] {
            assert!(
                inventory
                    .warnings
                    .iter()
                    .any(|warning| warning.contains(expected)),
                "missing warning {expected:?} in {:?}",
                inventory.warnings
            );
        }
        // The foreign tap layout and non-plist files are skipped without
        // inventing records; no warning is needed for the plain README.
        assert!(!inventory.warnings.iter().any(|w| w.contains("empty")));
    }

    #[test]
    fn empty_prefix_is_a_valid_empty_inventory() {
        let sandbox = tempfile::tempdir().unwrap();
        let inventory = collect(sandbox.path(), &[]).unwrap();
        assert_eq!(inventory, Inventory::default());
    }

    #[test]
    fn refuses_relative_unnormalized_or_missing_prefixes() {
        let sandbox = tempfile::tempdir().unwrap();
        assert!(collect(Path::new("relative/prefix"), &[]).is_err());
        assert!(collect(Path::new("/"), &[]).is_err());
        let unnormalized = sandbox.path().join("..").join("prefix");
        assert!(collect(&unnormalized, &[]).is_err());
        assert!(collect(&sandbox.path().join("missing"), &[]).is_err());
        let file = sandbox.path().join("not-a-directory");
        fs::write(&file, b"x").unwrap();
        assert!(collect(&file, &[]).is_err());
    }

    #[test]
    fn reports_missing_service_directories_without_failing() {
        let sandbox = tempfile::tempdir().unwrap();
        let missing = sandbox.path().join("missing agents");
        let inventory = collect(sandbox.path(), std::slice::from_ref(&missing)).unwrap();
        assert!(inventory.services.is_empty());
        assert!(
            inventory.warnings.iter().any(|warning| warning
                .contains("Services: directory does not exist")
                && warning.contains("missing agents")),
            "{:?}",
            inventory.warnings
        );
    }

    #[test]
    fn keeps_multiple_versions_and_caps_them() {
        let sandbox = tempfile::tempdir().unwrap();
        let prefix = sandbox.path();
        for index in 0..=MAX_VERSIONS_PER_PACKAGE {
            let version = format!("1.0.{index}");
            fs::create_dir_all(prefix.join("Cellar/many").join(version)).unwrap();
        }
        let inventory = collect(prefix, &[]).unwrap();
        let many = formula(&inventory, "many");
        assert_eq!(many.versions.len(), MAX_VERSIONS_PER_PACKAGE);
        assert!(
            inventory
                .warnings
                .iter()
                .any(|warning| warning.contains("keeping only")),
            "{:?}",
            inventory.warnings
        );
    }

    #[test]
    fn caps_retained_warnings_and_says_how_many_were_omitted() {
        let sandbox = tempfile::tempdir().unwrap();
        let prefix = sandbox.path();
        for index in 0..MAX_WARNINGS + 5 {
            // Missing receipts produce one warning per version.
            fs::create_dir_all(prefix.join(format!("Cellar/f{index}/1.0"))).unwrap();
        }
        let inventory = collect(prefix, &[]).unwrap();
        assert_eq!(inventory.warnings.len(), MAX_WARNINGS + 1);
        assert!(
            inventory
                .warnings
                .last()
                .unwrap()
                .contains("more warnings omitted")
        );
    }

    #[test]
    fn separates_requested_and_dependency_receipts() {
        let sandbox = tempfile::tempdir().unwrap();
        let prefix = sandbox.path();
        write_receipt(
            &prefix.join("Cellar/app/2.0/INSTALL_RECEIPT.json"),
            Some("acme/tools"),
            Some(true),
        );
        write_receipt(
            &prefix.join("Cellar/lib/1.0/INSTALL_RECEIPT.json"),
            Some("acme/tools"),
            Some(false),
        );
        let inventory = collect(prefix, &[]).unwrap();
        assert_eq!(formula(&inventory, "app").requested, Some(true));
        assert_eq!(formula(&inventory, "lib").requested, Some(false));
        assert_eq!(
            formula(&inventory, "app").tap.as_deref(),
            Some("acme/tools")
        );
    }

    #[test]
    fn rejects_implausible_receipt_metadata_but_keeps_the_version() {
        let sandbox = tempfile::tempdir().unwrap();
        let prefix = sandbox.path();
        write_receipt(
            &prefix.join("Cellar/odd/1.0/INSTALL_RECEIPT.json"),
            Some("not a tap"),
            Some(true),
        );
        fs::create_dir_all(prefix.join("Cellar/odd/2.0")).unwrap();
        fs::write(
            prefix.join("Cellar/odd/2.0/INSTALL_RECEIPT.json"),
            vec![b' '; MAX_RECEIPT_BYTES as usize + 1],
        )
        .unwrap();
        let inventory = collect(prefix, &[]).unwrap();
        let odd = formula(&inventory, "odd");
        assert_eq!(odd.versions, vec!["1.0", "2.0"]);
        assert_eq!(odd.tap, None);
        assert_eq!(odd.requested, Some(true));
        assert!(
            inventory
                .warnings
                .iter()
                .any(|warning| warning.contains("implausible source tap")),
            "{:?}",
            inventory.warnings
        );
        assert!(
            inventory
                .warnings
                .iter()
                .any(|warning| warning.contains("exceeds the")),
            "{:?}",
            inventory.warnings
        );
    }

    #[cfg(unix)]
    #[test]
    fn never_follows_symlinks_for_cellar_casks_taps_or_services() {
        use std::os::unix::fs::symlink;

        let sandbox = tempfile::tempdir().unwrap();
        let outside = sandbox.path().join("outside");
        let prefix = sandbox.path().join("prefix");
        fs::create_dir_all(prefix.join("Cellar")).unwrap();
        fs::create_dir_all(prefix.join("Caskroom")).unwrap();
        fs::create_dir_all(prefix.join("Library/Taps/acme")).unwrap();

        // A symlinked formula directory pointing at a real receipt elsewhere.
        write_receipt(
            &outside.join("Cellar/evil/1.0/INSTALL_RECEIPT.json"),
            Some("homebrew/core"),
            Some(true),
        );
        symlink(outside.join("Cellar/evil"), prefix.join("Cellar/evil")).unwrap();

        // A real keg whose receipt is a symlink to an outside receipt.
        fs::create_dir_all(prefix.join("Cellar/linked/1.0")).unwrap();
        write_receipt(
            &outside.join("receipt.json"),
            Some("homebrew/core"),
            Some(true),
        );
        symlink(
            outside.join("receipt.json"),
            prefix.join("Cellar/linked/1.0/INSTALL_RECEIPT.json"),
        )
        .unwrap();

        // A symlinked version directory.
        fs::create_dir_all(prefix.join("Cellar/versioned")).unwrap();
        write_receipt(
            &outside.join("keg/2.0/INSTALL_RECEIPT.json"),
            Some("homebrew/core"),
            Some(true),
        );
        symlink(outside.join("keg/2.0"), prefix.join("Cellar/versioned/2.0")).unwrap();

        // A symlinked Caskroom token and tap repository.
        fs::create_dir_all(outside.join("Caskroom/app/1.0")).unwrap();
        symlink(outside.join("Caskroom/app"), prefix.join("Caskroom/app")).unwrap();
        fs::create_dir_all(outside.join("tap-repo")).unwrap();
        symlink(
            outside.join("tap-repo"),
            prefix.join("Library/Taps/acme/homebrew-evil"),
        )
        .unwrap();

        // A symlinked service plist.
        let agents = sandbox.path().join("agents");
        fs::create_dir_all(&agents).unwrap();
        fs::write(outside.join("service.plist"), b"<plist/>").unwrap();
        symlink(
            outside.join("service.plist"),
            agents.join("homebrew.mxcl.evil.plist"),
        )
        .unwrap();

        let inventory = collect(&prefix, &[agents]).unwrap();
        assert!(inventory.casks.is_empty());
        assert!(inventory.taps.is_empty());
        assert!(inventory.services.is_empty());
        assert_eq!(
            inventory
                .formulae
                .iter()
                .map(|formula| formula.name.as_str())
                .collect::<Vec<_>>(),
            vec!["linked"]
        );
        assert_eq!(formula(&inventory, "linked").tap, None);
        for expected in ["symlinked formula directory", "symlinked install receipt"] {
            assert!(
                inventory
                    .warnings
                    .iter()
                    .any(|warning| warning.contains(expected)),
                "missing warning {expected:?} in {:?}",
                inventory.warnings
            );
        }
        assert!(
            !inventory
                .formulae
                .iter()
                .any(|formula| formula.name == "evil")
        );
    }

    #[cfg(unix)]
    #[test]
    fn skips_non_utf8_names_without_guessing() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        let sandbox = tempfile::tempdir().unwrap();
        let prefix = sandbox.path();
        fs::create_dir_all(prefix.join("Cellar")).unwrap();
        if let Err(error) = fs::create_dir(
            prefix
                .join("Cellar")
                .join(OsStr::from_bytes(b"bad\xffname")),
        ) {
            // Some harness sandboxes and filesystems (such as macOS APFS with
            // EILSEQ) refuse non-UTF-8 paths before creating them; the branch is
            // only unreachable on such hosts.
            if error.kind() == std::io::ErrorKind::PermissionDenied
                || (cfg!(target_os = "macos") && error.raw_os_error() == Some(92))
            {
                return;
            }
            panic!("could not create the non-UTF-8 fixture: {error}");
        }
        let inventory = collect(prefix, &[]).unwrap();
        assert!(inventory.formulae.is_empty());
        assert!(
            inventory
                .warnings
                .iter()
                .any(|warning| warning.contains("non-UTF-8")),
            "{:?}",
            inventory.warnings
        );
    }
}
