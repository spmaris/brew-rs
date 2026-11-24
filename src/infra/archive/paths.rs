//! Entry-path validation, ancestor checks, directory creation and modes.

use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use super::error::ArchiveError;
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
) -> Result<Option<PathBuf>, ArchiveError> {
    let trailing_slash = bytes.ends_with(b"/");
    let bytes = if trailing_slash {
        &bytes[..bytes.len() - 1]
    } else {
        bytes
    };
    if bytes.is_empty() {
        return Err(ArchiveError::EmptyPath);
    }
    if bytes
        .iter()
        .any(|byte| *byte == b'\\' || *byte == 0 || byte.is_ascii_control())
    {
        return Err(ArchiveError::PathControlBytes {
            path: String::from_utf8_lossy(bytes).into_owned(),
        });
    }
    let path = std::str::from_utf8(bytes).map_err(|_| ArchiveError::PathNotUtf8)?;
    let components: Vec<&str> = path.split('/').collect();
    if components
        .iter()
        .any(|component| component.is_empty() || *component == "." || *component == "..")
    {
        return Err(ArchiveError::PathNotNormalized {
            path: path.to_owned(),
        });
    }
    if components
        .iter()
        .any(|component| component.len() > MAX_COMPONENT_BYTES)
    {
        return Err(ArchiveError::PathOverlongComponent {
            path: path.to_owned(),
        });
    }
    #[cfg(windows)]
    if components.iter().any(|component| component.contains(':')) {
        return Err(ArchiveError::PathDrivePrefix {
            path: path.to_owned(),
        });
    }

    if components[0] != name {
        return Err(ArchiveError::UnexpectedRoot {
            path: path.to_owned(),
            expected: format!("{name}/"),
        });
    }
    let root_depth = if let Some(version) = version {
        if components.len() > 1 && components[1] != version {
            return Err(ArchiveError::UnexpectedRoot {
                path: path.to_owned(),
                expected: format!("{name}/{version}/"),
            });
        }
        2
    } else {
        1
    };
    if components.len() <= root_depth {
        if !is_dir {
            return Err(ArchiveError::PathNotDirectory {
                path: path.to_owned(),
            });
        }
        return Ok(None);
    }
    if trailing_slash && !is_dir {
        return Err(ArchiveError::FilePathTrailingSlash {
            path: path.to_owned(),
        });
    }
    let relative = components[root_depth..].iter().collect();
    Ok(Some(relative))
}

/// Reject an entry nested under a path that was already extracted as a file,
/// or under a symlink that will only be created after extraction.
pub(super) fn check_ancestors(
    relative: &Path,
    seen: &HashMap<PathBuf, Seen>,
) -> Result<(), ArchiveError> {
    let mut ancestor = PathBuf::new();
    let mut components = relative.components().peekable();
    while let Some(component) = components.next() {
        if components.peek().is_none() {
            break;
        }
        ancestor.push(component);
        match seen.get(&ancestor) {
            Some(Seen::File) => {
                return Err(ArchiveError::NestedUnderFile {
                    path: relative.to_path_buf(),
                });
            }
            Some(Seen::Symlink) => {
                return Err(ArchiveError::NestedUnderSymlink {
                    path: relative.to_path_buf(),
                });
            }
            _ => {}
        }
    }
    Ok(())
}

/// Create each component of `relative` below `out` with a normalized mode.
pub(super) fn create_directories(out: &Path, relative: &Path) -> Result<(), ArchiveError> {
    let mut current = out.to_path_buf();
    for component in relative.components() {
        current.push(component);
        match fs::create_dir(&current) {
            Ok(()) => set_mode(&current, 0o755)?,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                let metadata =
                    fs::symlink_metadata(&current).map_err(|source| ArchiveError::Io {
                        action: "inspect extraction path",
                        path: current.clone(),
                        source,
                    })?;
                if !metadata.file_type().is_dir() {
                    return Err(ArchiveError::ExtractionPathNotDirectory { path: current });
                }
            }
            Err(error) => {
                return Err(ArchiveError::Io {
                    action: "create directory",
                    path: current,
                    source: error,
                });
            }
        }
    }
    Ok(())
}

/// Apply a normalized permission mode where the platform supports one.
pub(super) fn set_mode(path: &Path, mode: u32) -> Result<(), ArchiveError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).map_err(|source| {
            ArchiveError::Io {
                action: "set permissions on",
                path: path.to_path_buf(),
                source,
            }
        })?;
    }
    #[cfg(not(unix))]
    {
        let _ = (path, mode);
    }
    Ok(())
}
