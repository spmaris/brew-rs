//! Fetching and staging of reviewed portable upstream releases.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Result, bail};

use crate::domain::plan::PlannedFormula;
use crate::domain::portable::artifact_for;
use crate::infra::{archive, cask_download, tap_artifact};

pub(crate) async fn fetch(
    config: &crate::config::Config,
    formula: &PlannedFormula,
) -> Result<PathBuf> {
    let (_, _, artifact) = artifact_for(formula)?;
    cask_download::fetch(config, &artifact.url, &artifact.sha256).await
}

pub(crate) fn stage(formula: &PlannedFormula, tarball: &Path, out: &Path) -> Result<()> {
    let (package, arch, artifact) = artifact_for(formula)?;
    archive::unpack_release(tarball, out, &artifact.root, &artifact.sha256)?;
    for relative in &package.required_files {
        regular_member(out, relative)?;
    }
    for relative in &package.executables {
        let path = regular_member(out, relative)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if fs::symlink_metadata(&path)?.permissions().mode() & 0o111 == 0 {
                bail!("Portable archive member {relative:?} is not executable");
            }
        }
        tap_artifact::validate_macho(&path, arch)?;
    }
    Ok(())
}

fn regular_member(out: &Path, relative: &str) -> Result<PathBuf> {
    let path = out.join(relative);
    // The private stage is canonicalized independently so callers may use a
    // prefix behind macOS's /var or /tmp aliases, but members cannot be aliases.
    if !fs::symlink_metadata(&path)?.is_file()
        || path.canonicalize()? != out.canonicalize()?.join(relative)
    {
        bail!(
            "Portable archive member {relative:?} must be a regular file with real directory ancestors"
        );
    }
    Ok(path)
}
