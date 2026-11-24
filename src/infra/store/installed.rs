//! Reading installed and retained receipts.

use std::fs;
use std::io;

use anyhow::{Context, Result, bail};

use super::paths::{meta_dir, receipts_dir, retained_dir, state_dir};
use super::prefix::check_optional_directory;
use super::receipt::{Receipt, is_safe_package_name, read_receipt};
use super::{RootState, Store};
use crate::domain::validate::is_safe_version;

impl Store {
    /// Read every receipt, sorted by package name.
    pub(crate) fn installed(&self) -> Result<Vec<Receipt>> {
        if self.state != RootState::Owned {
            return Ok(Vec::new());
        }
        if !check_optional_directory(&meta_dir(&self.prefix))?
            || !check_optional_directory(&state_dir(&self.prefix))?
        {
            return Ok(Vec::new());
        }
        let directory = receipts_dir(&self.prefix);
        match fs::symlink_metadata(&directory) {
            Ok(metadata) if metadata.file_type().is_symlink() => bail!(
                "refusing to read '{}': the receipts directory is a symlink",
                directory.display()
            ),
            Ok(metadata) if !metadata.is_dir() => bail!(
                "refusing to read '{}': the receipts path is not a directory",
                directory.display()
            ),
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("failed to inspect '{}'", directory.display()));
            }
        }

        let mut receipts = Vec::new();
        for entry in fs::read_dir(&directory)
            .with_context(|| format!("failed to read '{}'", directory.display()))?
        {
            let entry = entry
                .with_context(|| format!("failed to read an entry in '{}'", directory.display()))?;
            let Some(file_name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            let Some(stem) = file_name.strip_suffix(".json") else {
                continue;
            };
            let receipt = read_receipt(&entry.path())?;
            if receipt.name != stem {
                bail!(
                    "receipt '{}' names '{}', not '{}'",
                    entry.path().display(),
                    receipt.name,
                    stem
                );
            }
            receipts.push(receipt);
        }
        receipts.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(receipts)
    }

    /// Read every retained keg record, sorted by name and version.
    ///
    /// A record is the exact receipt a past upgrade moved out of the active
    /// receipts directory when a surviving receipt kept pinning its keg. The
    /// directory layout is `.brewrs/state/retained/<name>/<version>.json`, so
    /// several versions of one package can be retained at once. A missing
    /// directory reads as empty, as does a binding to a prefix that is not an
    /// owned store.
    ///
    /// Reading fails closed on anything foreign or ambiguous: a symlinked
    /// retained or package directory, a package entry that is not a directory
    /// or not a valid package name, and a record that is not a regular,
    /// bounded JSON receipt naming exactly its directory and version stem.
    /// Unrelated entries that do not end in `.json` are ignored rather than
    /// misrepresented as pins.
    pub(crate) fn retained(&self) -> Result<Vec<Receipt>> {
        if self.state != RootState::Owned {
            return Ok(Vec::new());
        }
        // A read-only caller (for example a reverse dependency query) must
        // not be redirected through a planted symlink: every ancestor of the
        // retained records has to be a real directory.
        for ancestor in [meta_dir(&self.prefix), state_dir(&self.prefix)] {
            match fs::symlink_metadata(&ancestor) {
                Ok(metadata) if metadata.file_type().is_symlink() => {
                    bail!("refusing to read '{}': it is a symlink", ancestor.display())
                }
                Ok(metadata) if !metadata.is_dir() => bail!(
                    "refusing to read '{}': it is not a directory",
                    ancestor.display()
                ),
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("failed to inspect '{}'", ancestor.display()));
                }
            }
        }
        let directory = retained_dir(&self.prefix);
        match fs::symlink_metadata(&directory) {
            Ok(metadata) if metadata.file_type().is_symlink() => bail!(
                "refusing to read '{}': the retained directory is a symlink",
                directory.display()
            ),
            Ok(metadata) if !metadata.is_dir() => bail!(
                "refusing to read '{}': the retained path is not a directory",
                directory.display()
            ),
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("failed to inspect '{}'", directory.display()));
            }
        }

        let mut records = Vec::new();
        for package in fs::read_dir(&directory)
            .with_context(|| format!("failed to read '{}'", directory.display()))?
        {
            let package = package
                .with_context(|| format!("failed to read an entry in '{}'", directory.display()))?;
            let metadata = fs::symlink_metadata(package.path())
                .with_context(|| format!("failed to inspect '{}'", package.path().display()))?;
            if metadata.file_type().is_symlink() {
                bail!(
                    "refusing to read '{}': the retained package directory is a symlink",
                    package.path().display()
                );
            }
            if !metadata.is_dir() {
                bail!(
                    "refusing to read '{}': '{}' is not a retained package directory",
                    directory.display(),
                    package.path().display()
                );
            }
            let Some(name) = package.file_name().to_str().map(str::to_owned) else {
                bail!(
                    "refusing to read '{}': a retained package directory has a non-UTF-8 name",
                    directory.display()
                );
            };
            if !is_safe_package_name(&name) {
                bail!(
                    "refusing to read '{}': '{}' is not a valid retained package name",
                    directory.display(),
                    name
                );
            }
            let package_dir = package.path();
            for record in fs::read_dir(&package_dir)
                .with_context(|| format!("failed to read '{}'", package_dir.display()))?
            {
                let record = record.with_context(|| {
                    format!("failed to read an entry in '{}'", package_dir.display())
                })?;
                let Some(file_name) = record.file_name().to_str().map(str::to_owned) else {
                    continue;
                };
                let Some(version) = file_name.strip_suffix(".json") else {
                    continue;
                };
                if !is_safe_version(version) {
                    bail!(
                        "refusing to read '{}': '{}' is not a valid retained version",
                        record.path().display(),
                        version
                    );
                }
                let receipt = read_receipt(&record.path())?;
                if receipt.name != name {
                    bail!(
                        "retained record '{}' names '{}', not '{}'",
                        record.path().display(),
                        receipt.name,
                        name
                    );
                }
                if receipt.version != version {
                    bail!(
                        "retained record '{}' records version '{}', not '{}'",
                        record.path().display(),
                        receipt.version,
                        version
                    );
                }
                records.push(receipt);
            }
        }
        records.sort_by(|left, right| {
            left.name
                .cmp(&right.name)
                .then_with(|| left.version.cmp(&right.version))
        });
        Ok(records)
    }
}
