//! Assembly of the declared artifacts from an unpacked tree into the stage.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};

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
) -> Result<StagedCaskPayload> {
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
            fs::create_dir_all(&apps_stage)?;
            let src = resolve_unpack_dir_path(unpack_dir, source)?;
            let meta = fs::symlink_metadata(&src).with_context(|| {
                format!(
                    "cask '{}' declared app '{}' which was not found in the archive",
                    planned.token, source
                )
            })?;
            if meta.file_type().is_symlink() || !meta.is_dir() {
                bail!(
                    "cask '{}' app '{source}' is not a regular .app directory",
                    planned.token
                );
            }
            if !src.join("Contents").is_dir() {
                bail!(
                    "cask '{}' app '{source}' is missing a Contents directory",
                    planned.token
                );
            }
            let dst = apps_stage.join(bundle_name);
            fs::rename(&src, &dst).with_context(|| {
                format!(
                    "failed to stage app bundle '{}' into '{}'",
                    bundle_name,
                    dst.display()
                )
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
                fs::create_dir_all(&wrappers_stage)?;
                let script = build_command_wrapper_script(&exec_abs, args);
                let wrapper_path = wrappers_stage.join(link_name);
                fs::write(&wrapper_path, script.as_bytes()).with_context(|| {
                    format!(
                        "failed to write command wrapper '{}'",
                        wrapper_path.display()
                    )
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
) -> Result<String> {
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
            let bundle_dir = staged_apps
                .get(bundle_name)
                .with_context(|| format!("referenced app bundle '{bundle_name}' was not staged"))?;
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
) -> Result<PathBuf> {
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
            let bundle_dir = staged_apps
                .get(bundle_name)
                .with_context(|| format!("referenced app bundle '{bundle_name}' was not staged"))?;
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
) -> Result<()> {
    if copied_payload_paths.contains_key(rel) {
        if chmod_exec {
            set_executable(&caskroom_stage.join(rel))?;
        }
        return Ok(());
    }
    let src = resolve_unpack_dir_path(unpack_dir, rel)?;
    let meta = fs::symlink_metadata(&src)
        .with_context(|| format!("missing declared payload artifact '{rel}'"))?;
    if meta.file_type().is_symlink() || !meta.is_file() {
        bail!("declared payload artifact '{rel}' is not a regular file");
    }
    let dst = caskroom_stage.join(rel);
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::rename(&src, &dst)
        .or_else(|_| fs::copy(&src, &dst).map(|_| ()))
        .with_context(|| format!("failed to stage Caskroom file '{rel}'"))?;
    if chmod_exec {
        set_executable(&dst)?;
    }
    copied_payload_paths.insert(rel.to_string(), rel.to_string());
    caskroom_files.push(rel.to_string());
    Ok(())
}

fn resolve_unpack_dir_path(unpack_dir: &Path, rel: &str) -> Result<PathBuf> {
    let mut cur = unpack_dir.to_path_buf();
    for segment in rel.split('/') {
        cur.push(segment);
        let meta = fs::symlink_metadata(&cur)
            .with_context(|| format!("extracted artifact '{rel}' not found in archive"))?;
        if meta.file_type().is_symlink() {
            bail!("refusing symlink along declared artifact path '{rel}'");
        }
    }
    Ok(cur)
}

/// Resolve `inner_path` inside `bundle_dir` without escaping `bundle_dir`,
/// allowing bounded relative symlinks within the bundle.
fn verify_regular_file_in_bundle(bundle_dir: &Path, inner_path: &str) -> Result<()> {
    let mut current_rel = PathBuf::new();
    let mut remaining: Vec<String> = inner_path.split('/').map(str::to_string).collect();
    remaining.reverse();
    let mut hops = 0_usize;

    while let Some(seg) = remaining.pop() {
        match seg.as_str() {
            "" | "." => continue,
            ".." => {
                if !current_rel.pop() {
                    bail!(
                        "path '{inner_path}' escapes bundle '{}'",
                        bundle_dir.display()
                    );
                }
                continue;
            }
            _ => {}
        }
        current_rel.push(&seg);
        let abs = bundle_dir.join(&current_rel);
        let meta = fs::symlink_metadata(&abs).with_context(|| {
            format!(
                "missing file '{inner_path}' inside bundle '{}'",
                bundle_dir.display()
            )
        })?;
        if meta.file_type().is_symlink() {
            hops += 1;
            if hops > MAX_SYMLINK_HOPS {
                bail!("symlink loop resolving '{inner_path}' in bundle");
            }
            let target = fs::read_link(&abs)?;
            let target_str = target
                .to_str()
                .context("non-UTF-8 symlink inside .app bundle")?;
            validate_relative_symlink_lexically(&current_rel, target_str)?;
            current_rel.pop();
            let mut replacement: Vec<String> = target_str.split('/').map(str::to_string).collect();
            replacement.reverse();
            remaining.extend(replacement);
        } else if !remaining.is_empty() && !meta.is_dir() {
            bail!(
                "non-directory component along '{inner_path}' in '{}'",
                bundle_dir.display()
            );
        }
    }

    let final_meta = fs::symlink_metadata(bundle_dir.join(&current_rel)).with_context(|| {
        format!(
            "missing target '{inner_path}' in bundle '{}'",
            bundle_dir.display()
        )
    })?;
    if !final_meta.is_file() {
        bail!(
            "target '{inner_path}' inside '{}' is not a regular file",
            bundle_dir.display()
        );
    }
    Ok(())
}
