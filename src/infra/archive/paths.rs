//! Entry-path validation, ancestor checks, directory creation and modes.

use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use super::extract::Seen;
use super::limits::MAX_COMPONENT_BYTES;

/// Validate one resolved tar path and return its path below the keg root.
///
/// `None` is returned for the redundant `<name>` and `<name>/<version>`
/// directory entries, which are allowed as ancestors but not materialized.
pub(super) fn relative_path(
    bytes: &[u8],
    name: &str,
    version: Option<&str>,
    is_dir: bool,
) -> Result<Option<PathBuf>> {
    let trailing_slash = bytes.ends_with(b"/");
    let bytes = if trailing_slash {
        &bytes[..bytes.len() - 1]
    } else {
        bytes
    };
    if bytes.is_empty() {
        bail!("bottle archive contains an empty path");
    }
    if bytes
        .iter()
        .any(|byte| *byte == b'\\' || *byte == 0 || byte.is_ascii_control())
    {
        bail!(
            "bottle archive path '{}' contains a backslash, NUL, or control byte",
            String::from_utf8_lossy(bytes)
        );
    }
    let path = std::str::from_utf8(bytes)
        .ok()
        .context("bottle archive path is not valid UTF-8")?;
    let components: Vec<&str> = path.split('/').collect();
    if components
        .iter()
        .any(|component| component.is_empty() || *component == "." || *component == "..")
    {
        bail!("bottle archive path '{path}' is not a normalized relative path");
    }
    if components
        .iter()
        .any(|component| component.len() > MAX_COMPONENT_BYTES)
    {
        bail!("bottle archive path '{path}' contains an overlong component");
    }
    #[cfg(windows)]
    if components.iter().any(|component| component.contains(':')) {
        bail!("bottle archive path '{path}' contains a Windows drive prefix");
    }

    if components[0] != name {
        bail!("bottle archive path '{path}' is outside '{name}/'");
    }
    let root_depth = if let Some(version) = version {
        if components.len() > 1 && components[1] != version {
            bail!("bottle archive path '{path}' is outside '{name}/{version}/'");
        }
        2
    } else {
        1
    };
    if components.len() <= root_depth {
        if !is_dir {
            bail!("bottle archive path '{path}' is not a directory");
        }
        return Ok(None);
    }
    if trailing_slash && !is_dir {
        bail!("bottle archive file path '{path}' ends with a slash");
    }
    let relative = components[root_depth..].iter().collect();
    Ok(Some(relative))
}

/// Reject an entry nested under a path that was already extracted as a file,
/// or under a symlink that will only be created after extraction.
pub(super) fn check_ancestors(relative: &Path, seen: &HashMap<PathBuf, Seen>) -> Result<()> {
    let mut ancestor = PathBuf::new();
    let mut components = relative.components().peekable();
    while let Some(component) = components.next() {
        if components.peek().is_none() {
            break;
        }
        ancestor.push(component);
        match seen.get(&ancestor) {
            Some(Seen::File) => bail!(
                "bottle archive path '{}' is nested under an extracted file",
                relative.display()
            ),
            Some(Seen::Symlink) => bail!(
                "bottle archive path '{}' is nested under an extracted symlink",
                relative.display()
            ),
            _ => {}
        }
    }
    Ok(())
}

/// Create each component of `relative` below `out` with a normalized mode.
pub(super) fn create_directories(out: &Path, relative: &Path) -> Result<()> {
    let mut current = out.to_path_buf();
    for component in relative.components() {
        current.push(component);
        match fs::create_dir(&current) {
            Ok(()) => set_mode(&current, 0o755)?,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                let metadata = fs::symlink_metadata(&current).with_context(|| {
                    format!("failed to inspect extraction path {}", current.display())
                })?;
                if !metadata.file_type().is_dir() {
                    bail!(
                        "extraction path {} is not a regular directory",
                        current.display()
                    );
                }
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("failed to create directory {}", current.display()));
            }
        }
    }
    Ok(())
}

/// Apply a normalized permission mode where the platform supports one.
pub(super) fn set_mode(path: &Path, mode: u32) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(mode))
            .with_context(|| format!("failed to set permissions on {}", path.display()))?;
    }
    #[cfg(not(unix))]
    {
        let _ = (path, mode);
    }
    Ok(())
}
