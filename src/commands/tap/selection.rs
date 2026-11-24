use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::domain::tap_manifest::{TAPROOM, TapReceipt, host_arch, parse_qualified};
use crate::infra::store::Store;
use crate::ui::DnfStyleFormatter;

use super::fsguard::describe_path;
use super::state::{TappedTap, all_tapped, read_optional_receipt, read_receipts, read_tapped};
use super::verify::verify_installed_state;

/// One fully resolved tap install selection.
#[derive(Debug)]
pub(super) struct TapPlan {
    pub(super) prefix: PathBuf,
    pub(super) qualified: String,
    pub(super) tap: String,
    pub(super) owner: String,
    pub(super) repo: String,
    pub(super) token: String,
    pub(super) version: String,
    pub(super) bin: String,
    pub(super) architecture: &'static str,
    pub(super) artifact_url: String,
    pub(super) artifact_sha256: String,
    pub(super) manifest_sha256: String,
    pub(super) source_manifest: PathBuf,
    pub(super) keg: String,
    pub(super) link: String,
    pub(super) receipt_path: PathBuf,
    pub(super) installed: Option<TapReceipt>,
}

impl TapPlan {
    pub(super) fn same_selection(&self, other: &Self) -> bool {
        self.prefix == other.prefix
            && self.qualified == other.qualified
            && self.tap == other.tap
            && self.owner == other.owner
            && self.repo == other.repo
            && self.token == other.token
            && self.version == other.version
            && self.bin == other.bin
            && self.architecture == other.architecture
            && self.artifact_url == other.artifact_url
            && self.artifact_sha256 == other.artifact_sha256
            && self.manifest_sha256 == other.manifest_sha256
            && self.source_manifest == other.source_manifest
            && self.keg == other.keg
            && self.link == other.link
            && self.receipt_path == other.receipt_path
    }
}

pub(super) fn build_plan(prefix: &Path, package: &str) -> Result<TapPlan> {
    let store = Store::open_existing(prefix)?;
    let target = parse_qualified(package)?;
    let tap = target.tap();
    let tapped = read_tapped(store.prefix(), &tap)?;
    let declared = tapped.manifest.package(target.token)?;
    let architecture = host_arch()?;
    let artifact = declared.artifact_for(architecture)?;

    let keg = format!(
        "{TAPROOM}/{}/{}/{}/{}",
        target.owner, target.repo, target.token, declared.version
    );
    let link = format!("bin/{}", declared.bin);
    let receipt_path = tapped.receipts.join(format!("{}.json", declared.token));
    let installed = read_optional_receipt(&receipt_path)?;
    if let Some(receipt) = &installed {
        validate_receipt_binding(
            receipt,
            &target,
            &tapped,
            declared,
            artifact,
            architecture,
            &keg,
            &link,
        )?;
        verify_installed_state(store.prefix(), receipt).with_context(|| {
            format!(
                "The installed tap package '{package}' is incomplete; run `brewrs tap uninstall {package}` and reinstall"
            )
        })?;
    } else {
        check_collisions(&store, &tapped, declared, &keg, &link)?;
    }

    Ok(TapPlan {
        prefix: store.prefix().to_path_buf(),
        qualified: package.to_string(),
        tap,
        owner: target.owner.to_string(),
        repo: target.repo.to_string(),
        token: target.token.to_string(),
        version: declared.version.clone(),
        bin: declared.bin.clone(),
        architecture,
        artifact_url: artifact.url.clone(),
        artifact_sha256: artifact.sha256.clone(),
        manifest_sha256: tapped.manifest_sha256().to_string(),
        source_manifest: PathBuf::from(&tapped.record.source),
        keg,
        link,
        receipt_path,
        installed,
    })
}

#[allow(clippy::too_many_arguments)]
pub(super) fn validate_receipt_binding(
    receipt: &TapReceipt,
    target: &crate::domain::tap_manifest::QualifiedTarget<'_>,
    tapped: &TappedTap,
    declared: &crate::domain::tap_manifest::ManifestPackage,
    artifact: &crate::domain::tap_manifest::Artifact,
    architecture: &str,
    keg: &str,
    link: &str,
) -> Result<()> {
    receipt.validate()?;
    let matches = receipt.tap == tapped.record.tap
        && receipt.token == target.token
        && receipt.qualified == format!("{}/{}", tapped.record.tap, target.token)
        && receipt.version == declared.version
        && receipt.bin == declared.bin
        && receipt.architecture == architecture
        && receipt.artifact_url == artifact.url
        && receipt.artifact_sha256 == artifact.sha256
        && receipt.manifest_sha256 == tapped.manifest_sha256()
        && receipt.source_manifest == tapped.record.source
        && receipt.keg == keg
        && receipt.link == link;
    if !matches {
        bail!(
            "The installed tap receipt for '{}/{}' is not bound to the current manifest and selected artifact; run `brewrs tap uninstall {}/{}/{}` and reinstall",
            target.owner,
            target.repo,
            target.owner,
            target.repo,
            target.token
        );
    }
    Ok(())
}

pub(super) fn check_collisions(
    store: &Store,
    tapped: &TappedTap,
    declared: &crate::domain::tap_manifest::ManifestPackage,
    keg: &str,
    link: &str,
) -> Result<()> {
    let prefix = store.prefix();
    let formulae = store.installed()?;
    if formulae
        .iter()
        .any(|receipt| receipt.name == declared.token)
    {
        bail!(
            "refusing tap package '{}': a standard brewrs formula with the same name is installed; tap packages never shadow official packages",
            declared.token
        );
    }
    let casks = crate::infra::cask_store::installed(store)?;
    if casks.iter().any(|receipt| receipt.token == declared.token) {
        bail!(
            "refusing tap package '{}': a brewrs cask with the same token is installed; tap packages never shadow casks",
            declared.token
        );
    }
    for cask in &casks {
        if cask.links.iter().any(|entry| entry.dest == link)
            || cask.wrappers.iter().any(|entry| entry.dest == link)
        {
            bail!(
                "refusing tap package '{}': cask '{}' already owns '{}'",
                declared.token,
                cask.token,
                link
            );
        }
    }

    let link_path = prefix.join(link);
    match fs::symlink_metadata(&link_path) {
        Ok(metadata) => bail!(
            "refusing tap package '{}': destination '{}' already exists ({})",
            declared.token,
            link_path.display(),
            describe_path(&metadata)
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed to inspect '{}'", link_path.display()));
        }
    }

    let keg_path = prefix.join(keg);
    let package_dir = keg_path
        .parent()
        .context("tap keg path has no package directory")?;
    match fs::symlink_metadata(package_dir) {
        Ok(metadata) => bail!(
            "refusing tap package '{}': incomplete tap payload state already exists at '{}' ({})",
            declared.token,
            package_dir.display(),
            describe_path(&metadata)
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed to inspect '{}'", package_dir.display()));
        }
    }

    for other in all_tapped(prefix)? {
        for receipt in read_receipts(&other)? {
            if receipt.token == declared.token {
                bail!(
                    "refusing tap package '{}': the same token is already installed from '{}'; uninstall `{}` first",
                    declared.token,
                    receipt.tap,
                    receipt.qualified
                );
            }
            if receipt.bin == declared.bin {
                bail!(
                    "refusing tap package '{}': executable '{}' is already provided by '{}'",
                    declared.token,
                    declared.bin,
                    receipt.qualified
                );
            }
        }
    }

    if tapped.manifest_sha256().is_empty() {
        bail!("tap '{}' has an empty manifest digest", tapped.record.tap);
    }
    Ok(())
}

pub(super) fn render_plan(formatter: &DnfStyleFormatter, plan: &TapPlan, title: &str) {
    formatter.note(&format!(
        "{}  {}",
        formatter.section(title),
        formatter.info(&plan.qualified)
    ));
    let state = match &plan.installed {
        Some(receipt) => format!("installed ({})", receipt.version),
        None => "not installed".to_string(),
    };
    let prefix = &plan.prefix;
    for (label, value) in [
        ("Tap", plan.tap.clone()),
        ("Package", plan.qualified.clone()),
        ("Version", plan.version.clone()),
        ("Architecture", plan.architecture.to_string()),
        ("Artifact", plan.artifact_url.clone()),
        ("SHA-256", plan.artifact_sha256.clone()),
        ("Executable", plan.bin.clone()),
        ("Link", prefix.join(&plan.link).display().to_string()),
        ("Keg", prefix.join(&plan.keg).display().to_string()),
        ("Manifest", plan.source_manifest.display().to_string()),
        ("Manifest SHA", plan.manifest_sha256.clone()),
        ("State", state),
    ] {
        formatter.note(&formatter.field(label, &value));
    }
}
