use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io::Read;
use std::path::Path;

use serde::Deserialize;

use super::Formula;
use super::fs_scan::{
    directory_entries, entry_name, is_hidden, plausible_component, real_directory, valid_tap_name,
};
use super::limits::{MAX_FORMULAE, MAX_RECEIPT_BYTES, MAX_VERSIONS_PER_PACKAGE};
use super::warnings::Warnings;

pub(super) fn cellar(prefix: &Path, warnings: &mut Warnings) -> Vec<Formula> {
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

pub(super) fn package_versions(
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

#[derive(Debug, Default, Deserialize)]
pub(super) struct InstallReceipt {
    #[serde(default)]
    installed_on_request: Option<bool>,
    #[serde(default)]
    source: Option<ReceiptSource>,
}

#[derive(Debug, Default, Deserialize)]
pub(super) struct ReceiptSource {
    #[serde(default)]
    tap: Option<String>,
}

#[derive(Debug, Default)]
pub(super) struct ReceiptFacts {
    tap: Option<String>,
    requested: Option<bool>,
}

pub(super) fn read_receipt(
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
