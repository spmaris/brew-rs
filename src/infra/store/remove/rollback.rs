use std::fs;
use std::io;
use std::path::Path;

use anyhow::{Context, Result, bail};

use crate::infra::store::{AppliedChange, MovedPath, RemovalStep, StepKind};

/// Move one preflighted path into the quarantine, rechecking its exact type.
///
/// Renaming a symlink moves the link itself, so this never follows a foreign
/// target. A path that changed since preflight aborts the whole removal.
pub(crate) fn move_aside(
    step: &RemovalStep,
    quarantine: &Path,
    index: usize,
    applied: &mut Vec<AppliedChange>,
) -> Result<()> {
    let source = &step.source;
    let metadata = match fs::symlink_metadata(source) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => bail!(
            "refusing to remove '{}': it disappeared after preflight",
            source.display()
        ),
        Err(error) => {
            return Err(error).with_context(|| format!("failed to inspect '{}'", source.display()));
        }
    };
    match &step.kind {
        StepKind::Receipt if metadata.file_type().is_symlink() || !metadata.is_file() => bail!(
            "refusing to remove '{}': it is no longer a regular receipt",
            source.display()
        ),
        StepKind::Directory if metadata.file_type().is_symlink() || !metadata.is_dir() => bail!(
            "refusing to remove '{}': it is no longer a real directory",
            source.display()
        ),
        StepKind::Link { target } => {
            if !metadata.file_type().is_symlink() {
                bail!(
                    "refusing to remove '{}': it is no longer a brewrs symlink",
                    source.display()
                );
            }
            let actual = fs::read_link(source)
                .with_context(|| format!("failed to read link '{}'", source.display()))?;
            if actual.as_os_str() != target.as_os_str() {
                bail!(
                    "refusing to remove '{}': the link target is now '{}'",
                    source.display(),
                    actual.display()
                );
            }
        }
        _ => {}
    }

    let parked = quarantine.join(format!("item-{index}"));
    fs::rename(source, &parked).with_context(|| {
        format!(
            "failed to move '{}' into the removal quarantine",
            source.display()
        )
    })?;
    applied.push(AppliedChange::Moved(MovedPath {
        source: source.clone(),
        parked,
    }));
    Ok(())
}

/// Remove the package directory, but only while it is empty.
///
/// A non-recursive removal cannot delete a foreign entry that appeared after
/// preflight: it fails with `DirectoryNotEmpty` and the directory is kept.
pub(super) fn prune_package_dir(
    step: &RemovalStep,
    applied: &mut Vec<AppliedChange>,
) -> Result<()> {
    let directory = &step.source;
    match fs::remove_dir(directory) {
        Ok(()) => {
            applied.push(AppliedChange::Pruned(directory.clone()));
            Ok(())
        }
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::DirectoryNotEmpty
            ) =>
        {
            Ok(())
        }
        Err(error) => Err(error).with_context(|| {
            format!(
                "failed to remove the emptied package directory '{}'",
                directory.display()
            )
        }),
    }
}

/// Undo every applied removal, newest change first.
pub(crate) fn rollback_removal(applied: &[AppliedChange]) -> Vec<String> {
    let mut issues = Vec::new();
    for change in applied.iter().rev() {
        match change {
            AppliedChange::Pruned(directory) => match fs::create_dir(directory) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => issues.push(format!(
                    "failed to recreate '{}': {error}",
                    directory.display()
                )),
            },
            AppliedChange::Moved(moved) => {
                match fs::symlink_metadata(&moved.source) {
                    Ok(_) => {
                        issues.push(format!(
                            "left '{}' in quarantine: the path reappeared",
                            moved.source.display()
                        ));
                        continue;
                    }
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Err(error) => {
                        issues.push(format!(
                            "failed to inspect '{}': {error}",
                            moved.source.display()
                        ));
                        continue;
                    }
                }
                if let Err(error) = fs::rename(&moved.parked, &moved.source) {
                    issues.push(format!(
                        "failed to restore '{}': {error}",
                        moved.source.display()
                    ));
                }
            }
            AppliedChange::RetainedReceipt(moved) => {
                // The new receipt is removed before this runs, so the active
                // slot is expected to be free; anything else is never
                // overwritten and the retained record is left in place.
                match fs::symlink_metadata(&moved.source) {
                    Ok(_) => {
                        issues.push(format!(
                            "left the retained record '{}' in place: '{}' still exists",
                            moved.parked.display(),
                            moved.source.display()
                        ));
                        continue;
                    }
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Err(error) => {
                        issues.push(format!(
                            "failed to inspect '{}': {error}",
                            moved.source.display()
                        ));
                        continue;
                    }
                }
                if let Err(error) = fs::rename(&moved.parked, &moved.source) {
                    issues.push(format!(
                        "failed to restore the retained receipt '{}': {error}",
                        moved.source.display()
                    ));
                }
            }
        }
    }
    issues
}
