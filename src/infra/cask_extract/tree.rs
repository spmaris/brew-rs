//! Extracted-tree and archive-entry-path validation.

use std::fs;
use std::path::{Component, Path, PathBuf};

use super::error::CaskExtractError;
use super::limits::{MAX_ARCHIVE_BYTES, MAX_ENTRIES, MAX_UNPACKED_BYTES};

/// Maximum directory depth allowed during extraction and tree validation.
const MAX_DEPTH: usize = 64;

/// Walk `root` without following symlinks, enforcing entry/depth/size bounds and
/// confirming every symlink is relative and lexically stays inside `root`.
pub(super) fn validate_extracted_tree(root: &Path) -> Result<(), CaskExtractError> {
    let mut entries = 0_usize;
    let mut total_bytes = 0_u64;
    let mut stack = vec![(root.to_path_buf(), PathBuf::new(), 0_usize)];

    while let Some((abs, rel, depth)) = stack.pop() {
        if depth > MAX_DEPTH {
            return Err(CaskExtractError::TreeTooDeep { limit: MAX_DEPTH });
        }
        let children = fs::read_dir(&abs).map_err(|source| CaskExtractError::Io {
            action: "read extracted directory",
            path: abs.clone(),
            source,
        })?;
        for child in children {
            let child = child.map_err(CaskExtractError::io("read extracted directory", &abs))?;
            entries += 1;
            if entries > MAX_ENTRIES {
                return Err(CaskExtractError::TreeTooManyEntries { limit: MAX_ENTRIES });
            }
            let child_name = child.file_name();
            let child_abs = child.path();
            let child_rel = rel.join(&child_name);
            let meta = fs::symlink_metadata(&child_abs).map_err(|source| CaskExtractError::Io {
                action: "inspect",
                path: child_abs.clone(),
                source,
            })?;
            let ftype = meta.file_type();

            if ftype.is_symlink() {
                let target = fs::read_link(&child_abs).map_err(|source| CaskExtractError::Io {
                    action: "read symlink",
                    path: child_abs.clone(),
                    source,
                })?;
                let target_str =
                    target
                        .to_str()
                        .ok_or_else(|| CaskExtractError::TreeSymlinkTargetNotUtf8 {
                            path: child_rel.clone(),
                        })?;
                validate_relative_symlink_lexically(&child_rel, target_str)?;
            } else if ftype.is_dir() {
                stack.push((child_abs, child_rel, depth + 1));
            } else if ftype.is_file() {
                if meta.len() > MAX_ARCHIVE_BYTES {
                    return Err(CaskExtractError::ExtractedFileTooLarge { path: child_rel });
                }
                total_bytes = total_bytes
                    .checked_add(meta.len())
                    .ok_or(CaskExtractError::ExtractedSizeOverflow)?;
                if total_bytes > MAX_UNPACKED_BYTES {
                    return Err(CaskExtractError::ExtractedTooLarge {
                        limit: MAX_UNPACKED_BYTES,
                    });
                }
            } else {
                return Err(CaskExtractError::SpecialFile { path: child_rel });
            }
        }
    }
    Ok(())
}

pub(super) fn validate_relative_symlink_lexically(
    rel_link: &Path,
    target: &str,
) -> Result<(), CaskExtractError> {
    if target.is_empty() || target.contains('\0') {
        return Err(CaskExtractError::InvalidSymlinkTarget {
            link: rel_link.to_path_buf(),
        });
    }
    if target.starts_with('/') {
        return Err(CaskExtractError::AbsoluteSymlink {
            link: rel_link.to_path_buf(),
            target: target.to_owned(),
        });
    }
    let parent = rel_link.parent().unwrap_or_else(|| Path::new(""));
    let mut depth = parent
        .components()
        .filter(|comp| matches!(comp, Component::Normal(_)))
        .count();

    for component in Path::new(target).components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if depth == 0 {
                    return Err(CaskExtractError::EscapingSymlink {
                        link: rel_link.to_path_buf(),
                        target: target.to_owned(),
                    });
                }
                depth -= 1;
            }
            Component::Normal(_) => depth += 1,
            Component::RootDir | Component::Prefix(_) => {
                return Err(CaskExtractError::NonRelativeSymlink {
                    link: rel_link.to_path_buf(),
                    target: target.to_owned(),
                });
            }
        }
    }
    Ok(())
}

pub(super) fn validate_archive_entry_path(
    raw: &str,
    allow_trailing_slash: bool,
) -> Result<(), CaskExtractError> {
    let trimmed = if allow_trailing_slash {
        raw.trim_end_matches('/')
    } else {
        raw
    };
    if trimmed.is_empty() || trimmed == "." {
        return if allow_trailing_slash && (raw == "./" || raw == ".") {
            Ok(())
        } else {
            Err(CaskExtractError::EmptyEntryPath)
        };
    }
    if trimmed.contains('\0') || trimmed.contains('\\') || trimmed.starts_with('/') {
        return Err(CaskExtractError::UnsafeEntryPath {
            path: raw.to_owned(),
        });
    }
    if let Some(first_byte) = trimmed.as_bytes().get(1)
        && *first_byte == b':'
        && trimmed.as_bytes()[0].is_ascii_alphabetic()
    {
        return Err(CaskExtractError::WindowsDrivePath {
            path: raw.to_owned(),
        });
    }
    let mut depth = 0_usize;
    for comp in Path::new(trimmed).components() {
        match comp {
            Component::CurDir => {}
            Component::Normal(_) => depth += 1,
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(CaskExtractError::DisallowedPathComponent {
                    path: raw.to_owned(),
                });
            }
        }
    }
    if depth > MAX_DEPTH {
        return Err(CaskExtractError::EntryTooDeep {
            path: raw.to_owned(),
            limit: MAX_DEPTH,
        });
    }
    Ok(())
}

pub(super) fn normalize_relative_entry_path(path: &Path) -> Result<PathBuf, CaskExtractError> {
    let raw = path.to_str().ok_or(CaskExtractError::EntryPathNotUtf8)?;
    validate_archive_entry_path(raw, true)?;
    let mut normalized = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::CurDir => {}
            Component::Normal(part) => normalized.push(part),
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(CaskExtractError::DisallowedPath {
                    path: raw.to_owned(),
                });
            }
        }
    }
    Ok(normalized)
}
