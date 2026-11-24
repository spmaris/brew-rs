//! Assembly of the declared artifacts from an unpacked tree into the stage.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use super::error::CaskExtractError;
use super::model::{StagedAppBundle, StagedCaskLink, StagedCaskPayload, StagedWrapper};
use super::tree::validate_relative_symlink_lexically;
use super::wrapper::{build_command_wrapper_script, set_executable};
use crate::domain::cask_plan::{CaskSourcePath, PlannedCask, PlannedCaskArtifact};

/// Maximum symlink hops resolved inside a staged `.app` bundle.
const MAX_SYMLINK_HOPS: usize = 8;

/// Arrange unpacked artifacts into `<stage>/apps`, `<stage>/caskroom`, and
/// `<stage>/wrappers`, returning metadata needed by `cask_store::commit`.
pub(super) fn assemble_stage(
    stage_dir: &Path,
    unpack_dir: &Path,
    planned: &PlannedCask,
    prefix: &Path,
    appdir: &Path,
) -> Result<StagedCaskPayload, CaskExtractError> {
    let apps_stage = stage_dir.join("apps");
    let caskroom_stage = stage_dir.join("caskroom");
    let wrappers_stage = stage_dir.join("wrappers");

    let mut app_bundles = Vec::new();
    let mut staged_apps_by_name: HashMap<String, PathBuf> = HashMap::new();

    for artifact in &planned.artifacts {
        if let PlannedCaskArtifact::App {
            source,
            bundle_name,
        } = artifact
        {
            fs::create_dir_all(&apps_stage).map_err(CaskExtractError::Fs)?;
            let src = resolve_unpack_dir_path(unpack_dir, source)?;
            let meta =
                fs::symlink_metadata(&src).map_err(|error| CaskExtractError::AppNotFound {
                    token: planned.token.clone(),
                    app: source.clone(),
                    source: error,
                })?;
            if meta.file_type().is_symlink() || !meta.is_dir() {
                return Err(CaskExtractError::AppNotBundle {
                    token: planned.token.clone(),
                    app: source.clone(),
                });
            }
            if !src.join("Contents").is_dir() {
                return Err(CaskExtractError::AppMissingContents {
                    token: planned.token.clone(),
                    app: source.clone(),
                });
            }
            let dst = apps_stage.join(bundle_name);
            fs::rename(&src, &dst).map_err(|error| CaskExtractError::StageApp {
                bundle_name: bundle_name.clone(),
                dest: dst.clone(),
                source: error,
            })?;
            staged_apps_by_name.insert(bundle_name.clone(), dst.clone());
            app_bundles.push(StagedAppBundle {
                bundle_name: bundle_name.clone(),
                staged_dir: dst,
            });
        }
    }

    let mut caskroom_files = Vec::new();
    let mut copied_payload_paths: HashMap<String, String> = HashMap::new();
    let mut symlinks = Vec::new();
    let mut wrappers = Vec::new();

    for artifact in &planned.artifacts {
        match artifact {
            PlannedCaskArtifact::App { .. } => {}
            PlannedCaskArtifact::Binary { source, link_name } => {
                let target = stage_source_and_compute_target(
                    source,
                    unpack_dir,
                    &caskroom_stage,
                    &staged_apps_by_name,
                    prefix,
                    appdir,
                    &planned.token,
                    &planned.version,
                    true,
                    &mut caskroom_files,
                    &mut copied_payload_paths,
                    "../",
                )?;
                symlinks.push(StagedCaskLink {
                    relative_dest: format!("bin/{link_name}"),
                    target,
                });
            }
            PlannedCaskArtifact::PrefixLink {
                source,
                relative_dest,
            } => {
                let depth = relative_dest.matches('/').count();
                let up = "../".repeat(depth);
                let target = stage_source_and_compute_target(
                    source,
                    unpack_dir,
                    &caskroom_stage,
                    &staged_apps_by_name,
                    prefix,
                    appdir,
                    &planned.token,
                    &planned.version,
                    false,
                    &mut caskroom_files,
                    &mut copied_payload_paths,
                    &up,
                )?;
                symlinks.push(StagedCaskLink {
                    relative_dest: relative_dest.clone(),
                    target,
                });
            }
            PlannedCaskArtifact::CommandWrapper {
                executable,
                link_name,
                args,
            } => {
                let exec_abs = stage_source_for_wrapper_exec(
                    executable,
                    unpack_dir,
                    &caskroom_stage,
                    &staged_apps_by_name,
                    prefix,
                    appdir,
                    &planned.token,
                    &planned.version,
                    &mut caskroom_files,
                    &mut copied_payload_paths,
                )?;
                fs::create_dir_all(&wrappers_stage).map_err(CaskExtractError::Fs)?;
                let script = build_command_wrapper_script(&exec_abs, args);
                let wrapper_path = wrappers_stage.join(link_name);
                fs::write(&wrapper_path, script.as_bytes()).map_err(|source| {
                    CaskExtractError::Io {
                        action: "write command wrapper",
                        path: wrapper_path.clone(),
                        source,
                    }
                })?;
                set_executable(&wrapper_path)?;
                let digest = hex::encode(Sha256::digest(script.as_bytes()));
                wrappers.push(StagedWrapper {
                    relative_dest: format!("bin/{link_name}"),
                    staged_file: wrapper_path,
                    sha256: digest,
                });
            }
        }
    }

    caskroom_files.sort_unstable();
    let has_caskroom_files = !caskroom_files.is_empty();
    let _ = fs::remove_dir_all(unpack_dir);

    Ok(StagedCaskPayload {
        app_bundles,
        has_caskroom_files,
        caskroom_files,
        symlinks,
        wrappers,
    })
}

#[allow(clippy::too_many_arguments)]
fn stage_source_and_compute_target(
    source: &CaskSourcePath,
    unpack_dir: &Path,
    caskroom_stage: &Path,
    staged_apps: &HashMap<String, PathBuf>,
    prefix: &Path,
    appdir: &Path,
    token: &str,
    version: &str,
    chmod_exec: bool,
    caskroom_files: &mut Vec<String>,
    copied_payload_paths: &mut HashMap<String, String>,
    relative_up_to_prefix: &str,
) -> Result<String, CaskExtractError> {
    match source {
        CaskSourcePath::Payload(rel) => {
            ensure_payload_file_staged(
                unpack_dir,
                caskroom_stage,
                rel,
                chmod_exec,
                caskroom_files,
                copied_payload_paths,
            )?;
            Ok(format!(
                "{relative_up_to_prefix}Caskroom/{token}/{version}/{rel}"
            ))
        }
        CaskSourcePath::AppBundle {
            bundle_name,
            inner_path,
        } => {
            let bundle_dir =
                staged_apps
                    .get(bundle_name)
                    .ok_or_else(|| CaskExtractError::BundleNotStaged {
                        bundle_name: bundle_name.clone(),
                    })?;
            verify_regular_file_in_bundle(bundle_dir, inner_path)?;
            if appdir == prefix.join("Applications") {
                Ok(format!(
                    "{relative_up_to_prefix}Applications/{bundle_name}/{inner_path}"
                ))
            } else {
                Ok(appdir
                    .join(bundle_name)
                    .join(inner_path)
                    .to_string_lossy()
                    .into_owned())
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn stage_source_for_wrapper_exec(
    executable: &CaskSourcePath,
    unpack_dir: &Path,
    caskroom_stage: &Path,
    staged_apps: &HashMap<String, PathBuf>,
    prefix: &Path,
    appdir: &Path,
    token: &str,
    version: &str,
    caskroom_files: &mut Vec<String>,
    copied_payload_paths: &mut HashMap<String, String>,
) -> Result<PathBuf, CaskExtractError> {
    match executable {
        CaskSourcePath::Payload(rel) => {
            ensure_payload_file_staged(
                unpack_dir,
                caskroom_stage,
                rel,
                true,
                caskroom_files,
                copied_payload_paths,
            )?;
            Ok(prefix.join("Caskroom").join(token).join(version).join(rel))
        }
        CaskSourcePath::AppBundle {
            bundle_name,
            inner_path,
        } => {
            let bundle_dir =
                staged_apps
                    .get(bundle_name)
                    .ok_or_else(|| CaskExtractError::BundleNotStaged {
                        bundle_name: bundle_name.clone(),
                    })?;
            verify_regular_file_in_bundle(bundle_dir, inner_path)?;
            Ok(appdir.join(bundle_name).join(inner_path))
        }
    }
}

fn ensure_payload_file_staged(
    unpack_dir: &Path,
    caskroom_stage: &Path,
    rel: &str,
    chmod_exec: bool,
    caskroom_files: &mut Vec<String>,
    copied_payload_paths: &mut HashMap<String, String>,
) -> Result<(), CaskExtractError> {
    if copied_payload_paths.contains_key(rel) {
        if chmod_exec {
            set_executable(&caskroom_stage.join(rel))?;
        }
        return Ok(());
    }
    let src = resolve_unpack_dir_path(unpack_dir, rel)?;
    let meta = fs::symlink_metadata(&src).map_err(|source| CaskExtractError::PayloadMissing {
        rel: rel.to_owned(),
        source,
    })?;
    if meta.file_type().is_symlink() || !meta.is_file() {
        return Err(CaskExtractError::PayloadNotRegular {
            rel: rel.to_owned(),
        });
    }
    let dst = caskroom_stage.join(rel);
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent).map_err(CaskExtractError::Fs)?;
    }
    fs::rename(&src, &dst)
        .or_else(|_| fs::copy(&src, &dst).map(|_| ()))
        .map_err(|source| CaskExtractError::Io {
            action: "stage Caskroom file",
            path: PathBuf::from(rel),
            source,
        })?;
    if chmod_exec {
        set_executable(&dst)?;
    }
    copied_payload_paths.insert(rel.to_string(), rel.to_string());
    caskroom_files.push(rel.to_string());
    Ok(())
}

fn resolve_unpack_dir_path(unpack_dir: &Path, rel: &str) -> Result<PathBuf, CaskExtractError> {
    let mut cur = unpack_dir.to_path_buf();
    for segment in rel.split('/') {
        cur.push(segment);
        let meta =
            fs::symlink_metadata(&cur).map_err(|source| CaskExtractError::ArtifactNotFound {
                rel: rel.to_owned(),
                source,
            })?;
        if meta.file_type().is_symlink() {
            return Err(CaskExtractError::ArtifactSymlink {
                rel: rel.to_owned(),
            });
        }
    }
    Ok(cur)
}

/// Resolve `inner_path` inside `bundle_dir` without escaping `bundle_dir`,
/// allowing bounded relative symlinks within the bundle.
fn verify_regular_file_in_bundle(
    bundle_dir: &Path,
    inner_path: &str,
) -> Result<(), CaskExtractError> {
    let mut current_rel = PathBuf::new();
    let mut remaining: Vec<String> = inner_path.split('/').map(str::to_string).collect();
    remaining.reverse();
    let mut hops = 0_usize;

    while let Some(seg) = remaining.pop() {
        match seg.as_str() {
            "" | "." => continue,
            ".." => {
                if !current_rel.pop() {
                    return Err(CaskExtractError::BundleEscape {
                        inner_path: inner_path.to_owned(),
                        bundle: bundle_dir.to_path_buf(),
                    });
                }
                continue;
            }
            _ => {}
        }
        current_rel.push(&seg);
        let abs = bundle_dir.join(&current_rel);
        let meta =
            fs::symlink_metadata(&abs).map_err(|source| CaskExtractError::BundleFileMissing {
                inner_path: inner_path.to_owned(),
                bundle: bundle_dir.to_path_buf(),
                source,
            })?;
        if meta.file_type().is_symlink() {
            hops += 1;
            if hops > MAX_SYMLINK_HOPS {
                return Err(CaskExtractError::BundleSymlinkLoop {
                    inner_path: inner_path.to_owned(),
                });
            }
            let target = fs::read_link(&abs).map_err(CaskExtractError::Fs)?;
            let target_str = target
                .to_str()
                .ok_or(CaskExtractError::BundleSymlinkNotUtf8)?;
            validate_relative_symlink_lexically(&current_rel, target_str)?;
            current_rel.pop();
            let mut replacement: Vec<String> = target_str.split('/').map(str::to_string).collect();
            replacement.reverse();
            remaining.extend(replacement);
        } else if !remaining.is_empty() && !meta.is_dir() {
            return Err(CaskExtractError::BundleNonDirectory {
                inner_path: inner_path.to_owned(),
                bundle: bundle_dir.to_path_buf(),
            });
        }
    }

    let final_meta = fs::symlink_metadata(bundle_dir.join(&current_rel)).map_err(|source| {
        CaskExtractError::BundleTargetMissing {
            inner_path: inner_path.to_owned(),
            bundle: bundle_dir.to_path_buf(),
            source,
        }
    })?;
    if !final_meta.is_file() {
        return Err(CaskExtractError::BundleTargetNotRegular {
            inner_path: inner_path.to_owned(),
            bundle: bundle_dir.to_path_buf(),
        });
    }
    Ok(())
}
