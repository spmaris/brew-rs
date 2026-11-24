use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use super::cellar::package_versions;
use super::fs_scan::{
    directory_entries, entry_name, is_hidden, plausible_component, real_directory,
};
use super::limits::{MAX_CASKS, MAX_SERVICES, MAX_TAPS};
use super::warnings::Warnings;
use super::{Cask, Service};

pub(super) fn caskroom(prefix: &Path, warnings: &mut Warnings) -> Vec<Cask> {
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

pub(super) fn taps(prefix: &Path, warnings: &mut Warnings) -> Vec<String> {
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

pub(super) fn services(directories: &[PathBuf], warnings: &mut Warnings) -> Vec<Service> {
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
