//! Fetching and staging of reviewed portable upstream releases.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::domain::plan::PlannedFormula;
use crate::domain::portable::{PortableError, artifact_for};
use crate::infra::archive::ArchiveError;
use crate::infra::cask_download::CaskDownloadError;
use crate::infra::tap_artifact::TapArtifactError;
use crate::infra::{archive, cask_download, tap_artifact};

/// Why a portable upstream release could not be fetched or staged.
#[derive(Debug, thiserror::Error)]
pub(crate) enum PortableFetchError {
    #[error(transparent)]
    Catalog(#[from] PortableError),
    #[error(transparent)]
    Download(#[from] CaskDownloadError),
    #[error(transparent)]
    Archive(#[from] ArchiveError),
    #[error(transparent)]
    Artifact(#[from] TapArtifactError),
    #[error("failed to {action} '{}'", path.display())]
    Io {
        action: &'static str,
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("Portable archive member {relative:?} is not executable")]
    NotExecutable { relative: String },
    #[error(
        "Portable archive member {relative:?} must be a regular file with real directory ancestors"
    )]
    NotRegularMember { relative: String },
}

impl PortableFetchError {
    /// A `map_err` adapter that names the action and path of a failed I/O call.
    fn io(action: &'static str, path: &Path) -> impl FnOnce(io::Error) -> Self {
        let path = path.to_path_buf();
        move |source| Self::Io {
            action,
            path,
            source,
        }
    }
}

pub(crate) async fn fetch(
    config: &crate::config::Config,
    formula: &PlannedFormula,
) -> Result<PathBuf, PortableFetchError> {
    let (_, _, artifact) = artifact_for(formula)?;
    Ok(cask_download::fetch(config, &artifact.url, &artifact.sha256).await?)
}

pub(crate) fn stage(
    formula: &PlannedFormula,
    tarball: &Path,
    out: &Path,
) -> Result<(), PortableFetchError> {
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
            if fs::symlink_metadata(&path)
                .map_err(PortableFetchError::io("inspect", &path))?
                .permissions()
                .mode()
                & 0o111
                == 0
            {
                return Err(PortableFetchError::NotExecutable {
                    relative: relative.to_string(),
                });
            }
        }
        tap_artifact::validate_macho(&path, arch)?;
    }
    Ok(())
}

fn regular_member(out: &Path, relative: &str) -> Result<PathBuf, PortableFetchError> {
    let path = out.join(relative);
    // The private stage is canonicalized independently so callers may use a
    // prefix behind macOS's /var or /tmp aliases, but members cannot be aliases.
    if !fs::symlink_metadata(&path)
        .map_err(PortableFetchError::io("inspect", &path))?
        .is_file()
        || path
            .canonicalize()
            .map_err(PortableFetchError::io("resolve", &path))?
            != out
                .canonicalize()
                .map_err(PortableFetchError::io("resolve", out))?
                .join(relative)
    {
        return Err(PortableFetchError::NotRegularMember {
            relative: relative.to_owned(),
        });
    }
    Ok(path)
}
