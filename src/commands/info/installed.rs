use std::cmp::Ordering;
use std::path::Path;

use crate::commands::install::prefix_path;
use crate::domain::cask_plan::{CaskVersionOrder, compare_cask_versions};
use crate::infra::api::error::error_chain;
use crate::infra::store::Store;

use super::status::with_revision;

/// A package the native store recorded in the selected prefix.
#[derive(Debug)]
pub(super) enum Installed {
    Present { version: String, requested: bool },
    Absent,
    Unknown(String),
}

pub(super) fn read_installed(prefix: Option<&Path>, name: &str) -> Installed {
    let path = match prefix_path(prefix) {
        Ok(path) => path,
        Err(error) => return Installed::Unknown(format!("{error:#}")),
    };
    let store = match Store::open(&path, false) {
        Ok(store) => store,
        Err(error) => return Installed::Unknown(error_chain(&error)),
    };
    let receipts = match store.installed() {
        Ok(receipts) => receipts,
        Err(error) => return Installed::Unknown(error_chain(&error)),
    };
    match receipts.into_iter().find(|receipt| receipt.name == name) {
        Some(receipt) => Installed::Present {
            version: receipt.version,
            requested: receipt.requested,
        },
        None => Installed::Absent,
    }
}

/// Describe the installed overlay, including whether the API stable version is
/// newer. Version comparison is conservative; unsupported syntax leaves only
/// the installed version and reason, never a guessed upgrade state.
pub(super) fn describe_installed(
    installed: &Installed,
    available: Option<&str>,
    revision: u32,
) -> String {
    match installed {
        Installed::Present { version, requested } => {
            let mut details = vec![if *requested {
                "requested"
            } else {
                "dependency"
            }];
            if let Some(stable) = available.map(str::trim).filter(|value| !value.is_empty()) {
                match crate::domain::version::compare(&with_revision(stable, revision), version) {
                    Ok(Ordering::Greater) => details.push("update available"),
                    Ok(Ordering::Equal) => details.push("up to date"),
                    Ok(Ordering::Less) => details.push("newer than the API stable"),
                    Err(_) => {}
                }
            }
            format!("{version} ({})", details.join(", "))
        }
        Installed::Absent => "not installed by brewrs".to_string(),
        Installed::Unknown(_) => "unknown".to_string(),
    }
}

#[derive(Debug)]
pub(super) enum InstalledCask {
    Present { version: String },
    Absent,
    Unknown(String),
}

pub(super) fn read_installed_cask(prefix: Option<&Path>, token: &str) -> InstalledCask {
    let path = match prefix_path(prefix) {
        Ok(path) => path,
        Err(error) => return InstalledCask::Unknown(format!("{error:#}")),
    };
    let store = match Store::open(&path, false) {
        Ok(store) => store,
        Err(error) => return InstalledCask::Unknown(error_chain(&error)),
    };
    let receipts = match crate::infra::cask_store::installed(&store) {
        Ok(receipts) => receipts,
        Err(error) => return InstalledCask::Unknown(error_chain(&error)),
    };
    match receipts.into_iter().find(|receipt| receipt.token == token) {
        Some(receipt) => InstalledCask::Present {
            version: receipt.version,
        },
        None => InstalledCask::Absent,
    }
}

pub(super) fn describe_installed_cask(installed: &InstalledCask, available: &str) -> String {
    match installed {
        InstalledCask::Present { version } => {
            let available = available.trim();
            if available.is_empty() {
                return version.clone();
            }
            let status = match compare_cask_versions(available, version) {
                CaskVersionOrder::Same => "up to date",
                CaskVersionOrder::Upgrade => "update available",
                CaskVersionOrder::Downgrade => "newer than the API version",
            };
            format!("{version} ({status})")
        }
        InstalledCask::Absent => "not installed by brewrs".to_string(),
        InstalledCask::Unknown(_) => "unknown".to_string(),
    }
}
