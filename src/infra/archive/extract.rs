//! Streaming of tar entries into the staging directory.

use std::collections::{HashMap, HashSet};
use std::fs::OpenOptions;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use tar::EntryType;

use super::error::ArchiveError;
use super::limits::{
    MAX_DECOMPRESSED_BYTES, MAX_DEPTH, MAX_ENTRIES, MAX_FILE_BYTES, MAX_LINK_TARGET_BYTES,
    MAX_SYMLINKS,
};
use super::paths::{check_ancestors, create_directories, relative_path, set_mode};
use super::symlinks::{parse_symlink_target, validate_symlink_chain};

/// A symlink entry buffered during tar streaming and validated once every
/// entry in the archive is known.
struct RawSymlink {
    path: PathBuf,
    target_bytes: Vec<u8>,
}

/// A syntactically and semantically validated symlink whose creation is
/// deferred until the archive is completely verified.
pub(super) struct PendingSymlink {
    /// Keg-relative location of the link.
    pub(super) path: PathBuf,
    /// Keg-relative path the link resolves to.
    pub(super) target: PathBuf,
}

/// Walk the tar stream and materialize validated entries below `out`.
///
/// Valid symlinks are collected and returned rather than created, so the
/// caller never traverses them while the archive is still being read.
/// Validation runs after every tar entry is seen so forward-referenced
/// keg-internal symlinks (such as `bin/go -> ../libexec/bin/go`) succeed.
pub(super) fn extract<R: Read>(
    tar: &mut tar::Archive<R>,
    out: &Path,
    name: &str,
    version: Option<&str>,
) -> Result<Vec<PendingSymlink>, ArchiveError> {
    let mut seen: HashMap<PathBuf, Seen> = HashMap::new();
    let mut directories: HashSet<PathBuf> = HashSet::new();
    let mut raw_symlinks: Vec<RawSymlink> = Vec::new();
    let mut count = 0_usize;
    let mut declared_total = 0_u64;

    let entries = tar.entries().map_err(|source| ArchiveError::Stream {
        action: "read the bottle archive as tar",
        source,
    })?;
    for entry in entries {
        let mut entry = entry.map_err(|source| ArchiveError::Stream {
            action: "read a bottle archive entry",
            source,
        })?;
        count = count
            .checked_add(1)
            .ok_or(ArchiveError::EntryCountOverflow)?;
        if count > MAX_ENTRIES {
            return Err(ArchiveError::TooManyEntries { limit: MAX_ENTRIES });
        }

        let seen_kind = match entry.header().entry_type() {
            EntryType::Regular => Seen::File,
            EntryType::Directory => Seen::Dir,
            EntryType::Symlink => Seen::Symlink,
            entry_type => return Err(ArchiveError::UnsupportedEntryType { entry_type }),
        };
        let is_dir = seen_kind == Seen::Dir;

        let path_bytes = entry.path_bytes();
        let Some(relative) = relative_path(&path_bytes, name, version, is_dir)? else {
            continue;
        };
        if relative.components().count() > MAX_DEPTH {
            return Err(ArchiveError::EntryTooDeep {
                path: relative,
                limit: MAX_DEPTH,
            });
        }

        // Vendor Ruby metadata lives in `<name>/<version>/.brew`. It is never
        // extracted or evaluated, so it must not count as a staged target.
        if relative.starts_with(".brew") {
            continue;
        }

        note_directories(&relative, &mut directories);
        if seen.insert(relative.clone(), seen_kind).is_some() {
            return Err(ArchiveError::DuplicatePath { path: relative });
        }

        check_ancestors(&relative, &seen)?;

        if seen_kind == Seen::Symlink {
            if directories.contains(&relative) {
                return Err(ArchiveError::SymlinkAncestor { path: relative });
            }
            if entry.size() != 0 {
                return Err(ArchiveError::SymlinkNonzeroSize { path: relative });
            }
            let Some(target_bytes) = entry.link_name_bytes() else {
                return Err(ArchiveError::SymlinkNoTarget { path: relative });
            };
            if target_bytes.len() > MAX_LINK_TARGET_BYTES {
                return Err(ArchiveError::SymlinkTargetTooLong {
                    path: relative,
                    limit: MAX_LINK_TARGET_BYTES,
                });
            }
            if raw_symlinks.len() >= MAX_SYMLINKS {
                return Err(ArchiveError::TooManySymlinks {
                    limit: MAX_SYMLINKS,
                });
            }
            raw_symlinks.push(RawSymlink {
                path: relative,
                target_bytes: target_bytes.into_owned(),
            });
            continue;
        }

        let destination = out.join(&relative);

        if is_dir {
            create_directories(out, &relative)?;
            continue;
        }

        let size = entry.size();
        if size > MAX_FILE_BYTES {
            return Err(ArchiveError::EntryTooLarge {
                path: relative,
                limit: MAX_FILE_BYTES,
            });
        }
        declared_total = declared_total
            .checked_add(size)
            .ok_or(ArchiveError::ExpandedSizeOverflow)?;
        if declared_total > MAX_DECOMPRESSED_BYTES {
            return Err(ArchiveError::ExpandedTooLarge {
                limit: MAX_DECOMPRESSED_BYTES,
            });
        }

        if let Some(parent) = relative.parent() {
            create_directories(out, parent)?;
        }

        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(&destination)
            .map_err(|source| ArchiveError::Io {
                action: "create extracted file",
                path: destination.clone(),
                source,
            })?;
        let copied = io::copy(&mut entry, &mut file).map_err(|source| ArchiveError::Io {
            action: "extract file",
            path: destination.clone(),
            source,
        })?;
        if copied != size {
            return Err(ArchiveError::EntryTruncated {
                path: relative,
                expected: size,
                actual: copied,
            });
        }
        file.flush().map_err(|source| ArchiveError::Io {
            action: "flush extracted file",
            path: destination.clone(),
            source,
        })?;
        drop(file);

        let mode = entry
            .header()
            .mode()
            .map_err(|source| ArchiveError::ModeRead {
                path: relative.clone(),
                source,
            })?;
        set_mode(&destination, 0o644 | (mode & 0o111))?;
    }

    let mut parsed = Vec::with_capacity(raw_symlinks.len());
    let mut raw_targets = HashMap::with_capacity(raw_symlinks.len());
    for raw in raw_symlinks {
        if directories.contains(&raw.path) {
            return Err(ArchiveError::SymlinkAncestor { path: raw.path });
        }
        let immediate = parse_symlink_target(&raw.path, &raw.target_bytes)?;
        raw_targets.insert(raw.path.clone(), immediate.clone());
        parsed.push((raw.path, immediate));
    }

    let mut symlinks = Vec::with_capacity(parsed.len());
    for (path, immediate) in parsed {
        validate_symlink_chain(&path, &immediate, &seen, &directories, &raw_targets)?;
        symlinks.push(PendingSymlink {
            path,
            target: immediate,
        });
    }

    Ok(symlinks)
}

/// Whether an archive path is an extracted file, directory, or deferred link.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Seen {
    File,
    Dir,
    Symlink,
}

/// Record every proper ancestor directory of a validated entry path.
///
/// This tracks directories that were only created implicitly because a file or
/// link lives below them, so a later symlink cannot claim one of those paths.
fn note_directories(relative: &Path, directories: &mut HashSet<PathBuf>) {
    let mut current = PathBuf::new();
    let mut components = relative.components().peekable();
    while let Some(component) = components.next() {
        if components.peek().is_none() {
            break;
        }
        current.push(component);
        directories.insert(current.clone());
    }
}
