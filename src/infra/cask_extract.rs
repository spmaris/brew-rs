//! Safe format detection, preflighted extraction, and payload staging for casks.
//!
//! Supported formats:
//! * Naked binaries (Mach-O executables or `#!` scripts),
//! * ZIP archives (preflighted in pure Rust across Central Directory headers
//!   before invoking `/usr/bin/ditto -x -k` so Apple signatures and resource
//!   forks remain intact),
//! * `.tar.gz` archives (streamed through `flate2` + `tar` with strict path and
//!   deferred relative-symlink containment checks),
//! * `.tar.bz2` / `.tar.xz` archives (entry list preflighted before `/usr/bin/tar -xf`
//!   plus post-unpack containment walk),
//! * `.dmg` disk images (mounted read-only via `/usr/bin/hdiutil attach -mountrandom`
//!   with RAII `detach`, copying only declared artifact sources via `/usr/bin/ditto`).
//!
//! Every extracted tree is walked without following symlinks to reject special
//! files and escaping links before artifacts are staged for [`super::cask_store`].

mod assemble;
mod dmg_extract;
mod format;
mod limits;
mod model;
mod tar_extract;
mod tree;
mod wrapper;
mod zip_extract;

use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};

use self::assemble::assemble_stage;
use self::dmg_extract::extract_dmg;
use self::format::{detect_format, stage_naked_payload};
use self::model::PayloadFormat;
use self::tar_extract::{extract_tar_gz, preflight_bsdtar, run_bsdtar_extract};
use self::tree::validate_extracted_tree;
use self::zip_extract::{preflight_zip, run_ditto_extract};
use super::download::verify_file_digest;
use crate::domain::cask_plan::PlannedCask;

#[cfg(test)]
pub(crate) use model::{StagedAppBundle, StagedWrapper};
pub(crate) use model::{StagedCaskLink, StagedCaskPayload};

/// Stage a verified cask download inside `stage_dir` and prepare all declared
/// app bundles, Caskroom files, symlinks, and command wrappers.
pub(crate) fn stage(
    download_path: &Path,
    stage_dir: &Path,
    planned: &PlannedCask,
    prefix: &Path,
    appdir: &Path,
) -> Result<StagedCaskPayload> {
    verify_file_digest(download_path, &planned.sha256).with_context(|| {
        format!(
            "SHA-256 verification failed before staging cask {} {}",
            planned.token, planned.version
        )
    })?;

    let stage_meta = fs::symlink_metadata(stage_dir)
        .with_context(|| format!("failed to inspect stage '{}'", stage_dir.display()))?;
    if stage_meta.file_type().is_symlink() || !stage_meta.is_dir() {
        bail!(
            "cask stage '{}' is not a real directory",
            stage_dir.display()
        );
    }

    let format = detect_format(download_path, &planned.url, planned.container)?;
    let unpack_dir = stage_dir.join("unpack");
    fs::create_dir(&unpack_dir).with_context(|| {
        format!(
            "failed to create unpack directory '{}'",
            unpack_dir.display()
        )
    })?;

    match format {
        PayloadFormat::Naked => stage_naked_payload(download_path, &unpack_dir, planned)?,
        PayloadFormat::Zip => {
            preflight_zip(download_path)?;
            run_ditto_extract(download_path, &unpack_dir)?;
            validate_extracted_tree(&unpack_dir)?;
        }
        PayloadFormat::TarGz => {
            extract_tar_gz(download_path, &unpack_dir)?;
            validate_extracted_tree(&unpack_dir)?;
        }
        PayloadFormat::TarBz2 | PayloadFormat::TarXz => {
            preflight_bsdtar(download_path)?;
            run_bsdtar_extract(download_path, &unpack_dir)?;
            validate_extracted_tree(&unpack_dir)?;
        }
        PayloadFormat::Dmg => {
            extract_dmg(download_path, stage_dir, &unpack_dir, planned)?;
            validate_extracted_tree(&unpack_dir)?;
        }
    }

    assemble_stage(stage_dir, &unpack_dir, planned, prefix, appdir)
}

#[cfg(test)]
mod tests;
