//! Managed link directories, link targets, and staged members.

use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use super::prefix::ensure_directory;
use super::receipt::is_safe_bin_name;
use super::{BIN, CELLAR, ManagedLink, OwnedLink, SBIN};

/// Prefix-relative directory whose direct members are linked into the store,
/// together with the member rules for that directory.
///
/// `subdir` is both the keg-relative and prefix-relative path, so a link always
/// uses one `..` per path component. Every directory is optional: an older
/// brewrs prefix has only `bin`, `sbin`, and `opt`, and missing directories are
/// created on demand by the first package that provides a member.
#[derive(Clone, Copy)]
pub(super) struct LinkDirectory {
    pub(super) subdir: &'static str,
    /// Whether a member must be executable (CLI, admin CLI, Docker plugin).
    executable: bool,
    /// Optional required member-name prefix, as Docker uses for plugins.
    member_prefix: Option<&'static str>,
}

pub(super) const LINK_DIRECTORIES: [LinkDirectory; 14] = [
    LinkDirectory {
        subdir: BIN,
        executable: true,
        member_prefix: None,
    },
    LinkDirectory {
        subdir: SBIN,
        executable: true,
        member_prefix: None,
    },
    LinkDirectory {
        subdir: "share/man/man1",
        executable: false,
        member_prefix: None,
    },
    LinkDirectory {
        subdir: "share/man/man2",
        executable: false,
        member_prefix: None,
    },
    LinkDirectory {
        subdir: "share/man/man3",
        executable: false,
        member_prefix: None,
    },
    LinkDirectory {
        subdir: "share/man/man4",
        executable: false,
        member_prefix: None,
    },
    LinkDirectory {
        subdir: "share/man/man5",
        executable: false,
        member_prefix: None,
    },
    LinkDirectory {
        subdir: "share/man/man6",
        executable: false,
        member_prefix: None,
    },
    LinkDirectory {
        subdir: "share/man/man7",
        executable: false,
        member_prefix: None,
    },
    LinkDirectory {
        subdir: "share/man/man8",
        executable: false,
        member_prefix: None,
    },
    LinkDirectory {
        subdir: "etc/bash_completion.d",
        executable: false,
        member_prefix: None,
    },
    LinkDirectory {
        subdir: "share/zsh/site-functions",
        executable: false,
        member_prefix: None,
    },
    LinkDirectory {
        subdir: "share/fish/vendor_completions.d",
        executable: false,
        member_prefix: None,
    },
    LinkDirectory {
        subdir: "lib/docker/cli-plugins",
        executable: true,
        member_prefix: Some("docker-"),
    },
];

/// The exact relative target [`Store::commit`] stores for a managed member.
///
/// `subdir` is both keg- and prefix-relative, so it contributes one `..` per
/// path component: `bin/tool -> ../Cellar/.../bin/tool` and
/// `share/man/man1/tool.1 -> ../../../Cellar/.../share/man/man1/tool.1`.
pub(super) fn keg_link_target(name: &str, version: &str, subdir: &str, member: &str) -> PathBuf {
    let mut target = PathBuf::new();
    for _ in subdir.split('/') {
        target.push("..");
    }
    target
        .join(CELLAR)
        .join(name)
        .join(version)
        .join(subdir)
        .join(member)
}

/// Scan one prefix link directory for symlinks whose exact relative target
/// matches `../.../Cellar/<name>/<version>/<subdir>/<member>`.
pub(super) fn collect_owned_links(
    dir: &Path,
    root: &LinkDirectory,
    name: &str,
    version: &str,
) -> Result<Vec<OwnedLink>> {
    let mut links = Vec::new();
    for entry in fs::read_dir(dir).with_context(|| format!("failed to read '{}'", dir.display()))? {
        let entry =
            entry.with_context(|| format!("failed to read an entry in '{}'", dir.display()))?;
        if !entry
            .file_type()
            .with_context(|| format!("failed to inspect '{}'", entry.path().display()))?
            .is_symlink()
        {
            continue;
        }
        let Some(member) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if !is_safe_bin_name(&member) {
            continue;
        }
        if let Some(prefix) = root.member_prefix
            && !member.starts_with(prefix)
        {
            continue;
        }
        let path = entry.path();
        let target = keg_link_target(name, version, root.subdir, &member);
        let actual = fs::read_link(&path)
            .with_context(|| format!("failed to read link '{}'", path.display()))?;
        if actual.as_os_str() == target.as_os_str() {
            links.push(OwnedLink { path, target });
        }
    }
    Ok(links)
}

/// The exact relative target [`Store::commit`] stores for `opt/<name>`.
pub(super) fn opt_link_target(name: &str, version: &str) -> PathBuf {
    Path::new("..").join(CELLAR).join(name).join(version)
}

/// Collect every linkable member below the staged keg's managed directories.
pub(super) fn staged_links(stage: &Path) -> Result<Vec<ManagedLink>> {
    let mut links = Vec::new();
    for root in LINK_DIRECTORIES {
        for name in staged_members(stage, &root)? {
            links.push(ManagedLink {
                subdir: root.subdir,
                name,
            });
        }
    }
    Ok(links)
}

/// Resolve a managed directory below the stage without following symlinked
/// ancestors. `None` means the stage simply does not ship that directory.
fn staged_directory(stage: &Path, subdir: &str) -> Result<Option<PathBuf>> {
    let mut directory = stage.to_path_buf();
    for component in subdir.split('/') {
        directory.push(component);
        match fs::symlink_metadata(&directory) {
            Ok(metadata) if metadata.file_type().is_symlink() => bail!(
                "refusing stage '{}': '{subdir}' has a symlinked parent directory",
                stage.display()
            ),
            Ok(metadata) if metadata.is_dir() => {}
            Ok(_) => return Ok(None),
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("failed to inspect '{}'", directory.display()));
            }
        }
    }
    Ok(Some(directory))
}

/// Direct regular files or keg-contained relative symlinks below one staged
/// managed directory.
fn staged_members(stage: &Path, root: &LinkDirectory) -> Result<Vec<String>> {
    let Some(dir) = staged_directory(stage, root.subdir)? else {
        return Ok(Vec::new());
    };

    let stage_root = stage
        .canonicalize()
        .with_context(|| format!("failed to resolve stage '{}'", stage.display()))?;
    let mut members = Vec::new();
    for entry in
        fs::read_dir(&dir).with_context(|| format!("failed to read '{}'", dir.display()))?
    {
        let entry =
            entry.with_context(|| format!("failed to read an entry in '{}'", dir.display()))?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if !is_safe_bin_name(&name) {
            continue;
        }
        if let Some(prefix) = root.member_prefix
            && !name.starts_with(prefix)
        {
            continue;
        }
        let entry_path = entry.path();
        let file_type = entry
            .file_type()
            .with_context(|| format!("failed to inspect '{}'", entry_path.display()))?;
        if file_type.is_file() {
            let metadata = entry
                .metadata()
                .with_context(|| format!("failed to inspect '{}'", entry_path.display()))?;
            if !root.executable || is_executable(&metadata) {
                members.push(name);
            }
        } else if file_type.is_symlink() {
            let raw_target = fs::read_link(&entry_path)
                .with_context(|| format!("failed to read link '{}'", entry_path.display()))?;
            if raw_target.is_absolute() {
                bail!(
                    "refusing stage '{}': '{}/{}' is an absolute symlink",
                    stage.display(),
                    root.subdir,
                    name
                );
            }
            let resolved = fs::canonicalize(&entry_path).with_context(|| {
                format!(
                    "refusing stage '{}': '{}/{}' is a broken symlink",
                    stage.display(),
                    root.subdir,
                    name
                )
            })?;
            if !resolved.starts_with(&stage_root) {
                bail!(
                    "refusing stage '{}': '{}/{}' escapes the staged keg",
                    stage.display(),
                    root.subdir,
                    name
                );
            }
            let target_meta = fs::symlink_metadata(&resolved)
                .with_context(|| format!("failed to inspect '{}'", resolved.display()))?;
            if target_meta.is_file() && (!root.executable || is_executable(&target_meta)) {
                members.push(name);
            }
        }
    }
    members.sort();
    Ok(members)
}

/// Resolve `<prefix>/<subdir>` while refusing any symlinked or non-directory
/// ancestor, and optionally create missing components.
///
/// `None` means an ancestor is missing and `create` is false. Existing
/// components are never followed when they are symlinks, so a foreign link
/// cannot redirect a managed link outside the prefix.
pub(super) fn link_directory(
    prefix: &Path,
    subdir: &str,
    mut created: Option<&mut Vec<PathBuf>>,
) -> Result<Option<PathBuf>> {
    let mut directory = prefix.to_path_buf();
    for component in subdir.split('/') {
        directory.push(component);
        match fs::symlink_metadata(&directory) {
            Ok(metadata) if metadata.file_type().is_symlink() => bail!(
                "refusing '{}': a brewrs path may not be a symlink",
                directory.display()
            ),
            Ok(metadata) if metadata.is_dir() => {}
            Ok(_) => bail!(
                "refusing '{}': a brewrs path must be a directory",
                directory.display()
            ),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let Some(created) = created.as_mut() else {
                    return Ok(None);
                };
                fs::create_dir(&directory)
                    .with_context(|| format!("failed to create '{}'", directory.display()))?;
                created.push(directory.clone());
                ensure_directory(&directory, false)?;
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("failed to inspect '{}'", directory.display()));
            }
        }
    }
    Ok(Some(directory))
}

/// Every managed link directory that already exists, keyed by prefix-relative
/// path. Missing directories are absent from the map and are created on demand
/// by a package that actually provides a member.
pub(super) fn existing_link_directories(prefix: &Path) -> Result<HashMap<&'static str, PathBuf>> {
    let mut directories = HashMap::with_capacity(LINK_DIRECTORIES.len());
    for root in LINK_DIRECTORIES {
        if let Some(directory) = link_directory(prefix, root.subdir, None)? {
            directories.insert(root.subdir, directory);
        }
    }
    Ok(directories)
}

/// Create the relative CLI link, never replacing an existing path.
pub(crate) fn create_symlink(target: &Path, link: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, link)
    }
    #[cfg(not(unix))]
    {
        let _ = (target, link);
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "native CLI links are only supported on Unix",
        ))
    }
}

fn is_executable(metadata: &fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        false
    }
}
