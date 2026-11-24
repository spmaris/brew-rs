//! Binding a prefix: marker, ownership, and protected-prefix checks.

use std::ffi::OsStr;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use super::paths::{
    bin_dir, cellar_dir, marker_path, meta_dir, opt_dir, receipts_dir, retained_dir, sbin_dir,
    stages_dir, state_dir, sync_directory,
};
use super::{MARKER_CONTENT, MARKER_FILE, RootState, Store};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MarkerState {
    Missing,
    Valid,
}

fn marker_state(prefix: &Path) -> Result<MarkerState> {
    let path = marker_path(prefix);
    match fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.file_type().is_symlink() => bail!(
            "refusing '{}': the {} marker is a symlink",
            prefix.display(),
            MARKER_FILE
        ),
        Ok(metadata) if !metadata.is_file() => bail!(
            "refusing '{}': the {} marker is not a regular file",
            prefix.display(),
            MARKER_FILE
        ),
        Ok(metadata) => {
            if metadata.len() > MARKER_CONTENT.len() as u64 {
                bail!(
                    "refusing '{}': the {} marker is not a brewrs marker",
                    prefix.display(),
                    MARKER_FILE
                );
            }
            let contents =
                fs::read(&path).with_context(|| format!("failed to read '{}'", path.display()))?;
            if contents == MARKER_CONTENT.as_bytes() {
                Ok(MarkerState::Valid)
            } else {
                bail!(
                    "refusing '{}': the {} marker does not belong to brewrs",
                    prefix.display(),
                    MARKER_FILE
                );
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(MarkerState::Missing),
        Err(error) => Err(error).with_context(|| format!("failed to inspect '{}'", path.display())),
    }
}

fn create_marker(prefix: &Path) -> Result<()> {
    let path = marker_path(prefix);
    match OpenOptions::new().write(true).create_new(true).open(&path) {
        Ok(mut file) => {
            if let Err(error) = file
                .write_all(MARKER_CONTENT.as_bytes())
                .and_then(|()| file.sync_all())
            {
                let _ = fs::remove_file(&path);
                return Err(error)
                    .with_context(|| format!("failed to initialize '{}'", path.display()));
            }
            sync_directory(prefix);
            Ok(())
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => match marker_state(prefix)? {
            MarkerState::Valid => Ok(()),
            MarkerState::Missing => bail!(
                "refusing '{}': the marker disappeared while initializing it",
                prefix.display()
            ),
        },
        Err(error) => {
            Err(error).with_context(|| format!("failed to create marker '{}'", path.display()))
        }
    }
}

/// Require an owned directory, optionally creating it (and never a symlink).
pub(crate) fn ensure_directory(path: &Path, create: bool) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => bail!(
            "refusing '{}': a brewrs path may not be a symlink",
            path.display()
        ),
        Ok(metadata) if metadata.is_dir() => Ok(()),
        Ok(_) => bail!(
            "refusing '{}': a brewrs path must be a directory",
            path.display()
        ),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            if !create {
                bail!(
                    "refusing '{}': the managed directory is missing",
                    path.display()
                );
            }
            fs::create_dir(path)
                .with_context(|| format!("failed to create '{}'", path.display()))?;
            ensure_directory(path, false)
        }
        Err(error) => Err(error).with_context(|| format!("failed to inspect '{}'", path.display())),
    }
}

/// Inspect a managed directory that is allowed to be absent.
///
/// Returns false only for a missing path; a symlink or file is an error.
pub(in crate::infra) fn check_optional_directory(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => bail!(
            "refusing '{}': a brewrs path may not be a symlink",
            path.display()
        ),
        Ok(metadata) if metadata.is_dir() => Ok(true),
        Ok(_) => bail!(
            "refusing '{}': a brewrs path must be a directory",
            path.display()
        ),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).with_context(|| format!("failed to inspect '{}'", path.display())),
    }
}

/// Reject the filesystem root and any known Homebrew prefix, resolving
/// existing ancestors so a symlink cannot point a store into Homebrew.
pub(crate) fn reject_protected_prefix(prefix: &Path) -> Result<()> {
    let candidate = canonical_candidate(prefix)?;
    if candidate == Path::new("/") || candidate.parent().is_none() {
        bail!("refusing to use the filesystem root as a brewrs prefix");
    }
    for root in protected_prefixes() {
        let Ok(root) = canonical_candidate(&root) else {
            continue;
        };
        if candidate == root || candidate.starts_with(&root) {
            bail!(
                "refusing to use '{}' as a brewrs prefix: it is inside the Homebrew prefix '{}'; choose a dedicated directory such as ~/.brewrs",
                prefix.display(),
                root.display()
            );
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
pub(crate) fn canonical_candidate(path: &Path) -> Result<PathBuf> {
    canonical_candidate_inner(path, 0)
}

fn canonical_candidate_inner(path: &Path, depth: usize) -> Result<PathBuf> {
    if depth > 64 {
        bail!("could not resolve '{}': too many symlinks", path.display());
    }
    let mut probe = path;
    let mut tail: Vec<&OsStr> = Vec::new();
    loop {
        match fs::symlink_metadata(probe) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                // `canonicalize` gives up on dangling links, so resolve the
                // link target ourselves and keep inspecting the tail.
                let target = fs::read_link(probe)
                    .with_context(|| format!("could not read link '{}'", probe.display()))?;
                let target = if target.is_absolute() {
                    target
                } else {
                    probe
                        .parent()
                        .with_context(|| format!("could not resolve '{}'", path.display()))?
                        .join(target)
                };
                let mut base = canonical_candidate_inner(&target, depth + 1)?;
                append_tail(&mut base, &tail);
                return Ok(base);
            }
            Ok(_) => {
                let mut base = probe
                    .canonicalize()
                    .with_context(|| format!("could not resolve '{}'", path.display()))?;
                append_tail(&mut base, &tail);
                return Ok(base);
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let Some(name) = probe.file_name() else {
                    bail!("could not resolve '{}': {error}", path.display());
                };
                tail.push(name);
                probe = probe
                    .parent()
                    .with_context(|| format!("could not resolve '{}'", path.display()))?;
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("could not resolve '{}'", path.display()));
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
    pub(crate) fn open(prefix: &Path, create: bool) -> Result<Self> {
        if prefix.as_os_str().is_empty() {
            bail!("refusing an empty brewrs prefix");
        }
        if !prefix.is_absolute() {
            bail!(
                "refusing relative brewrs prefix '{}'; use an absolute path",
                prefix.display()
            );
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
    pub(crate) fn open_existing(prefix: &Path) -> Result<Self> {
        let mut store = Self::open(prefix, false)?;
        if store.state != RootState::Owned {
            bail!(
                "refusing to modify '{}': it is not an initialized brewrs store",
                prefix.display()
            );
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
    fn bind(prefix: &Path) -> Result<Self> {
        match fs::symlink_metadata(prefix) {
            Ok(metadata) if metadata.file_type().is_symlink() => bail!(
                "refusing '{}': a brewrs prefix may not be a symlink",
                prefix.display()
            ),
            Ok(metadata) if !metadata.is_dir() => bail!(
                "refusing '{}': a brewrs prefix must be a directory",
                prefix.display()
            ),
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
            Err(error) => Err(error)
                .with_context(|| format!("failed to inspect prefix '{}'", prefix.display())),
        }
    }

    /// Initialize or reuse a dedicated prefix.
    fn create(prefix: &Path) -> Result<Self> {
        reject_protected_prefix(prefix)?;
        match fs::symlink_metadata(prefix) {
            Ok(metadata) if metadata.file_type().is_symlink() => bail!(
                "refusing '{}': a brewrs prefix may not be a symlink",
                prefix.display()
            ),
            Ok(metadata) if !metadata.is_dir() => bail!(
                "refusing '{}': a brewrs prefix must be a directory",
                prefix.display()
            ),
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                fs::create_dir_all(prefix)
                    .with_context(|| format!("failed to create prefix '{}'", prefix.display()))?;
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("failed to inspect prefix '{}'", prefix.display()));
            }
        }

        // Re-check after creation: a path component may have appeared as a
        // symlink while the directories were being made.
        reject_protected_prefix(prefix)?;
        let metadata = fs::symlink_metadata(prefix)
            .with_context(|| format!("failed to inspect prefix '{}'", prefix.display()))?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            bail!(
                "refusing '{}': the prefix is not a real directory",
                prefix.display()
            );
        }

        match marker_state(prefix)? {
            MarkerState::Valid => {}
            MarkerState::Missing => {
                if fs::read_dir(prefix)
                    .with_context(|| format!("failed to read prefix '{}'", prefix.display()))?
                    .next()
                    .is_some()
                {
                    // Another initializer may have won the marker race after
                    // our first check; that is not a foreign directory.
                    if marker_state(prefix)? == MarkerState::Missing {
                        bail!(
                            "refusing to use '{}': it is not empty and has no {} marker; choose an empty directory or an existing brewrs prefix",
                            prefix.display(),
                            MARKER_FILE
                        );
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
    pub(in crate::infra) fn ensure_owned(&self) -> Result<()> {
        if !self.writable {
            bail!(
                "refusing to modify '{}': the store was opened read-only",
                self.prefix.display()
            );
        }
        self.verify_owned()
    }

    /// Re-validate ownership without requiring a writable binding.
    ///
    /// Removal runs on [`Store::open`]'s read-only binding, so it uses this to
    /// refuse an absent, unowned, or foreign prefix before writing anything.
    pub(in crate::infra) fn verify_owned(&self) -> Result<()> {
        if self.state != RootState::Owned {
            bail!(
                "refusing to modify '{}': it is not an initialized brewrs store",
                self.prefix.display()
            );
        }
        // Reject a protected location before looking at its contents: a
        // planted marker must not make a Homebrew prefix look manageable.
        reject_protected_prefix(&self.prefix)?;
        let metadata = fs::symlink_metadata(&self.prefix)
            .with_context(|| format!("failed to inspect prefix '{}'", self.prefix.display()))?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            bail!(
                "refusing '{}': the prefix is no longer a real directory",
                self.prefix.display()
            );
        }
        match marker_state(&self.prefix)? {
            MarkerState::Valid => {}
            MarkerState::Missing => bail!(
                "refusing '{}': the {} marker disappeared",
                self.prefix.display(),
                MARKER_FILE
            ),
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
