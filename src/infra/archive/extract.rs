//! Streaming of tar entries into the staging directory.

use std::collections::{HashMap, HashSet};
use std::fs::OpenOptions;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use tar::EntryType;

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
) -> Result<Vec<PendingSymlink>> {
    let mut seen: HashMap<PathBuf, Seen> = HashMap::new();
    let mut directories: HashSet<PathBuf> = HashSet::new();
    let mut raw_symlinks: Vec<RawSymlink> = Vec::new();
    let mut count = 0_usize;
    let mut declared_total = 0_u64;

    let entries = tar
        .entries()
        .context("failed to read the bottle archive as tar")?;
    for entry in entries {
        let mut entry = entry.context("failed to read a bottle archive entry")?;
        count = count
            .checked_add(1)
            .context("bottle archive entry count overflowed")?;
        if count > MAX_ENTRIES {
            bail!("bottle archive has more than {MAX_ENTRIES} entries");
        }

        let seen_kind = match entry.header().entry_type() {
            EntryType::Regular => Seen::File,
            EntryType::Directory => Seen::Dir,
            EntryType::Symlink => Seen::Symlink,
            other => bail!("unsupported bottle archive entry type {other:?}"),
        };
        let is_dir = seen_kind == Seen::Dir;

        let path_bytes = entry.path_bytes();
        let Some(relative) = relative_path(&path_bytes, name, version, is_dir)? else {
            continue;
        };
        if relative.components().count() > MAX_DEPTH {
            bail!(
                "bottle archive entry '{}' exceeds the depth limit of {MAX_DEPTH}",
                relative.display()
            );
        }

        // Vendor Ruby metadata lives in `<name>/<version>/.brew`. It is never
        // extracted or evaluated, so it must not count as a staged target.
        if relative.starts_with(".brew") {
            continue;
        }

        note_directories(&relative, &mut directories);
        if seen.insert(relative.clone(), seen_kind).is_some() {
            bail!(
                "bottle archive contains duplicate path '{}'",
                relative.display()
            );
        }

        check_ancestors(&relative, &seen)?;

        if seen_kind == Seen::Symlink {
            if directories.contains(&relative) {
                bail!(
                    "bottle archive symlink '{}' is used as an ancestor directory",
                    relative.display()
                );
            }
            if entry.size() != 0 {
                bail!(
                    "bottle archive symlink '{}' declares a nonzero size",
                    relative.display()
                );
            }
            let Some(target_bytes) = entry.link_name_bytes() else {
                bail!(
                    "bottle archive symlink '{}' has no target",
                    relative.display()
                );
            };
            if target_bytes.len() > MAX_LINK_TARGET_BYTES {
                bail!(
                    "bottle archive symlink '{}' has a target longer than {MAX_LINK_TARGET_BYTES} bytes",
                    relative.display()
                );
            }
            if raw_symlinks.len() >= MAX_SYMLINKS {
                bail!("bottle archive has more than {MAX_SYMLINKS} symlinks");
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
            bail!(
                "bottle archive entry '{}' exceeds the {MAX_FILE_BYTES} byte limit",
                relative.display()
            );
        }
        declared_total = declared_total
            .checked_add(size)
            .context("bottle archive expanded size overflowed")?;
        if declared_total > MAX_DECOMPRESSED_BYTES {
            bail!("bottle archive expands beyond the {MAX_DECOMPRESSED_BYTES} byte limit");
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
        let mut file = options.open(&destination).with_context(|| {
            format!("failed to create extracted file {}", destination.display())
        })?;
        let copied = io::copy(&mut entry, &mut file)
            .with_context(|| format!("failed to extract file {}", destination.display()))?;
        if copied != size {
            bail!(
                "bottle archive entry '{}' was truncated: expected {size} bytes, got {copied}",
                relative.display()
            );
        }
        file.flush()
            .with_context(|| format!("failed to flush extracted file {}", destination.display()))?;
        drop(file);

        let mode = entry
            .header()
            .mode()
            .with_context(|| format!("failed to read the mode of '{}'", relative.display()))?;
        set_mode(&destination, 0o644 | (mode & 0o111))?;
    }

    let mut parsed = Vec::with_capacity(raw_symlinks.len());
    let mut raw_targets = HashMap::with_capacity(raw_symlinks.len());
    for raw in raw_symlinks {
        if directories.contains(&raw.path) {
            bail!(
                "bottle archive symlink '{}' is used as an ancestor directory",
                raw.path.display()
            );
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
