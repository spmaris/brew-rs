use std::fs;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};

use super::launchd::MAX_SERVICE_NAME_LEN;

/// Future isolated lifecycle state root. Nothing in this module creates it.
pub(super) const SERVICE_STATE_ROOT: &str = ".brewrs/services";

/// Existing reviewed preview root for the Colima trial. The preview itself
/// refuses to reuse a non-fresh trial path.
pub(super) const SERVICE_TRIAL_ROOT: &str = ".brewrs/service-trials";

/// Maximum entries observed under each brewrs-owned service state root.
pub(super) const MAX_SERVICE_LIST_ENTRIES: usize = 128;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum DirectoryObservation {
    Present,
    Absent,
    OutsidePrefix,
}

pub(super) struct ObservedState {
    pub(super) roots: Vec<StateRoot>,
    pub(super) entries: Vec<ObservedEntry>,
}

pub(super) struct StateRoot {
    pub(super) label: &'static str,
    pub(super) path: PathBuf,
    pub(super) present: bool,
    pub(super) entries: usize,
    pub(super) inspected: bool,
}

pub(super) struct ObservedEntry {
    pub(super) root: &'static str,
    pub(super) name: String,
    pub(super) kind: &'static str,
}

impl ObservedState {
    pub(super) fn describe(
        &self,
        name: &str,
        attributed: &mut Vec<(&'static str, String)>,
    ) -> String {
        let mut parts = Vec::new();
        for entry in self.entries.iter().filter(|entry| entry.name == name) {
            attributed.push((entry.root, entry.name.clone()));
            let root = if entry.root == "service-state" {
                SERVICE_STATE_ROOT
            } else {
                SERVICE_TRIAL_ROOT
            };
            parts.push(format!(
                "{root} {} present (runtime not inspected)",
                entry.kind
            ));
        }
        if parts.is_empty() {
            "no brewrs service state".to_string()
        } else {
            parts.join("; ")
        }
    }
}

pub(super) fn observe_state(prefix: &Path, inspected: bool) -> Result<ObservedState> {
    let mut roots = Vec::with_capacity(2);
    let mut entries = Vec::new();
    for (key, label, relative) in [
        ("service-state", "Service state", SERVICE_STATE_ROOT),
        ("service-trial", "Service trials", SERVICE_TRIAL_ROOT),
    ] {
        let path = prefix.join(relative);
        if !inspected {
            roots.push(StateRoot {
                label,
                path,
                present: false,
                entries: 0,
                inspected: false,
            });
            continue;
        }
        match fs::symlink_metadata(&path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                roots.push(StateRoot {
                    label,
                    path,
                    present: false,
                    entries: 0,
                    inspected: true,
                });
            }
            Err(error) => {
                return Err(error).with_context(|| format!("Could not inspect {}", path.display()));
            }
            Ok(metadata) => {
                if metadata.file_type().is_symlink() {
                    bail!(
                        "Refusing to list '{}': a brewrs service state root must not be a symlink",
                        path.display()
                    );
                }
                if !metadata.is_dir() {
                    bail!(
                        "Refusing to list '{}': a brewrs service state root must be a directory",
                        path.display()
                    );
                }
                let mut names = Vec::new();
                for entry in fs::read_dir(&path)
                    .with_context(|| format!("Could not read {}", path.display()))?
                {
                    let entry = entry.with_context(|| {
                        format!("Could not read an entry in {}", path.display())
                    })?;
                    let entry_path = entry.path();
                    let metadata = fs::symlink_metadata(&entry_path)
                        .with_context(|| format!("Could not inspect {}", entry_path.display()))?;
                    let kind = if metadata.file_type().is_symlink() {
                        "symlink (not followed)"
                    } else if metadata.is_dir() {
                        "directory"
                    } else if metadata.is_file() {
                        "file"
                    } else {
                        "special file"
                    };
                    let name = entry.file_name().to_string_lossy().into_owned();
                    if name.is_empty()
                        || name.len() > MAX_SERVICE_NAME_LEN
                        || name.chars().any(char::is_control)
                    {
                        bail!(
                            "Refusing to list '{}': an entry has an unsupported name",
                            path.display()
                        );
                    }
                    names.push((name, kind));
                    if names.len() > MAX_SERVICE_LIST_ENTRIES {
                        bail!(
                            "Refusing to list '{}': more than {MAX_SERVICE_LIST_ENTRIES} entries",
                            path.display()
                        );
                    }
                }
                names.sort_by(|left, right| left.0.cmp(&right.0));
                roots.push(StateRoot {
                    label,
                    path,
                    present: true,
                    entries: names.len(),
                    inspected: true,
                });
                entries.extend(names.into_iter().map(|(name, kind)| ObservedEntry {
                    root: key,
                    name,
                    kind,
                }));
            }
        }
    }
    Ok(ObservedState { roots, entries })
}

pub(super) fn ensure_real_directory(path: &Path, what: &str) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("Could not inspect {}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!("{what} is not a real directory: {}", path.display());
    }
    Ok(())
}

/// Observe a working directory without creating or following anything.
///
/// Inside the prefix every existing component must be a real directory; a
/// missing component means the declared directory is not present yet. Paths
/// outside the prefix are never inspected.
pub(super) fn observe_directory(prefix: &Path, path: &Path) -> Result<DirectoryObservation> {
    let Ok(relative) = path.strip_prefix(prefix) else {
        return Ok(DirectoryObservation::OutsidePrefix);
    };
    let mut current = prefix.to_path_buf();
    for component in relative.components() {
        let Component::Normal(part) = component else {
            bail!(
                "Service working_dir has an unsupported path component: {}",
                path.display()
            );
        };
        current.push(part);
        match fs::symlink_metadata(&current) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() || !metadata.is_dir() {
                    bail!(
                        "Service working_dir is not a real directory: {}",
                        current.display()
                    );
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(DirectoryObservation::Absent);
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("Could not inspect {}", current.display()));
            }
        }
    }
    Ok(DirectoryObservation::Present)
}
