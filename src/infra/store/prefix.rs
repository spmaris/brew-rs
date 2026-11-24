//! Binding a prefix: marker, ownership, and protected-prefix checks.

use std::ffi::OsStr;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use super::StoreError;

use super::paths::{
    bin_dir, cellar_dir, marker_path, meta_dir, opt_dir, receipts_dir, retained_dir, sbin_dir,
    stages_dir, state_dir, sync_directory,
};
use super::{MARKER_CONTENT, RootState, Store};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MarkerState {
    Missing,
    Valid,
}

fn marker_state(prefix: &Path) -> Result<MarkerState, StoreError> {
    let path = marker_path(prefix);
    match fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(StoreError::MarkerSymlink {
            prefix: prefix.to_path_buf(),
        }),
        Ok(metadata) if !metadata.is_file() => Err(StoreError::MarkerNotRegular {
            prefix: prefix.to_path_buf(),
        }),
        Ok(metadata) => {
            if metadata.len() > MARKER_CONTENT.len() as u64 {
                return Err(StoreError::MarkerNotBrewrs {
                    prefix: prefix.to_path_buf(),
                });
            }
            let contents = fs::read(&path).map_err(StoreError::io("failed to read", &path))?;
            if contents == MARKER_CONTENT.as_bytes() {
                Ok(MarkerState::Valid)
            } else {
                Err(StoreError::MarkerForeign {
                    prefix: prefix.to_path_buf(),
                })
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(MarkerState::Missing),
        Err(error) => Err(StoreError::io("failed to inspect", &path)(error)),
    }
}

fn create_marker(prefix: &Path) -> Result<(), StoreError> {
    let path = marker_path(prefix);
    match OpenOptions::new().write(true).create_new(true).open(&path) {
        Ok(mut file) => {
            if let Err(error) = file
                .write_all(MARKER_CONTENT.as_bytes())
                .and_then(|()| file.sync_all())
            {
                let _ = fs::remove_file(&path);
                return Err(StoreError::io("failed to initialize", &path)(error));
            }
            sync_directory(prefix);
            Ok(())
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => match marker_state(prefix)? {
            MarkerState::Valid => Ok(()),
            MarkerState::Missing => Err(StoreError::MarkerVanishedDuringInit {
                prefix: prefix.to_path_buf(),
            }),
        },
        Err(error) => Err(StoreError::io("failed to create marker", &path)(error)),
    }
}

/// Require an owned directory, optionally creating it (and never a symlink).
pub(crate) fn ensure_directory(path: &Path, create: bool) -> Result<(), StoreError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(StoreError::PathSymlink {
            path: path.to_path_buf(),
        }),
        Ok(metadata) if metadata.is_dir() => Ok(()),
        Ok(_) => Err(StoreError::PathNotDirectory {
            path: path.to_path_buf(),
        }),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            if !create {
                return Err(StoreError::ManagedDirectoryMissing {
                    path: path.to_path_buf(),
                });
            }
            fs::create_dir(path).map_err(StoreError::io("failed to create", &path))?;
            ensure_directory(path, false)
        }
        Err(error) => Err(StoreError::io("failed to inspect", &path)(error)),
    }
}

/// Inspect a managed directory that is allowed to be absent.
///
/// Returns false only for a missing path; a symlink or file is an error.
pub(in crate::infra) fn check_optional_directory(path: &Path) -> Result<bool, StoreError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(StoreError::PathSymlink {
            path: path.to_path_buf(),
        }),
        Ok(metadata) if metadata.is_dir() => Ok(true),
        Ok(_) => Err(StoreError::PathNotDirectory {
            path: path.to_path_buf(),
        }),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(StoreError::io("failed to inspect", &path)(error)),
    }
}

/// Reject the filesystem root and any known Homebrew prefix, resolving
/// existing ancestors so a symlink cannot point a store into Homebrew.
pub(crate) fn reject_protected_prefix(prefix: &Path) -> Result<(), StoreError> {
    let candidate = canonical_candidate(prefix)?;
    if candidate == Path::new("/") || candidate.parent().is_none() {
        return Err(StoreError::RootPrefix);
    }
    for root in protected_prefixes() {
        let Ok(root) = canonical_candidate(&root) else {
            continue;
        };
        if candidate == root || candidate.starts_with(&root) {
            return Err(StoreError::HomebrewPrefix {
                prefix: prefix.to_path_buf(),
                homebrew: root.to_path_buf(),
            });
        }
    }
    Ok(())
}

fn protected_prefixes() -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = ["/opt/homebrew", "/usr/local", "/home/linuxbrew"]
        .into_iter()
        .map(PathBuf::from)
        .collect();
    if let Some(home) = std::env::var_os("HOME") {
        roots.push(PathBuf::from(home).join(".linuxbrew"));
    }
    if let Some(prefix) = std::env::var_os("HOMEBREW_PREFIX") {
        let prefix = PathBuf::from(prefix);
        if prefix.is_absolute() {
            roots.push(prefix);
        }
    }
    roots
}

/// Resolve the path a prefix would occupy, following existing symlinks and
/// dangling ones, without requiring the missing tail to exist.
pub(crate) fn canonical_candidate(path: &Path) -> Result<PathBuf, StoreError> {
    canonical_candidate_inner(path, 0)
}

fn canonical_candidate_inner(path: &Path, depth: usize) -> Result<PathBuf, StoreError> {
    if depth > 64 {
        return Err(StoreError::TooManySymlinks {
            path: path.to_path_buf(),
        });
    }
    let mut probe = path;
    let mut tail: Vec<&OsStr> = Vec::new();
    loop {
        match fs::symlink_metadata(probe) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                // `canonicalize` gives up on dangling links, so resolve the
                // link target ourselves and keep inspecting the tail.
                let target =
                    fs::read_link(probe).map_err(StoreError::io("could not read link", &probe))?;
                let target = if target.is_absolute() {
                    target
                } else {
                    probe
                        .parent()
                        .ok_or_else(|| StoreError::NoParent {
                            path: path.to_path_buf(),
                        })?
                        .join(target)
                };
                let mut base = canonical_candidate_inner(&target, depth + 1)?;
                append_tail(&mut base, &tail);
                return Ok(base);
            }
            Ok(_) => {
                let mut base = probe
                    .canonicalize()
                    .map_err(StoreError::io("could not resolve", &path))?;
                append_tail(&mut base, &tail);
                return Ok(base);
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let Some(name) = probe.file_name() else {
                    return Err(StoreError::ResolveFailed {
                        path: path.to_path_buf(),
                        error,
                    });
                };
                tail.push(name);
                probe = probe.parent().ok_or_else(|| StoreError::NoParent {
                    path: path.to_path_buf(),
                })?;
            }
            Err(error) => {
                return Err(StoreError::io("could not resolve", &path)(error));
            }
        }
    }
}

fn append_tail(base: &mut PathBuf, tail: &[&OsStr]) {
    for part in tail.iter().rev() {
        if *part == OsStr::new("..") {
            base.pop();
        } else if *part != OsStr::new(".") {
            base.push(*part);
        }
    }
}

impl Store {
    /// Bind to `prefix`, or initialize it when `create` is true.
    ///
    /// A read-only binding never writes. An absent prefix is represented as an
    /// empty store; an existing directory without the marker is unowned and
    /// also reads as empty.
    pub(crate) fn open(prefix: &Path, create: bool) -> Result<Self, StoreError> {
        if prefix.as_os_str().is_empty() {
            return Err(StoreError::EmptyPrefix);
        }
        if !prefix.is_absolute() {
            return Err(StoreError::RelativePrefix {
                prefix: prefix.to_path_buf(),
            });
        }
        if create {
            Self::create(prefix)
        } else {
            Self::bind(prefix)
        }
    }

    /// Bind an existing store for a writer without creating or initializing
    /// anything.
    ///
    /// Ownership, the marker, and every managed directory are validated before
    /// the binding is marked writable. An absent, unowned, or foreign prefix is
    /// refused rather than initialized.
    pub(crate) fn open_existing(prefix: &Path) -> Result<Self, StoreError> {
        let mut store = Self::open(prefix, false)?;
        if store.state != RootState::Owned {
            return Err(StoreError::NotInitialized {
                prefix: prefix.to_path_buf(),
            });
        }
        store.verify_owned()?;
        store.writable = true;
        Ok(store)
    }

    /// The prefix this store is bound to.
    pub(crate) fn prefix(&self) -> &Path {
        &self.prefix
    }

    /// Whether this store is bound to an existing owned prefix.
    pub(crate) fn is_owned(&self) -> bool {
        self.state == RootState::Owned
    }

    /// Bind without writing.
    fn bind(prefix: &Path) -> Result<Self, StoreError> {
        match fs::symlink_metadata(prefix) {
            Ok(metadata) if metadata.file_type().is_symlink() => Err(StoreError::PrefixSymlink {
                prefix: prefix.to_path_buf(),
            }),
            Ok(metadata) if !metadata.is_dir() => Err(StoreError::PrefixNotDirectory {
                prefix: prefix.to_path_buf(),
            }),
            Ok(_) => {
                let state = match marker_state(prefix)? {
                    MarkerState::Valid => RootState::Owned,
                    MarkerState::Missing => RootState::Unowned,
                };
                Ok(Self {
                    prefix: prefix.to_path_buf(),
                    state,
                    writable: false,
                })
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Self {
                prefix: prefix.to_path_buf(),
                state: RootState::Absent,
                writable: false,
            }),
            Err(error) => Err(StoreError::io("failed to inspect prefix", &prefix)(error)),
        }
    }

    /// Initialize or reuse a dedicated prefix.
    fn create(prefix: &Path) -> Result<Self, StoreError> {
        reject_protected_prefix(prefix)?;
        match fs::symlink_metadata(prefix) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(StoreError::PrefixSymlink {
                    prefix: prefix.to_path_buf(),
                });
            }
            Ok(metadata) if !metadata.is_dir() => {
                return Err(StoreError::PrefixNotDirectory {
                    prefix: prefix.to_path_buf(),
                });
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                fs::create_dir_all(prefix)
                    .map_err(StoreError::io("failed to create prefix", &prefix))?;
            }
            Err(error) => {
                return Err(StoreError::io("failed to inspect prefix", &prefix)(error));
            }
        }

        // Re-check after creation: a path component may have appeared as a
        // symlink while the directories were being made.
        reject_protected_prefix(prefix)?;
        let metadata = fs::symlink_metadata(prefix)
            .map_err(StoreError::io("failed to inspect prefix", &prefix))?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(StoreError::PrefixNotRealDirectory {
                prefix: prefix.to_path_buf(),
            });
        }

        match marker_state(prefix)? {
            MarkerState::Valid => {}
            MarkerState::Missing => {
                if fs::read_dir(prefix)
                    .map_err(StoreError::io("failed to read prefix", &prefix))?
                    .next()
                    .is_some()
                {
                    // Another initializer may have won the marker race after
                    // our first check; that is not a foreign directory.
                    if marker_state(prefix)? == MarkerState::Missing {
                        return Err(StoreError::PrefixNotEmpty {
                            prefix: prefix.to_path_buf(),
                        });
                    }
                } else {
                    create_marker(prefix)?;
                }
            }
        }

        for directory in [
            cellar_dir(prefix),
            bin_dir(prefix),
            sbin_dir(prefix),
            opt_dir(prefix),
            meta_dir(prefix),
            state_dir(prefix),
            receipts_dir(prefix),
            stages_dir(prefix),
        ] {
            ensure_directory(&directory, true)?;
        }

        Ok(Self {
            prefix: prefix.to_path_buf(),
            state: RootState::Owned,
            writable: true,
        })
    }

    /// Re-validate ownership and every managed directory before writing.
    pub(in crate::infra) fn ensure_owned(&self) -> Result<(), StoreError> {
        if !self.writable {
            return Err(StoreError::ReadOnly {
                prefix: self.prefix.to_path_buf(),
            });
        }
        self.verify_owned()
    }

    /// Re-validate ownership without requiring a writable binding.
    ///
    /// Removal runs on [`Store::open`]'s read-only binding, so it uses this to
    /// refuse an absent, unowned, or foreign prefix before writing anything.
    pub(in crate::infra) fn verify_owned(&self) -> Result<(), StoreError> {
        if self.state != RootState::Owned {
            return Err(StoreError::NotInitialized {
                prefix: self.prefix.to_path_buf(),
            });
        }
        // Reject a protected location before looking at its contents: a
        // planted marker must not make a Homebrew prefix look manageable.
        reject_protected_prefix(&self.prefix)?;
        let metadata = fs::symlink_metadata(&self.prefix)
            .map_err(StoreError::io("failed to inspect prefix", &self.prefix))?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(StoreError::PrefixChanged {
                prefix: self.prefix.to_path_buf(),
            });
        }
        match marker_state(&self.prefix)? {
            MarkerState::Valid => {}
            MarkerState::Missing => {
                return Err(StoreError::MarkerDisappeared {
                    prefix: self.prefix.to_path_buf(),
                });
            }
        }
        for directory in [
            cellar_dir(&self.prefix),
            bin_dir(&self.prefix),
            meta_dir(&self.prefix),
            state_dir(&self.prefix),
            receipts_dir(&self.prefix),
            stages_dir(&self.prefix),
        ] {
            ensure_directory(&directory, false)?;
        }
        check_optional_directory(&sbin_dir(&self.prefix))?;
        check_optional_directory(&opt_dir(&self.prefix))?;
        check_optional_directory(&retained_dir(&self.prefix))?;
        check_optional_directory(&self.prefix.join("Caskroom"))?;
        check_optional_directory(&self.prefix.join("Applications"))?;
        check_optional_directory(&state_dir(&self.prefix).join("casks"))?;
        Ok(())
    }
}
