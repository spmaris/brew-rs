use std::fs;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};

use super::launchd::absolute_path;
use super::observe::ensure_real_directory;

pub(super) fn ensure_fresh_trial(trial: &Path) -> Result<()> {
    let parents = trial.parent().context("Trial has no parent directory")?;
    match fs::symlink_metadata(parents) {
        Ok(meta) if meta.is_dir() => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Ok(_) => bail!(
            "Service trial parent is not a managed directory: {}",
            parents.display()
        ),
        Err(error) => return Err(error).context("Could not inspect the service trial parent"),
    }
    match fs::symlink_metadata(trial) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).context("Could not inspect the service trial directory"),
        Ok(_) => bail!(
            "Service trial path already exists; choose a fresh brewrs prefix to preview a new side-by-side trial"
        ),
    }
}

pub(super) fn managed_binary(
    prefix: &Path,
    name: &str,
    version: &str,
    bin: &str,
) -> Result<PathBuf> {
    let cellar = prefix.join("Cellar");
    let package = cellar.join(name);
    let keg = package.join(version);
    for path in [&cellar, &package, &keg, &keg.join("bin")] {
        if !fs::symlink_metadata(path)
            .with_context(|| format!("Could not inspect {}", path.display()))?
            .is_dir()
        {
            bail!("Not a real managed directory: {}", path.display());
        }
    }
    let path = keg.join("bin").join(bin);
    let metadata = fs::symlink_metadata(&path)
        .with_context(|| format!("Could not inspect {}", path.display()))?;
    if !metadata.is_file() {
        bail!(
            "Service requires a regular managed executable: {}",
            path.display()
        );
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o111 == 0 {
            bail!("Service binary is not executable: {}", path.display());
        }
    }
    Ok(path)
}

/// Resolve the declared executable to the pinned keg and verify every ancestor.
///
/// A `$HOMEBREW_PREFIX/opt/<name>` declaration is accepted only when the opt
/// entry is a real symlink whose target is exactly the pinned keg; the returned
/// path is always the immutable keg path.
pub(super) fn pinned_executable(
    prefix: &Path,
    name: &str,
    version: &str,
    value: &str,
) -> Result<(PathBuf, bool)> {
    let path = absolute_path(value, "run argument 0")?;
    let keg = prefix.join("Cellar").join(name).join(version);
    let (relative, used_opt_alias) = match path.strip_prefix(&keg) {
        Ok(relative) => (relative.to_path_buf(), false),
        Err(_) => {
            let opt = prefix.join("opt").join(name);
            let Ok(relative) = path.strip_prefix(&opt) else {
                bail!(
                    "Service executable is outside the pinned keg: {} (pinned keg {})",
                    path.display(),
                    keg.display()
                );
            };
            verify_opt_alias(prefix, name, version)?;
            (relative.to_path_buf(), true)
        }
    };
    if relative.as_os_str().is_empty() {
        bail!("Service executable is the pinned keg directory, not a file");
    }
    let executable = keg.join(&relative);
    verify_keg_ancestors(prefix, &executable)?;
    verify_executable(&executable)?;
    Ok((executable, used_opt_alias))
}

/// Verify the reviewed brewrs opt link without resolving it.
///
/// The raw target must be exactly `../Cellar/<name>/<version>`, the only link
/// format brewrs creates. Lexical normalization is deliberately not used: a
/// target such as `../Cellar/current/0.46.0` could point through an arbitrary
/// symlinked intermediate and still normalize to the keg. The keg itself is
/// then checked as a real, non-symlinked directory chain by
/// [`verify_keg_ancestors`].
pub(super) fn verify_opt_alias(prefix: &Path, name: &str, version: &str) -> Result<()> {
    let opt = prefix.join("opt");
    ensure_real_directory(&opt, "managed opt directory")?;
    let alias = opt.join(name);
    let metadata = fs::symlink_metadata(&alias)
        .with_context(|| format!("Could not inspect {}", alias.display()))?;
    if !metadata.file_type().is_symlink() {
        bail!(
            "Service executable uses '{}', but it is not a managed symlink",
            alias.display()
        );
    }
    let target = fs::read_link(&alias)
        .with_context(|| format!("Could not read the symlink {}", alias.display()))?;
    let expected = Path::new("..").join("Cellar").join(name).join(version);
    if target != expected {
        bail!(
            "Service executable uses the mutable opt alias '{}', but its target {} is not the exact pinned keg link {expected:?}",
            alias.display(),
            target.display()
        );
    }
    Ok(())
}

pub(super) fn verify_keg_ancestors(prefix: &Path, executable: &Path) -> Result<()> {
    let relative = executable.strip_prefix(prefix).with_context(|| {
        format!(
            "Service executable is outside the brewrs prefix: {}",
            executable.display()
        )
    })?;
    let mut current = prefix.to_path_buf();
    for component in relative.components() {
        let Component::Normal(part) = component else {
            bail!(
                "Service executable has an unsupported path component: {}",
                executable.display()
            );
        };
        current.push(part);
        if current == executable {
            break;
        }
        ensure_real_directory(&current, "service executable ancestor")?;
    }
    Ok(())
}

pub(super) fn verify_executable(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("Could not inspect {}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        bail!(
            "Service requires a regular executable file, not a symlink: {}",
            path.display()
        );
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o111 == 0 {
            bail!("Service binary is not executable: {}", path.display());
        }
    }
    Ok(())
}
