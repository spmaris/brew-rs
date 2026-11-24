//! Reading installed and retained receipts.

use std::fs;
use std::io;

use super::StoreError;

use super::paths::{meta_dir, receipts_dir, retained_dir, state_dir};
use super::prefix::check_optional_directory;
use super::receipt::{Receipt, is_safe_package_name, read_receipt};
use super::{RootState, Store};
use crate::domain::validate::is_safe_version;

impl Store {
    /// Read every receipt, sorted by package name.
    pub(crate) fn installed(&self) -> Result<Vec<Receipt>, StoreError> {
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
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(StoreError::ReceiptsDirectorySymlink {
                    path: directory.to_path_buf(),
                });
            }
            Ok(metadata) if !metadata.is_dir() => {
                return Err(StoreError::ReceiptsPathNotDirectory {
                    path: directory.to_path_buf(),
                });
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => {
                return Err(StoreError::io("failed to inspect", &directory)(error));
            }
        }

        let mut receipts = Vec::new();
        for entry in
            fs::read_dir(&directory).map_err(StoreError::io("failed to read", &directory))?
        {
            let entry = entry.map_err(StoreError::io("failed to read an entry in", &directory))?;
            let Some(file_name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            let Some(stem) = file_name.strip_suffix(".json") else {
                continue;
            };
            let receipt = read_receipt(&entry.path())?;
            if receipt.name != stem {
                return Err(StoreError::ReceiptNameMismatch {
                    path: entry.path().to_path_buf(),
                    actual: receipt.name.to_string(),
                    expected: stem.to_string(),
                });
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
    pub(crate) fn retained(&self) -> Result<Vec<Receipt>, StoreError> {
        if self.state != RootState::Owned {
            return Ok(Vec::new());
        }
        // A read-only caller (for example a reverse dependency query) must
        // not be redirected through a planted symlink: every ancestor of the
        // retained records has to be a real directory.
        for ancestor in [meta_dir(&self.prefix), state_dir(&self.prefix)] {
            match fs::symlink_metadata(&ancestor) {
                Ok(metadata) if metadata.file_type().is_symlink() => {
                    return Err(StoreError::ReadSymlink {
                        path: ancestor.to_path_buf(),
                    });
                }
                Ok(metadata) if !metadata.is_dir() => {
                    return Err(StoreError::ReadNotDirectory {
                        path: ancestor.to_path_buf(),
                    });
                }
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
                Err(error) => {
                    return Err(StoreError::io("failed to inspect", &ancestor)(error));
                }
            }
        }
        let directory = retained_dir(&self.prefix);
        match fs::symlink_metadata(&directory) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(StoreError::RetainedDirectorySymlink {
                    path: directory.to_path_buf(),
                });
            }
            Ok(metadata) if !metadata.is_dir() => {
                return Err(StoreError::RetainedPathNotDirectory {
                    path: directory.to_path_buf(),
                });
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => {
                return Err(StoreError::io("failed to inspect", &directory)(error));
            }
        }

        let mut records = Vec::new();
        for package in
            fs::read_dir(&directory).map_err(StoreError::io("failed to read", &directory))?
        {
            let package =
                package.map_err(StoreError::io("failed to read an entry in", &directory))?;
            let metadata = fs::symlink_metadata(package.path())
                .map_err(StoreError::io("failed to inspect", &package.path()))?;
            if metadata.file_type().is_symlink() {
                return Err(StoreError::RetainedPackageSymlink {
                    path: package.path().to_path_buf(),
                });
            }
            if !metadata.is_dir() {
                return Err(StoreError::NotRetainedPackageDirectory {
                    directory: directory.to_path_buf(),
                    path: package.path().to_path_buf(),
                });
            }
            let Some(name) = package.file_name().to_str().map(str::to_owned) else {
                return Err(StoreError::RetainedPackageNonUtf8 {
                    directory: directory.to_path_buf(),
                });
            };
            if !is_safe_package_name(&name) {
                return Err(StoreError::InvalidRetainedPackageName {
                    directory: directory.to_path_buf(),
                    name: name.to_string(),
                });
            }
            let package_dir = package.path();
            for record in fs::read_dir(&package_dir)
                .map_err(StoreError::io("failed to read", &package_dir))?
            {
                let record =
                    record.map_err(StoreError::io("failed to read an entry in", &package_dir))?;
                let Some(file_name) = record.file_name().to_str().map(str::to_owned) else {
                    continue;
                };
                let Some(version) = file_name.strip_suffix(".json") else {
                    continue;
                };
                if !is_safe_version(version) {
                    return Err(StoreError::InvalidRetainedVersion {
                        path: record.path().to_path_buf(),
                        version: version.to_string(),
                    });
                }
                let receipt = read_receipt(&record.path())?;
                if receipt.name != name {
                    return Err(StoreError::RetainedRecordNameMismatch {
                        path: record.path().to_path_buf(),
                        actual: receipt.name.to_string(),
                        expected: name.to_string(),
                    });
                }
                if receipt.version != version {
                    return Err(StoreError::RetainedRecordVersionMismatch {
                        path: record.path().to_path_buf(),
                        actual: receipt.version.to_string(),
                        expected: version.to_string(),
                    });
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
