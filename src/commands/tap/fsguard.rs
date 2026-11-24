use std::fs;
use std::path::{Component, Path};

use anyhow::{Context, Result, bail};

pub(super) fn read_bounded_regular(path: &Path, limit: u64, what: &str) -> Result<Vec<u8>> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("failed to inspect {what} '{}'", path.display()))?;
    if metadata.file_type().is_symlink() {
        bail!("refusing {what} '{}': it is a symlink", path.display());
    }
    if !metadata.is_file() {
        bail!(
            "refusing {what} '{}': it is not a regular file",
            path.display()
        );
    }
    if metadata.len() > limit {
        bail!(
            "refusing {what} '{}': {} bytes exceeds the {limit} byte limit",
            path.display(),
            metadata.len()
        );
    }
    let bytes =
        fs::read(path).with_context(|| format!("failed to read {what} '{}'", path.display()))?;
    if bytes.len() as u64 > limit {
        bail!(
            "refusing {what} '{}': it exceeds the {limit} byte limit",
            path.display()
        );
    }
    Ok(bytes)
}

/// Verify that every existing ancestor component below `prefix` is a real
/// directory, never a symlink. Missing components are reported as absent.
///
/// The final component is intentionally not inspected here; callers use
/// `symlink_metadata` for it so a symlinked executable or link is handled
/// explicitly.
pub(super) fn verify_ancestor_dirs(prefix: &Path, relative: &Path, what: &str) -> Result<bool> {
    if relative.as_os_str().is_empty() {
        bail!("refusing {what}: empty relative path");
    }
    if relative.is_absolute() {
        bail!("refusing {what}: path must be relative to the brewrs prefix");
    }
    let prefix_metadata = fs::symlink_metadata(prefix)
        .with_context(|| format!("failed to inspect brewrs prefix '{}'", prefix.display()))?;
    if prefix_metadata.file_type().is_symlink() || !prefix_metadata.is_dir() {
        bail!(
            "refusing {what}: brewrs prefix '{}' is not a real directory",
            prefix.display()
        );
    }

    let mut components = relative.components();
    let Some(final_component) = components.next_back() else {
        bail!("refusing {what}: empty relative path");
    };
    if !matches!(final_component, Component::Normal(_)) {
        bail!("refusing {what}: path does not end in a normal component");
    }

    let mut ancestors = Vec::new();
    for component in components {
        match component {
            Component::Normal(name) => ancestors.push(name.to_os_string()),
            _ => bail!(
                "refusing {what}: relative path contains '{}'",
                component.as_os_str().to_string_lossy()
            ),
        }
    }

    let mut current = prefix.to_path_buf();
    for name in ancestors {
        current.push(&name);
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => bail!(
                "refusing {what}: ancestor '{}' is a symlink",
                current.display()
            ),
            Ok(metadata) if metadata.is_dir() => {}
            Ok(_) => bail!(
                "refusing {what}: ancestor '{}' is not a directory",
                current.display()
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("failed to inspect '{}'", current.display()));
            }
        }
    }
    Ok(true)
}

pub(super) fn ensure_real_dir(path: &Path, what: &str) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => bail!(
            "refusing '{}': the {what} is not a real directory",
            path.display()
        ),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            bail!("{what} '{}' is missing", path.display())
        }
        Err(error) => Err(error).with_context(|| format!("failed to inspect '{}'", path.display())),
    }
}

pub(super) fn read_dir_names(directory: &Path, limit: usize, what: &str) -> Result<Vec<String>> {
    ensure_real_dir(directory, what)?;
    let mut names = Vec::new();
    for entry in fs::read_dir(directory)
        .with_context(|| format!("failed to read '{}'", directory.display()))?
    {
        let entry = entry
            .with_context(|| format!("failed to read an entry in '{}'", directory.display()))?;
        let name = entry.file_name().into_string().map_err(|_| {
            anyhow::anyhow!(
                "refusing non-UTF-8 {what} entry in '{}'",
                directory.display()
            )
        })?;
        names.push(name);
        if names.len() > limit {
            bail!(
                "refusing '{}': more than {limit} {what} entries",
                directory.display()
            );
        }
    }
    names.sort();
    Ok(names)
}

pub(super) fn directory_entries_exact(
    directory: &Path,
    expected: &[&str],
    what: &str,
) -> Result<()> {
    let names = read_dir_names(directory, expected.len() + 1, what)?;
    let mut expected: Vec<&str> = expected.to_vec();
    expected.sort_unstable();
    if names.len() != expected.len()
        || names
            .iter()
            .map(String::as_str)
            .ne(expected.iter().copied())
    {
        bail!(
            "refusing '{}': expected exactly [{}], found [{}]",
            directory.display(),
            expected.join(", "),
            names.join(", ")
        );
    }
    Ok(())
}

pub(super) fn describe_path(metadata: &fs::Metadata) -> &'static str {
    if metadata.file_type().is_symlink() {
        "symlink"
    } else if metadata.is_file() {
        "regular file"
    } else if metadata.is_dir() {
        "directory"
    } else {
        "special file"
    }
}
