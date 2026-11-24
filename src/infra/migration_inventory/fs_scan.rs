use std::fs;
use std::path::Path;

use super::limits::{MAX_DIRECTORY_ENTRIES, MAX_NAME_LEN};
use super::warnings::Warnings;

pub(super) fn directory_entries(
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
pub(super) fn real_directory(
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

pub(super) fn entry_name(
    entry: &fs::DirEntry,
    area: &str,
    warnings: &mut Warnings,
) -> Option<String> {
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

pub(super) fn is_hidden(name: &str) -> bool {
    name.starts_with('.')
}

/// Conservative names for formulae, casks, versions, tap parts and labels.
///
/// These characters cover every real Homebrew name this inventory reports
/// (`python@3.12`, `libpng`, `1.2.3_1`, `homebrew.mxcl.ripgrep`) while keeping
/// separators, control bytes, spaces and traversal components out.
pub(super) fn plausible_component(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_NAME_LEN
        && !value.starts_with('.')
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_' | b'+' | b'@')
        })
}

pub(super) fn valid_tap_name(value: &str) -> bool {
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
