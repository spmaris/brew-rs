//! Validation and deferred creation of keg-internal relative symlinks.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use super::error::ArchiveError;
use super::extract::{PendingSymlink, Seen};
use super::limits::{
    MAX_COMPONENT_BYTES, MAX_DEPTH, MAX_LINK_COMPONENTS, MAX_LINK_TARGET_BYTES, MAX_SYMLINK_HOPS,
};
use super::paths::create_directories;

/// Validate a symlink target lexically and return its immediate keg-relative target.
///
/// Only relative targets that stay inside the keg are accepted. `..` may only
/// appear before any normal component, so a target can never rely on a
/// directory that a later `..` cancels. Chain, cycle, and existence checks run
/// separately in [`validate_symlink_chain`] once all archive symlinks are known.
pub(super) fn parse_symlink_target(link: &Path, bytes: &[u8]) -> Result<PathBuf, ArchiveError> {
    if bytes.is_empty() {
        return Err(ArchiveError::SymlinkEmptyTarget {
            link: link.to_path_buf(),
        });
    }
    if bytes.len() > MAX_LINK_TARGET_BYTES {
        return Err(ArchiveError::SymlinkTargetTooLong {
            path: link.to_path_buf(),
            limit: MAX_LINK_TARGET_BYTES,
        });
    }
    if bytes
        .iter()
        .any(|byte| *byte == b'\\' || *byte == 0 || byte.is_ascii_control())
    {
        return Err(ArchiveError::SymlinkTargetControlBytes {
            link: link.to_path_buf(),
        });
    }
    let target = std::str::from_utf8(bytes).map_err(|_| ArchiveError::SymlinkTargetNotUtf8 {
        link: link.to_path_buf(),
    })?;
    if target.starts_with('/') {
        return Err(ArchiveError::SymlinkAbsoluteTarget {
            link: link.to_path_buf(),
        });
    }
    #[cfg(windows)]
    if target.starts_with('\\') || target.contains(':') {
        return Err(ArchiveError::SymlinkWindowsTarget {
            link: link.to_path_buf(),
        });
    }
    let components: Vec<&str> = target.split('/').collect();
    if components.len() > MAX_LINK_COMPONENTS {
        return Err(ArchiveError::SymlinkTooManyComponents {
            link: link.to_path_buf(),
        });
    }
    if components.iter().any(|component| component.is_empty()) {
        return Err(ArchiveError::SymlinkEmptyComponent {
            link: link.to_path_buf(),
        });
    }
    if components
        .iter()
        .any(|component| component.len() > MAX_COMPONENT_BYTES)
    {
        return Err(ArchiveError::SymlinkOverlongComponent {
            link: link.to_path_buf(),
        });
    }

    // The link name is a validated, normalized UTF-8 path, so its components
    // are safe to use as the lexical starting point.
    let link_name = link.to_str().ok_or(ArchiveError::SymlinkPathNotUtf8)?;
    let link_components: Vec<&str> = link_name
        .split('/')
        .filter(|component| !component.is_empty())
        .collect();
    let mut resolved: Vec<&str> = link_components
        .get(..link_components.len().saturating_sub(1))
        .unwrap_or_default()
        .to_vec();
    let mut normal_component = false;

    for component in components {
        match component {
            "." => {}
            ".." => {
                if normal_component {
                    return Err(ArchiveError::SymlinkParentAfterNormal {
                        link: link.to_path_buf(),
                    });
                }
                if resolved.pop().is_none() {
                    return Err(ArchiveError::SymlinkEscapesRoot {
                        link: link.to_path_buf(),
                    });
                }
            }
            other => {
                normal_component = true;
                if resolved.len() >= MAX_DEPTH {
                    return Err(ArchiveError::SymlinkTargetTooDeep {
                        link: link.to_path_buf(),
                        limit: MAX_DEPTH,
                    });
                }
                resolved.push(other);
            }
        }
    }
    if resolved.is_empty() {
        return Err(ArchiveError::SymlinkResolvesToRoot {
            link: link.to_path_buf(),
        });
    }

    let mut immediate = PathBuf::new();
    for component in resolved {
        immediate.push(component);
    }
    Ok(immediate)
}

/// Resolve an immediate keg-relative symlink target purely in-memory across
/// bounded symlink hops, ensuring it is acyclic and ends at a staged regular
/// file or non-ancestor directory.
pub(super) fn validate_symlink_chain(
    link: &Path,
    immediate: &Path,
    seen: &HashMap<PathBuf, Seen>,
    directories: &HashSet<PathBuf>,
    raw_targets: &HashMap<PathBuf, PathBuf>,
) -> Result<(), ArchiveError> {
    let mut current = immediate.to_path_buf();
    let mut hops = 0usize;
    let mut visited = HashSet::new();
    visited.insert(link.to_path_buf());

    'walk: loop {
        let components: Vec<PathBuf> = current
            .components()
            .map(|component| PathBuf::from(component.as_os_str()))
            .collect();
        let mut prefix = PathBuf::new();
        for (index, component) in components.iter().enumerate() {
            prefix.push(component);
            let is_leaf = index + 1 == components.len();
            match seen.get(&prefix) {
                Some(Seen::File) if is_leaf => return Ok(()),
                Some(Seen::File) => {
                    return Err(ArchiveError::SymlinkThroughFile {
                        link: link.to_path_buf(),
                    });
                }
                Some(Seen::Dir) => {
                    if is_leaf {
                        // A directory link to one of its own ancestors can be
                        // followed forever (for example `lib/sub/link -> ..`),
                        // so it is not a simple link.
                        if link.starts_with(&prefix) {
                            return Err(ArchiveError::SymlinkAncestorCycle {
                                link: link.to_path_buf(),
                            });
                        }
                        return Ok(());
                    }
                }
                Some(Seen::Symlink) => {
                    if !visited.insert(prefix.clone()) {
                        return Err(ArchiveError::SymlinkCycle {
                            link: link.to_path_buf(),
                        });
                    }
                    hops += 1;
                    if hops > MAX_SYMLINK_HOPS {
                        return Err(ArchiveError::SymlinkHopLimit {
                            link: link.to_path_buf(),
                            limit: MAX_SYMLINK_HOPS,
                        });
                    }
                    let expanded_base = raw_targets.get(&prefix).ok_or_else(|| {
                        ArchiveError::SymlinkUnresolved {
                            link: link.to_path_buf(),
                            target: prefix.clone(),
                        }
                    })?;
                    let mut expanded = expanded_base.clone();
                    for tail in &components[index + 1..] {
                        expanded.push(tail);
                    }
                    if expanded.components().count() > MAX_DEPTH {
                        return Err(ArchiveError::SymlinkTargetTooDeep {
                            link: link.to_path_buf(),
                            limit: MAX_DEPTH,
                        });
                    }
                    current = expanded;
                    continue 'walk;
                }
                None if directories.contains(&prefix) => {
                    if is_leaf {
                        if link.starts_with(&prefix) {
                            return Err(ArchiveError::SymlinkAncestorCycle {
                                link: link.to_path_buf(),
                            });
                        }
                        return Ok(());
                    }
                }
                None => {
                    return Err(ArchiveError::SymlinkNotStaged {
                        link: link.to_path_buf(),
                        target: prefix,
                    });
                }
            }
        }
        return Err(ArchiveError::SymlinkEmptyPath {
            link: link.to_path_buf(),
        });
    }
}

/// Create the deferred symlinks after all regular entries are in place.
pub(super) fn materialize_symlinks(
    out: &Path,
    symlinks: &[PendingSymlink],
) -> Result<(), ArchiveError> {
    for link in symlinks {
        if let Some(parent) = link.path.parent()
            && !parent.as_os_str().is_empty()
        {
            create_directories(out, parent)?;
        }
        let destination = out.join(&link.path);
        if fs::symlink_metadata(&destination).is_ok() {
            return Err(ArchiveError::SymlinkDestinationExists { path: destination });
        }
        let target = relative_link_target(&link.path, &link.target);
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            symlink(&target, &destination).map_err(|source| ArchiveError::Io {
                action: "create extracted symlink",
                path: destination,
                source,
            })?;
        }
        #[cfg(not(unix))]
        {
            let _ = (&target, &destination);
            return Err(ArchiveError::SymlinkUnsupportedPlatform {
                link: link.path.clone(),
            });
        }
    }
    Ok(())
}

/// Express a keg-relative target relative to the directory holding the link.
///
/// This keeps the on-disk link free of any component that a `..` elsewhere in
/// the target could cancel, so kernel path resolution matches the lexical
/// validation exactly.
fn relative_link_target(link: &Path, target: &Path) -> PathBuf {
    let parent: Vec<_> = link
        .parent()
        .map(|parent| parent.components().collect())
        .unwrap_or_default();
    let target: Vec<_> = target.components().collect();
    let common = parent
        .iter()
        .zip(target.iter())
        .take_while(|(left, right)| left == right)
        .count();
    let mut relative = PathBuf::new();
    for _ in common..parent.len() {
        relative.push("..");
    }
    for component in &target[common..] {
        relative.push(component.as_os_str());
    }
    relative
}
