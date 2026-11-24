//! Extracted-tree and archive-entry-path validation.

use std::fs;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};

use super::limits::{MAX_ARCHIVE_BYTES, MAX_ENTRIES, MAX_UNPACKED_BYTES};

/// Maximum directory depth allowed during extraction and tree validation.
const MAX_DEPTH: usize = 64;

/// Walk `root` without following symlinks, enforcing entry/depth/size bounds and
/// confirming every symlink is relative and lexically stays inside `root`.
pub(super) fn validate_extracted_tree(root: &Path) -> Result<()> {
    let mut entries = 0_usize;
    let mut total_bytes = 0_u64;
    let mut stack = vec![(root.to_path_buf(), PathBuf::new(), 0_usize)];

    while let Some((abs, rel, depth)) = stack.pop() {
        if depth > MAX_DEPTH {
            bail!("extracted cask exceeds maximum directory depth ({MAX_DEPTH})");
        }
        for child in fs::read_dir(&abs)
            .with_context(|| format!("failed to read extracted directory '{}'", abs.display()))?
        {
            let child = child?;
            entries += 1;
            if entries > MAX_ENTRIES {
                bail!("extracted cask exceeds {MAX_ENTRIES} entries");
            }
            let child_name = child.file_name();
            let child_abs = child.path();
            let child_rel = rel.join(&child_name);
            let meta = fs::symlink_metadata(&child_abs)
                .with_context(|| format!("failed to inspect '{}'", child_abs.display()))?;
            let ftype = meta.file_type();

            if ftype.is_symlink() {
                let target = fs::read_link(&child_abs)
                    .with_context(|| format!("failed to read symlink '{}'", child_abs.display()))?;
                let target_str = target.to_str().with_context(|| {
                    format!("non-UTF-8 symlink target at '{}'", child_rel.display())
                })?;
                validate_relative_symlink_lexically(&child_rel, target_str)?;
            } else if ftype.is_dir() {
                stack.push((child_abs, child_rel, depth + 1));
            } else if ftype.is_file() {
                if meta.len() > MAX_ARCHIVE_BYTES {
                    bail!(
                        "extracted file '{}' exceeds size limit",
                        child_rel.display()
                    );
                }
                total_bytes = total_bytes
                    .checked_add(meta.len())
                    .context("extracted cask byte size overflowed")?;
                if total_bytes > MAX_UNPACKED_BYTES {
                    bail!("extracted cask exceeds {MAX_UNPACKED_BYTES} bytes");
                }
            } else {
                bail!(
                    "refusing extracted special file at '{}'",
                    child_rel.display()
                );
            }
        }
    }
    Ok(())
}

pub(super) fn validate_relative_symlink_lexically(rel_link: &Path, target: &str) -> Result<()> {
    if target.is_empty() || target.contains('\0') {
        bail!("invalid symlink target at '{}'", rel_link.display());
    }
    if target.starts_with('/') {
        bail!(
            "refusing absolute symlink '{}' -> '{target}'",
            rel_link.display()
        );
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
                    bail!(
                        "refusing escaping symlink '{}' -> '{target}'",
                        rel_link.display()
                    );
                }
                depth -= 1;
            }
            Component::Normal(_) => depth += 1,
            Component::RootDir | Component::Prefix(_) => {
                bail!(
                    "refusing non-relative symlink '{}' -> '{target}'",
                    rel_link.display()
                );
            }
        }
    }
    Ok(())
}

pub(super) fn validate_archive_entry_path(raw: &str, allow_trailing_slash: bool) -> Result<()> {
    let trimmed = if allow_trailing_slash {
        raw.trim_end_matches('/')
    } else {
        raw
    };
    if trimmed.is_empty() || trimmed == "." {
        return if allow_trailing_slash && (raw == "./" || raw == ".") {
            Ok(())
        } else {
            bail!("archive contains empty entry path")
        };
    }
    if trimmed.contains('\0') || trimmed.contains('\\') || trimmed.starts_with('/') {
        bail!("archive contains unsafe entry path '{raw}'");
    }
    if let Some(first_byte) = trimmed.as_bytes().get(1)
        && *first_byte == b':'
        && trimmed.as_bytes()[0].is_ascii_alphabetic()
    {
        bail!("archive contains Windows drive path '{raw}'");
    }
    let mut depth = 0_usize;
    for comp in Path::new(trimmed).components() {
        match comp {
            Component::CurDir => {}
            Component::Normal(_) => depth += 1,
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                bail!("archive contains disallowed path component in '{raw}'");
            }
        }
    }
    if depth > MAX_DEPTH {
        bail!("archive entry '{raw}' exceeds maximum depth {MAX_DEPTH}");
    }
    Ok(())
}

pub(super) fn normalize_relative_entry_path(path: &Path) -> Result<PathBuf> {
    let raw = path
        .to_str()
        .context("archive entry path contains non-UTF-8 characters")?;
    validate_archive_entry_path(raw, true)?;
    let mut normalized = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::CurDir => {}
            Component::Normal(part) => normalized.push(part),
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                bail!("archive contains disallowed path '{raw}'");
            }
        }
    }
    Ok(normalized)
}
