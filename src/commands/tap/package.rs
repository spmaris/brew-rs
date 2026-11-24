use std::path::Path;

use anyhow::{Context, Result, bail};

use crate::commands::install::{confirm, prefix_path};
use crate::config::Config;
use crate::domain::tap_manifest::{TapReceipt, parse_qualified};
use crate::infra::store::{InstallLock, Store, reject_protected_prefix, stages_dir};
use crate::infra::tap_artifact::stage_verified;
use crate::ui::{DnfStyleFormatter, phrases};

use super::commit::{commit_install, commit_uninstall};
use super::fsguard::verify_ancestor_dirs;
use super::selection::{build_plan, render_plan};
use super::state::{RECEIPTS, TAPS, encode_json, read_optional_receipt};
use super::verify::{verify_installed_artifact, verify_removal_state};

/// Preview a fully qualified tap package without downloading anything.
pub(crate) fn plan(config: &Config, prefix: Option<&Path>, package: &str) -> Result<()> {
    let path = prefix_path(prefix)?;
    let plan = build_plan(&path, package)?;
    let ui = DnfStyleFormatter::new(config);
    render_plan(&ui, &plan, "Tap install plan");
    if plan.installed.is_some() {
        ui.note(&format!(
            "\nAlready installed from the tap. No changes; `brewrs tap install {package}` re-verifies the artifact bytes."
        ));
    } else {
        ui.note("\nNothing was downloaded or installed.");
    }
    ui.note(
        "This lane evaluates no Ruby or shell source and supports only explicit naked macOS executables.",
    );
    Ok(())
}

/// Install one fully qualified tap package after exact digest and Mach-O checks.
pub(crate) async fn install(
    config: &Config,
    prefix: Option<&Path>,
    package: &str,
    dry_run: bool,
    yes: bool,
) -> Result<()> {
    let path = prefix_path(prefix)?;
    reject_protected_prefix(&path)?;
    let plan = build_plan(&path, package)?;
    let ui = DnfStyleFormatter::new(config);
    render_plan(&ui, &plan, "Tap install");

    if let Some(receipt) = &plan.installed {
        let bytes = verify_installed_artifact(&path, receipt).with_context(|| {
            format!(
                "The installed tap package '{}' failed byte re-verification; run `brewrs tap uninstall {package}` and reinstall",
                plan.qualified
            )
        })?;
        if bytes != receipt.artifact_bytes {
            bail!(
                "Installed tap package '{}' has {} bytes, but its receipt records {}",
                plan.qualified,
                bytes,
                receipt.artifact_bytes
            );
        }
        ui.note("\nAlready installed; artifact bytes verified. No changes.");
        return Ok(());
    }

    if dry_run {
        ui.note(&format!("\n{}", phrases::dry_run_summary(1, 0)));
        return Ok(());
    }
    if config.general.confirm_operations && !yes && !confirm("Install 1 package?")? {
        eprintln!("{}", phrases::cancelled("packages"));
        return Ok(());
    }

    let cached =
        crate::infra::cask_download::fetch(config, &plan.artifact_url, &plan.artifact_sha256)
            .await
            .with_context(|| {
                format!(
                    "Could not fetch the pinned artifact for '{}' from {}",
                    plan.qualified, plan.artifact_url
                )
            })?;

    let _lock = InstallLock::acquire(&path)?;
    let store = Store::open_existing(&path)?;
    let current = build_plan(&path, package)?;
    if let Some(receipt) = &current.installed {
        let bytes = verify_installed_artifact(&path, receipt)?;
        if bytes != receipt.artifact_bytes {
            bail!(
                "Tap package '{}' was installed concurrently but its artifact size does not match its receipt",
                current.qualified
            );
        }
        ui.note("\nAlready installed by a concurrent brewrs process. No changes.");
        return Ok(());
    }
    if !current.same_selection(&plan) {
        bail!(
            "Tap state or the selected artifact changed while the download was in progress; no changes were made"
        );
    }

    let stage = tempfile::Builder::new()
        .prefix("tap-install-")
        .tempdir_in(stages_dir(store.prefix()))
        .context("failed to create a private tap staging directory")?;
    let staged = stage.path().join(&current.bin);
    let artifact_bytes = stage_verified(
        &cached,
        &staged,
        &current.artifact_sha256,
        current.architecture,
    )
    .with_context(|| format!("Failed to verify the artifact for '{}'", current.qualified))?;

    let receipt = TapReceipt::new(
        &current.tap,
        &current.token,
        &current.version,
        &current.bin,
        current.architecture,
        &current.artifact_url,
        &current.artifact_sha256,
        artifact_bytes,
        &current.manifest_sha256,
        &current.source_manifest,
    )?;
    let receipt_bytes = encode_json(&receipt)?;
    commit_install(&store, &current, &staged, &receipt_bytes)
        .with_context(|| format!("Failed to install '{}'", current.qualified))?;
    drop(stage);

    ui.note(&format!(
        "\n{} {}.",
        ui.success("Installed"),
        current.qualified
    ));
    ui.note(&ui.field(
        "Bin",
        &store.prefix().join(&current.link).display().to_string(),
    ));
    ui.note(&ui.field("Receipt", &current.receipt_path.display().to_string()));
    ui.note(&format!(
        "The artifact was SHA-256 verified and checked as a {} Mach-O executable. Only the executable was installed.",
        current.architecture
    ));
    Ok(())
}

/// Remove one installed tap package under the shared writer lock.
pub(crate) fn uninstall(
    config: &Config,
    prefix: Option<&Path>,
    package: &str,
    dry_run: bool,
    yes: bool,
) -> Result<()> {
    let path = prefix_path(prefix)?;
    Store::open_existing(&path)
        .context("Uninstalling a tap package requires an initialized brewrs prefix")?;
    let target = parse_qualified(package)?;
    let tap = target.tap();
    let receipt_rel = Path::new(".brewrs")
        .join("state")
        .join(TAPS)
        .join(target.owner)
        .join(target.repo)
        .join(RECEIPTS)
        .join(format!("{}.json", target.token));
    verify_ancestor_dirs(&path, &receipt_rel, "tap receipt")?;
    let receipt_path = path.join(&receipt_rel);
    let receipt = read_optional_receipt(&receipt_path)?.with_context(|| {
        format!(
            "tap package '{package}' is not installed in '{}'",
            path.display()
        )
    })?;
    if receipt.tap != tap || receipt.token != target.token || receipt.qualified != package {
        bail!(
            "tap receipt '{}' does not describe '{package}'",
            receipt_path.display()
        );
    }
    verify_removal_state(&path, &receipt)?;

    let ui = DnfStyleFormatter::new(config);
    ui.note(&format!(
        "{}  {}",
        ui.section("Tap uninstall"),
        ui.info(package)
    ));
    for (label, value) in [
        ("Version", receipt.version.clone()),
        ("Architecture", receipt.architecture.clone()),
        ("Bin", path.join(&receipt.link).display().to_string()),
        ("Keg", path.join(&receipt.keg).display().to_string()),
        ("Receipt", receipt_path.display().to_string()),
    ] {
        ui.note(&ui.field(label, &value));
    }

    if dry_run {
        ui.note(&format!("\n{}", phrases::dry_run_summary(1, 0)));
        return Ok(());
    }
    if config.general.confirm_operations && !yes && !confirm("Remove 1 package?")? {
        eprintln!("{}", phrases::cancelled("packages"));
        return Ok(());
    }

    let _lock = InstallLock::acquire(&path)?;
    let store = Store::open_existing(&path)?;
    let current = read_optional_receipt(&receipt_path)?.with_context(|| {
        format!("tap package '{package}' disappeared before it could be removed")
    })?;
    if current != receipt {
        bail!("Tap receipt for '{package}' changed before removal; no changes were made");
    }
    verify_removal_state(store.prefix(), &current)?;
    commit_uninstall(store.prefix(), &current)?;
    ui.note(&format!("\n{} {package}.", ui.success("Uninstalled")));
    Ok(())
}
