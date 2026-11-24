//! Validation and deferred creation of keg-internal relative symlinks.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

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
pub(super) fn parse_symlink_target(link: &Path, bytes: &[u8]) -> Result<PathBuf> {
    if bytes.is_empty() {
        bail!(
            "bottle archive symlink '{}' has an empty target",
            link.display()
        );
    }
    if bytes.len() > MAX_LINK_TARGET_BYTES {
        bail!(
            "bottle archive symlink '{}' has a target longer than {MAX_LINK_TARGET_BYTES} bytes",
            link.display()
        );
    }
    if bytes
        .iter()
        .any(|byte| *byte == b'\\' || *byte == 0 || byte.is_ascii_control())
    {
        bail!(
            "bottle archive symlink '{}' has a target containing a backslash, NUL, or control byte",
            link.display()
        );
    }
    let target = std::str::from_utf8(bytes).ok().with_context(|| {
        format!(
            "bottle archive symlink '{}' has a target that is not valid UTF-8",
            link.display()
        )
    })?;
    if target.starts_with('/') {
        bail!(
            "bottle archive symlink '{}' has an absolute target",
            link.display()
        );
    }
    #[cfg(windows)]
    if target.starts_with('\\') || target.contains(':') {
        bail!(
            "bottle archive symlink '{}' has a Windows absolute or drive target",
            link.display()
        );
    }
    let components: Vec<&str> = target.split('/').collect();
    if components.len() > MAX_LINK_COMPONENTS {
        bail!(
            "bottle archive symlink '{}' has a target with too many components",
            link.display()
        );
    }
    if components.iter().any(|component| component.is_empty()) {
        bail!(
            "bottle archive symlink '{}' has a target with an empty component",
            link.display()
        );
    }
    if components
        .iter()
        .any(|component| component.len() > MAX_COMPONENT_BYTES)
    {
        bail!(
            "bottle archive symlink '{}' has a target with an overlong component",
            link.display()
        );
    }

    // The link name is a validated, normalized UTF-8 path, so its components
    // are safe to use as the lexical starting point.
    let link_name = link
        .to_str()
        .context("bottle archive symlink path is not valid UTF-8")?;
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
                    bail!(
                        "bottle archive symlink '{}' has a target that uses '..' after a normal component",
                        link.display()
                    );
                }
                if resolved.pop().is_none() {
                    bail!(
                        "bottle archive symlink '{}' escapes the keg root",
                        link.display()
                    );
                }
            }
            other => {
                normal_component = true;
                if resolved.len() >= MAX_DEPTH {
                    bail!(
                        "bottle archive symlink '{}' has a target that exceeds the depth limit of {MAX_DEPTH}",
                        link.display()
                    );
                }
                resolved.push(other);
            }
        }
    }
    if resolved.is_empty() {
        bail!(
            "bottle archive symlink '{}' resolves to the keg root",
            link.display()
        );
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
) -> Result<()> {
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
                Some(Seen::File) => bail!(
                    "bottle archive symlink '{}' targets a path through a regular file",
                    link.display()
                ),
                Some(Seen::Dir) => {
                    if is_leaf {
                        // A directory link to one of its own ancestors can be
                        // followed forever (for example `lib/sub/link -> ..`),
                        // so it is not a simple link.
                        if link.starts_with(&prefix) {
                            bail!(
                                "bottle archive symlink '{}' targets an ancestor directory, which would form a cycle",
                                link.display()
                            );
                        }
                        return Ok(());
                    }
                }
                Some(Seen::Symlink) => {
                    if !visited.insert(prefix.clone()) {
                        bail!(
                            "bottle archive symlink '{}' forms a symlink cycle",
                            link.display()
                        );
                    }
                    hops += 1;
                    if hops > MAX_SYMLINK_HOPS {
                        bail!(
                            "bottle archive symlink '{}' exceeds the symlink resolution depth limit of {MAX_SYMLINK_HOPS}",
                            link.display()
                        );
                    }
                    let expanded_base = raw_targets.get(&prefix).with_context(|| {
                        format!(
                            "bottle archive symlink '{}' references unresolved symlink '{}'",
                            link.display(),
                            prefix.display()
                        )
                    })?;
                    let mut expanded = expanded_base.clone();
                    for tail in &components[index + 1..] {
                        expanded.push(tail);
                    }
                    if expanded.components().count() > MAX_DEPTH {
                        bail!(
                            "bottle archive symlink '{}' has a target that exceeds the depth limit of {MAX_DEPTH}",
                            link.display()
                        );
                    }
                    current = expanded;
                    continue 'walk;
                }
                None if directories.contains(&prefix) => {
                    if is_leaf {
                        if link.starts_with(&prefix) {
                            bail!(
                                "bottle archive symlink '{}' targets an ancestor directory, which would form a cycle",
                                link.display()
                            );
                        }
                        return Ok(());
                    }
                }
                None => bail!(
                    "bottle archive symlink '{}' targets '{}', which is not staged in the archive",
                    link.display(),
                    prefix.display()
                ),
            }
        }
        bail!(
            "bottle archive symlink '{}' resolves to an empty path",
            link.display()
        );
    }
}

/// Create the deferred symlinks after all regular entries are in place.
pub(super) fn materialize_symlinks(out: &Path, symlinks: &[PendingSymlink]) -> Result<()> {
    for link in symlinks {
        if let Some(parent) = link.path.parent()
            && !parent.as_os_str().is_empty()
        {
            create_directories(out, parent)?;
        }
        let destination = out.join(&link.path);
        if fs::symlink_metadata(&destination).is_ok() {
            bail!(
                "extraction path {} already exists; refusing to replace it with a symlink",
                destination.display()
            );
        }
        let target = relative_link_target(&link.path, &link.target);
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            symlink(&target, &destination).with_context(|| {
                format!(
                    "failed to create extracted symlink {}",
                    destination.display()
                )
            })?;
        }
        #[cfg(not(unix))]
        {
            let _ = (&target, &destination);
            bail!(
                "bottle archive symlink '{}' cannot be created on this platform",
                link.path.display()
            );
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
