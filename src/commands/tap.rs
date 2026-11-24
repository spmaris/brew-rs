//! Explicit, local third-party tap lane.
//!
//! This module manages a deliberately narrow slice of Homebrew-style taps:
//!
//! * a user registers one local declarative TOML manifest with
//!   `brewrs tap add <owner>/<repo> --manifest <path>`;
//! * every package must be requested as a fully qualified
//!   `<owner>/<repo>/<token>` target, so a tap package can never silently
//!   resolve to an official formula or cask;
//! * the selected artifact must be a naked macOS executable for the host
//!   architecture, fetched over validated public HTTPS, and pinned to an exact
//!   SHA-256 digest;
//! * nothing is evaluated: no Ruby, no shell, no source build, and no formula
//!   compatibility claim;
//! * tap receipts live in their own state root, never in the standard formula
//!   or cask receipt directories, so `brewrs upgrade` never treats a tap
//!   package as an official package.
//!
//! Installed state is laid out as:
//!
//! ```text
//! <prefix>/.brewrs/state/taps/<owner>/<repo>/tap.json       registry record
//! <prefix>/.brewrs/state/taps/<owner>/<repo>/manifest.toml  exact manifest copy
//! <prefix>/.brewrs/state/taps/<owner>/<repo>/receipts/<token>.json
//! <prefix>/Taproom/<owner>/<repo>/<token>/<version>/bin/<bin>
//! <prefix>/bin/<bin>                                        relative symlink
//! ```
//!
//! The writer lock is shared with the standard store, so tap installs and
//! formula installs cannot publish colliding `bin/` entries concurrently.

use std::fs;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::{config::Config, ui::DnfStyleFormatter};

use super::install::{confirm, prefix_path};
use crate::domain::tap_manifest::{
    MAX_MANIFEST_BYTES, MAX_PACKAGES, MAX_RECEIPT_BYTES, MAX_RECORD_BYTES, MAX_TAPS, TAPROOM,
    TapManifest, TapReceipt, TapRecord, host_arch, parse_qualified, parse_tap_name,
};
use crate::infra::store::{
    InstallLock, Store, create_symlink, ensure_directory, stages_dir, state_dir,
    write_new_file_atomic,
};
use crate::infra::tap_artifact::{hash_regular_file, stage_verified, verify_artifact};

/// Prefix-relative directory that holds registered tap state.
const TAPS: &str = "taps";
/// Registry record file name.
const RECORD_FILE: &str = "tap.json";
/// Exact manifest copy file name.
const MANIFEST_FILE: &str = "manifest.toml";
/// Per-tap receipt directory name.
const RECEIPTS: &str = "receipts";

/// A validated tap and its exact manifest copy.
struct TappedTap {
    record: TapRecord,
    manifest: TapManifest,
    dir: PathBuf,
    receipts: PathBuf,
}

impl TappedTap {
    fn manifest_sha256(&self) -> &str {
        &self.record.manifest_sha256
    }
}

/// One fully resolved tap install selection.
#[derive(Debug)]
struct TapPlan {
    prefix: PathBuf,
    qualified: String,
    tap: String,
    owner: String,
    repo: String,
    token: String,
    version: String,
    bin: String,
    architecture: &'static str,
    artifact_url: String,
    artifact_sha256: String,
    manifest_sha256: String,
    source_manifest: PathBuf,
    keg: String,
    link: String,
    receipt_path: PathBuf,
    installed: Option<TapReceipt>,
}

impl TapPlan {
    fn same_selection(&self, other: &Self) -> bool {
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

/// Register a local declarative manifest for a fully qualified tap.
pub(crate) fn add(
    config: &Config,
    prefix: Option<&Path>,
    tap_name: &str,
    manifest_path: &Path,
) -> Result<()> {
    let (owner, repo) = parse_tap_name(tap_name)?;
    let tap = format!("{owner}/{repo}");
    let (source, manifest_bytes) = read_source_manifest(manifest_path)?;
    let manifest = TapManifest::parse(&manifest_bytes, &tap).with_context(|| {
        format!(
            "refusing the tap manifest at '{}' for '{tap}'",
            manifest_path.display()
        )
    })?;
    let digest = sha256_hex(&manifest_bytes);

    // The manifest is fully validated before any prefix state is created.
    // `Store::open(..., true)` initializes only an absent or empty directory
    // and refuses protected or non-empty unowned prefixes.
    let path = prefix_path(prefix)?;
    Store::open(&path, true).context("Unable to initialize or open a brewrs prefix for the tap")?;
    let _lock = InstallLock::acquire(&path)?;
    let store = Store::open_existing(&path)?;
    let prefix = store.prefix();
    let dir = tap_dir(prefix, &owner, &repo);

    match fs::symlink_metadata(&dir) {
        Ok(_) => bail!(
            "tap '{tap}' is already registered in '{}'; run `brewrs untap {tap}` first",
            prefix.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).with_context(|| format!("failed to inspect '{}'", dir.display()));
        }
    }

    let roots = taps_root(prefix);
    ensure_directory(&roots, true)?;
    ensure_directory(&roots.join(&owner), true)?;
    fs::create_dir(&dir)
        .with_context(|| format!("failed to create the tap directory '{}'", dir.display()))?;

    let manifest_target = dir.join(MANIFEST_FILE);
    let record_target = dir.join(RECORD_FILE);
    let result = (|| -> Result<()> {
        write_new_file_atomic(&dir, &manifest_target, &manifest_bytes)?;
        let record = TapRecord::new(&tap, &source, &digest)?;
        let record_bytes = encode_json(&record)?;
        write_new_file_atomic(&dir, &record_target, &record_bytes)?;
        Ok(())
    })();

    if let Err(error) = result {
        let _ = fs::remove_file(&record_target);
        let _ = fs::remove_file(&manifest_target);
        let _ = fs::remove_dir(&dir);
        let _ = fs::remove_dir(roots.join(&owner));
        return Err(error).with_context(|| format!("failed to register tap '{tap}'"));
    }

    let ui = DnfStyleFormatter::new(config);
    ui.note(&format!("{}  {}", ui.section("Tapped"), ui.info(&tap)));
    ui.note(&ui.field("Manifest", &source.display().to_string()));
    ui.note(&ui.field("SHA-256", &digest));
    ui.note(&ui.field("Packages", &manifest.packages.len().to_string()));
    ui.note("\nNo artifact was downloaded or executed. Every package must be requested as a fully qualified <owner>/<repo>/<token> target.");
    Ok(())
}

/// List registered taps and their declared packages without touching Homebrew.
pub(crate) fn list(config: &Config, prefix: Option<&Path>) -> Result<()> {
    let path = prefix_path(prefix)?;
    let ui = DnfStyleFormatter::new(config);
    match fs::symlink_metadata(&path) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            ui.note(&format!(
                "No third-party taps: '{}' is not an initialized brewrs prefix.",
                path.display()
            ));
            return Ok(());
        }
        Err(error) => {
            return Err(error).with_context(|| format!("failed to inspect '{}'", path.display()));
        }
    }

    let store = Store::open_existing(&path)
        .context("Listing taps requires an initialized brewrs prefix")?;
    let taps = all_tapped(store.prefix())?;
    ui.note(&format!(
        "{}  {}",
        ui.section("Third-party taps"),
        ui.info(&store.prefix().display().to_string())
    ));
    if taps.is_empty() {
        ui.note("No third-party taps are registered. Use `brewrs tap add <owner>/<repo> --manifest <path>`.");
        return Ok(());
    }

    let architecture = host_arch().ok();
    let mut rows = Vec::new();
    for tapped in &taps {
        let receipts = read_receipts(tapped)?;
        for package in &tapped.manifest.packages {
            let installed = receipts
                .iter()
                .find(|receipt| receipt.token == package.token);
            let status = match installed {
                Some(receipt) => {
                    let matches_artifact = architecture.is_some_and(|architecture| {
                        package.artifacts.get(architecture).is_some_and(|artifact| {
                            artifact.sha256 == receipt.artifact_sha256
                                && artifact.url == receipt.artifact_url
                        })
                    });
                    let matches_identity = receipt.tap == tapped.record.tap
                        && receipt.token == package.token
                        && receipt.qualified == format!("{}/{}", tapped.record.tap, package.token)
                        && receipt.version == package.version
                        && receipt.bin == package.bin
                        && receipt.manifest_sha256 == tapped.record.manifest_sha256
                        && receipt.source_manifest == tapped.record.source;
                    if matches_identity && matches_artifact {
                        "yes"
                    } else {
                        "mismatch"
                    }
                }
                None => "no",
            };
            rows.push(vec![
                tapped.record.tap.clone(),
                package.token.clone(),
                package.version.clone(),
                package.bin.clone(),
                architecture.unwrap_or("unsupported").to_string(),
                status.to_string(),
            ]);
        }
    }
    println!(
        "{}",
        ui.table(
            &format!("Tap packages ({})", rows.len()),
            &["Tap", "Package", "Version", "Bin", "Arch", "Installed"],
            &rows
        )
    );
    ui.note("");
    for tapped in &taps {
        ui.note(&format!(
            "{}  {}  (manifest sha256 {})",
            ui.section(&tapped.record.tap),
            tapped.record.source,
            short_digest(tapped.manifest_sha256())
        ));
    }
    Ok(())
}

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
        ui.note("\nDry run: 1 change planned, nothing applied.");
        return Ok(());
    }
    if config.general.confirm_operations && !yes && !confirm("Install 1 package?")? {
        eprintln!("Cancelled. No packages changed.");
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
        ui.note("\nDry run: 1 change planned, nothing applied.");
        return Ok(());
    }
    if config.general.confirm_operations && !yes && !confirm("Remove 1 package?")? {
        eprintln!("Cancelled. No packages changed.");
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

/// Remove a tap registry entry that has no installed packages.
pub(crate) fn untap(config: &Config, prefix: Option<&Path>, tap_name: &str) -> Result<()> {
    let (owner, repo) = parse_tap_name(tap_name)?;
    let tap = format!("{owner}/{repo}");
    let path = prefix_path(prefix)?;
    Store::open_existing(&path).context("Untapping requires an initialized brewrs prefix")?;
    let _lock = InstallLock::acquire(&path)?;
    let store = Store::open_existing(&path)?;
    let prefix = store.prefix();
    let tapped = read_tapped(prefix, &tap)
        .with_context(|| format!("tap '{tap}' is not registered in '{}'", prefix.display()))?;

    let receipts = tapped.dir.join(RECEIPTS);
    match fs::symlink_metadata(&receipts) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => bail!(
            "refusing to untap '{tap}': '{}' is not a real directory",
            receipts.display()
        ),
        Ok(_) => {
            let entries = read_dir_names(&receipts, MAX_PACKAGES + 1, "tap receipt")?;
            if !entries.is_empty() {
                bail!(
                    "refusing to untap '{tap}': installed package receipts remain ({}); run `brewrs tap uninstall <owner>/<repo>/<token>` for each package first",
                    entries.join(", ")
                );
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed to inspect '{}'", receipts.display()));
        }
    }

    let taproom_package_rel = Path::new(TAPROOM).join(&owner).join(&repo);
    let taproom_package = prefix.join(&taproom_package_rel);
    if verify_ancestor_dirs(prefix, &taproom_package_rel, "tap payload")?
        && let Ok(metadata) = fs::symlink_metadata(&taproom_package)
    {
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            bail!(
                "refusing to untap '{tap}': '{}' is not a real directory",
                taproom_package.display()
            );
        }
        let entries = read_dir_names(&taproom_package, MAX_PACKAGES + 1, "tap payload")?;
        if !entries.is_empty() {
            bail!(
                "refusing to untap '{tap}': installed payload directories remain in '{}'",
                taproom_package.display()
            );
        }
    }

    let expected: Vec<&str> = if receipts.exists() {
        vec![RECORD_FILE, MANIFEST_FILE, RECEIPTS]
    } else {
        vec![RECORD_FILE, MANIFEST_FILE]
    };
    directory_entries_exact(&tapped.dir, &expected, "tap state directory")?;

    let record_rel = Path::new(".brewrs")
        .join("state")
        .join(TAPS)
        .join(&owner)
        .join(&repo)
        .join(RECORD_FILE);
    if !verify_ancestor_dirs(prefix, &record_rel, "tap state")? {
        bail!("refusing to untap '{tap}': its state directory disappeared");
    }

    let record_path = tapped.dir.join(RECORD_FILE);
    let manifest_path = tapped.dir.join(MANIFEST_FILE);
    fs::remove_file(&record_path)
        .with_context(|| format!("failed to remove '{}'", record_path.display()))?;
    fs::remove_file(&manifest_path)
        .with_context(|| format!("failed to remove '{}'", manifest_path.display()))?;
    if receipts.exists() {
        fs::remove_dir(&receipts)
            .with_context(|| format!("failed to remove '{}'", receipts.display()))?;
    }
    fs::remove_dir(&tapped.dir)
        .with_context(|| format!("failed to remove '{}'", tapped.dir.display()))?;
    let owner_rel = Path::new(".brewrs").join("state").join(TAPS).join(&owner);
    if verify_ancestor_dirs(prefix, &owner_rel, "tap state")? {
        let _ = fs::remove_dir(taps_root(prefix).join(&owner));
    }
    if verify_ancestor_dirs(prefix, &taproom_package_rel, "tap payload")? {
        let _ = fs::remove_dir(&taproom_package);
    }
    let taproom_owner_rel = Path::new(TAPROOM).join(&owner);
    if verify_ancestor_dirs(prefix, &taproom_owner_rel, "tap payload")? {
        let _ = fs::remove_dir(prefix.join(&taproom_owner_rel));
    }
    let _ = fs::remove_dir(prefix.join(TAPROOM));

    let ui = DnfStyleFormatter::new(config);
    ui.note(&format!("{}  {}", ui.section("Untapped"), ui.info(&tap)));
    ui.note("No installed tap package was touched.");
    Ok(())
}

fn build_plan(prefix: &Path, package: &str) -> Result<TapPlan> {
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
fn validate_receipt_binding(
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

fn check_collisions(
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

fn verify_installed_state(prefix: &Path, receipt: &TapReceipt) -> Result<()> {
    let link_rel = Path::new(&receipt.link);
    let link = prefix.join(link_rel);
    if !verify_ancestor_dirs(prefix, link_rel, "tap link")? {
        bail!(
            "refusing '{}': the tap link parent directory is missing",
            link.display()
        );
    }
    let metadata = fs::symlink_metadata(&link)
        .with_context(|| format!("failed to inspect '{}'", link.display()))?;
    if !metadata.file_type().is_symlink() {
        bail!(
            "refusing '{}': the tap link is not a symlink",
            link.display()
        );
    }
    let expected_target = Path::new("..")
        .join(&receipt.keg)
        .join("bin")
        .join(&receipt.bin);
    let actual_target =
        fs::read_link(&link).with_context(|| format!("failed to read '{}'", link.display()))?;
    if actual_target.as_os_str() != expected_target.as_os_str() {
        bail!(
            "refusing '{}': link target is '{}', not '{}'",
            link.display(),
            actual_target.display(),
            expected_target.display()
        );
    }

    let executable_rel = Path::new(&receipt.keg).join("bin").join(&receipt.bin);
    let executable = prefix.join(&executable_rel);
    if !verify_ancestor_dirs(prefix, &executable_rel, "tap executable")? {
        bail!(
            "refusing '{}': the tap executable parent directory is missing",
            executable.display()
        );
    }
    let metadata = fs::symlink_metadata(&executable)
        .with_context(|| format!("failed to inspect '{}'", executable.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        bail!(
            "refusing '{}': the tap executable is not a regular file",
            executable.display()
        );
    }
    if metadata.len() != receipt.artifact_bytes {
        bail!(
            "refusing '{}': size {} does not match the receipt's {} bytes",
            executable.display(),
            metadata.len(),
            receipt.artifact_bytes
        );
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o111 == 0 {
            bail!(
                "refusing '{}': the tap executable is not executable",
                executable.display()
            );
        }
    }
    Ok(())
}

fn verify_installed_artifact(prefix: &Path, receipt: &TapReceipt) -> Result<u64> {
    verify_installed_state(prefix, receipt)?;
    let executable = prefix.join(&receipt.keg).join("bin").join(&receipt.bin);
    verify_artifact(&executable, &receipt.artifact_sha256, &receipt.architecture)
}

fn verify_removal_state(prefix: &Path, receipt: &TapReceipt) -> Result<()> {
    receipt.validate()?;
    let link_rel = Path::new(&receipt.link);
    let link = prefix.join(link_rel);
    if verify_ancestor_dirs(prefix, link_rel, "tap link")? {
        match fs::symlink_metadata(&link) {
            Ok(metadata) if !metadata.file_type().is_symlink() => bail!(
                "refusing to remove '{}': it is not a symlink",
                link.display()
            ),
            Ok(_) => {
                let expected_target = Path::new("..")
                    .join(&receipt.keg)
                    .join("bin")
                    .join(&receipt.bin);
                let actual_target = fs::read_link(&link)
                    .with_context(|| format!("failed to read '{}'", link.display()))?;
                if actual_target.as_os_str() != expected_target.as_os_str() {
                    bail!(
                        "refusing to remove '{}': link target is '{}', not '{}'",
                        link.display(),
                        actual_target.display(),
                        expected_target.display()
                    );
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("failed to inspect '{}'", link.display()));
            }
        }
    }

    let keg_rel = Path::new(&receipt.keg);
    let keg = prefix.join(keg_rel);
    if verify_ancestor_dirs(prefix, keg_rel, "tap keg")? {
        match fs::symlink_metadata(&keg) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => bail!(
                "refusing to remove '{}': the tap keg is not a real directory",
                keg.display()
            ),
            Ok(_) => {
                directory_entries_exact(&keg, &["bin"], "tap keg")?;
                let bin_dir = keg.join("bin");
                directory_entries_exact(&bin_dir, &[&receipt.bin], "tap executable directory")?;
                let executable_rel = keg_rel.join("bin").join(&receipt.bin);
                let executable = prefix.join(&executable_rel);
                if !verify_ancestor_dirs(prefix, &executable_rel, "tap executable")? {
                    bail!(
                        "refusing to remove '{}': the tap executable parent directory is missing",
                        executable.display()
                    );
                }
                let metadata = fs::symlink_metadata(&executable)
                    .with_context(|| format!("failed to inspect '{}'", executable.display()))?;
                if metadata.file_type().is_symlink() || !metadata.is_file() {
                    bail!(
                        "refusing to remove '{}': it is not a regular file",
                        executable.display()
                    );
                }
                hash_regular_file(
                    &executable,
                    &receipt.artifact_sha256,
                    crate::domain::tap_manifest::MAX_ARTIFACT_BYTES,
                    "installed tap artifact",
                )?;
                let package_dir = keg.parent().context("tap keg has no package directory")?;
                directory_entries_exact(package_dir, &[&receipt.version], "tap package directory")?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("failed to inspect '{}'", keg.display()));
            }
        }
    }
    Ok(())
}

fn commit_install(
    store: &Store,
    plan: &TapPlan,
    staged: &Path,
    receipt_bytes: &[u8],
) -> Result<()> {
    let prefix = store.prefix();
    let link = prefix.join(&plan.link);
    match fs::symlink_metadata(&link) {
        Ok(metadata) => bail!(
            "refusing to publish '{}': it already exists ({})",
            link.display(),
            describe_path(&metadata)
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).with_context(|| format!("failed to inspect '{}'", link.display()));
        }
    }

    let keg = prefix.join(&plan.keg);
    if fs::symlink_metadata(&keg).is_ok() {
        bail!(
            "refusing to publish '{}': the keg path already exists",
            keg.display()
        );
    }

    let mut created = Vec::new();
    if let Err(error) = ensure_taproom(prefix, plan, &mut created) {
        remove_created_dirs(&created);
        return Err(error);
    }

    let mut link_created = false;
    let mut file_created = false;
    let result = (|| -> Result<()> {
        ensure_directory(&keg, true)?;
        let bin_dir = keg.join("bin");
        ensure_directory(&bin_dir, true)?;
        let executable = bin_dir.join(&plan.bin);
        fs::rename(staged, &executable).with_context(|| {
            format!(
                "failed to publish the tap executable to '{}'",
                executable.display()
            )
        })?;
        file_created = true;

        let target = Path::new("..").join(&plan.keg).join("bin").join(&plan.bin);
        create_symlink(&target, &link)
            .with_context(|| format!("failed to create the tap link '{}'", link.display()))?;
        link_created = true;

        let receipt_dir = plan
            .receipt_path
            .parent()
            .context("tap receipt path has no parent directory")?;
        ensure_directory(receipt_dir, true)?;
        write_new_file_atomic(receipt_dir, &plan.receipt_path, receipt_bytes)?;
        Ok(())
    })();

    if let Err(error) = result {
        let mut issues = Vec::new();
        if link_created && let Err(remove_error) = fs::remove_file(&link) {
            issues.push(format!(
                "failed to remove '{}': {remove_error}",
                link.display()
            ));
        }
        if file_created {
            let executable = keg.join("bin").join(&plan.bin);
            if let Err(remove_error) = fs::remove_file(&executable) {
                issues.push(format!(
                    "failed to remove '{}': {remove_error}",
                    executable.display()
                ));
            }
        }
        // `write_new_file_atomic` publishes only on success, so an error here
        // never created this receipt; never unlink a pre-existing path.
        let _ = fs::remove_dir(keg.join("bin"));
        let _ = fs::remove_dir(&keg);
        remove_created_dirs(&created);
        if issues.is_empty() {
            return Err(error);
        }
        return Err(error.context(format!(
            "tap install rollback was incomplete: {}",
            issues.join("; ")
        )));
    }

    crate::infra::store::sync_directory(&link);
    if let Some(parent) = keg.parent() {
        crate::infra::store::sync_directory(parent);
    }
    Ok(())
}

fn ensure_taproom(prefix: &Path, plan: &TapPlan, created: &mut Vec<PathBuf>) -> Result<()> {
    for path in [
        prefix.join(TAPROOM),
        prefix.join(TAPROOM).join(&plan.owner),
        prefix.join(TAPROOM).join(&plan.owner).join(&plan.repo),
        prefix
            .join(TAPROOM)
            .join(&plan.owner)
            .join(&plan.repo)
            .join(&plan.token),
    ] {
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => bail!(
                "refusing '{}': a tap-room path must be a real directory",
                path.display()
            ),
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if let Err(create_error) = fs::create_dir(&path) {
                    remove_created_dirs(created);
                    return Err(create_error)
                        .with_context(|| format!("failed to create '{}'", path.display()));
                }
                created.push(path);
            }
            Err(error) => {
                remove_created_dirs(created);
                return Err(error)
                    .with_context(|| format!("failed to inspect '{}'", path.display()));
            }
        }
    }
    Ok(())
}

fn remove_created_dirs(created: &[PathBuf]) {
    for path in created.iter().rev() {
        let _ = fs::remove_dir(path);
    }
}

fn commit_uninstall(prefix: &Path, receipt: &TapReceipt) -> Result<()> {
    verify_removal_state(prefix, receipt)?;
    let quarantine = tempfile::Builder::new()
        .prefix("tap-remove-")
        .tempdir_in(stages_dir(prefix))
        .context("failed to create a private tap removal quarantine")?;
    let mut moved: Vec<(PathBuf, PathBuf)> = Vec::new();

    let link_rel = Path::new(&receipt.link);
    let link = prefix.join(link_rel);
    let parked_link = quarantine.path().join("link");
    let keg_rel = Path::new(&receipt.keg);
    let keg = prefix.join(keg_rel);
    let executable_rel = keg_rel.join("bin").join(&receipt.bin);
    let executable = prefix.join(&executable_rel);
    let parked_file = quarantine.path().join("artifact");
    let parked_receipt = quarantine.path().join("receipt.json");
    let receipt_rel = receipt_relative_path(receipt);

    let result = (|| -> Result<()> {
        if fs::symlink_metadata(&link).is_ok() {
            if !verify_ancestor_dirs(prefix, link_rel, "tap link")? {
                bail!(
                    "refusing to park '{}': its parent directory disappeared",
                    link.display()
                );
            }
            fs::rename(&link, &parked_link)
                .with_context(|| format!("failed to park '{}'", link.display()))?;
            moved.push((link.clone(), parked_link));
        }
        if fs::symlink_metadata(&executable).is_ok() {
            if !verify_ancestor_dirs(prefix, &executable_rel, "tap executable")? {
                bail!(
                    "refusing to park '{}': its parent directory disappeared",
                    executable.display()
                );
            }
            fs::rename(&executable, &parked_file)
                .with_context(|| format!("failed to park '{}'", executable.display()))?;
            moved.push((executable.clone(), parked_file));
        }
        let source_receipt = prefix.join(&receipt_rel);
        if !verify_ancestor_dirs(prefix, &receipt_rel, "tap receipt")? {
            bail!(
                "refusing to park the tap receipt for '{}': its parent directory disappeared",
                receipt.qualified
            );
        }
        fs::rename(&source_receipt, &parked_receipt).with_context(|| {
            format!("failed to park the tap receipt for '{}'", receipt.qualified)
        })?;
        moved.push((source_receipt, parked_receipt));
        Ok(())
    })();

    if let Err(error) = result {
        let mut issues = Vec::new();
        for (source, parked) in moved.iter().rev() {
            if let Err(restore_error) = fs::rename(parked, source) {
                issues.push(format!(
                    "failed to restore '{}': {restore_error}",
                    source.display()
                ));
            }
        }
        if issues.is_empty() {
            return Err(error);
        }
        let kept = quarantine.keep();
        return Err(error.context(format!(
            "tap removal rollback was incomplete: {}; quarantine kept at '{}'",
            issues.join("; "),
            kept.display()
        )));
    }

    // Pruning is best-effort and only runs while every ancestor is still a
    // real directory below the prefix. A race leaves empty directories and the
    // quarantine instead of removing something outside the prefix.
    if verify_ancestor_dirs(prefix, &keg_rel.join("bin"), "tap keg").unwrap_or(false) {
        let _ = fs::remove_dir(keg.join("bin"));
        let _ = fs::remove_dir(&keg);
    }
    if let Some(package_rel) = keg_rel.parent()
        && verify_ancestor_dirs(prefix, package_rel, "tap package").unwrap_or(false)
    {
        let _ = fs::remove_dir(prefix.join(package_rel));
    }
    drop(quarantine);
    Ok(())
}

fn receipt_relative_path(receipt: &TapReceipt) -> PathBuf {
    let (owner, repo) = parse_tap_name(&receipt.tap).expect("validated tap name");
    Path::new(".brewrs")
        .join("state")
        .join(TAPS)
        .join(owner)
        .join(repo)
        .join(RECEIPTS)
        .join(format!("{}.json", receipt.token))
}

fn read_tapped(prefix: &Path, tap: &str) -> Result<TappedTap> {
    let (owner, repo) = parse_tap_name(tap)?;
    let dir = tap_dir(prefix, &owner, &repo);
    let record_rel = Path::new(".brewrs")
        .join("state")
        .join(TAPS)
        .join(&owner)
        .join(&repo)
        .join(RECORD_FILE);
    if !verify_ancestor_dirs(prefix, &record_rel, "tap state")? {
        bail!(
            "tap '{tap}' is not registered in '{}'; run `brewrs tap add {tap} --manifest <path>`",
            prefix.display()
        );
    }
    ensure_real_dir(&dir, "tap directory").with_context(|| {
        format!(
            "tap '{tap}' is not registered in '{}'; run `brewrs tap add {tap} --manifest <path>`",
            prefix.display()
        )
    })?;

    let record_path = dir.join(RECORD_FILE);
    let record_bytes = read_bounded_regular(&record_path, MAX_RECORD_BYTES, "tap registry record")?;
    let record: TapRecord = serde_json::from_slice(&record_bytes)
        .with_context(|| format!("failed to decode '{}'", record_path.display()))?;
    record.validate(tap)?;

    let manifest_path = dir.join(MANIFEST_FILE);
    let manifest_bytes =
        read_bounded_regular(&manifest_path, MAX_MANIFEST_BYTES, "tap manifest copy")?;
    let digest = sha256_hex(&manifest_bytes);
    if digest != record.manifest_sha256 {
        bail!(
            "tap '{tap}' manifest copy changed since it was registered (expected sha256 {}, got {digest}); run `brewrs untap {tap}` and add it again",
            record.manifest_sha256
        );
    }
    let manifest = TapManifest::parse(&manifest_bytes, tap)
        .with_context(|| format!("failed to validate '{}'", manifest_path.display()))?;

    let receipts = dir.join(RECEIPTS);
    match fs::symlink_metadata(&receipts) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => bail!(
            "refusing '{}': the tap receipt root is not a real directory",
            receipts.display()
        ),
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed to inspect '{}'", receipts.display()));
        }
    }

    Ok(TappedTap {
        receipts,
        dir,
        record,
        manifest,
    })
}

fn all_tapped(prefix: &Path) -> Result<Vec<TappedTap>> {
    let root = taps_root(prefix);
    match fs::symlink_metadata(&root) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => bail!(
            "refusing '{}': the tap state root is not a real directory",
            root.display()
        ),
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(error).with_context(|| format!("failed to inspect '{}'", root.display()));
        }
    }

    let owners = read_dir_names(&root, MAX_TAPS, "tap owner")?;
    let mut taps = Vec::new();
    for owner in owners {
        let owner_dir = root.join(&owner);
        ensure_real_dir(&owner_dir, "tap owner directory")?;
        let repos = read_dir_names(&owner_dir, MAX_TAPS, "tap repository")?;
        for repo in repos {
            let dir = owner_dir.join(&repo);
            ensure_real_dir(&dir, "tap directory")?;
            let entries = read_dir_names(&dir, MAX_PACKAGES + 3, "tap state entry")?;
            if entries.is_empty() {
                continue;
            }
            let tap = format!("{owner}/{repo}");
            taps.push(read_tapped(prefix, &tap)?);
            if taps.len() > MAX_TAPS {
                bail!(
                    "more than {MAX_TAPS} taps are registered in '{}'",
                    prefix.display()
                );
            }
        }
    }
    Ok(taps)
}

fn read_receipts(tapped: &TappedTap) -> Result<Vec<TapReceipt>> {
    match fs::symlink_metadata(&tapped.receipts) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => bail!(
            "refusing '{}': the tap receipt root is not a real directory",
            tapped.receipts.display()
        ),
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed to inspect '{}'", tapped.receipts.display()));
        }
    }

    let entries = read_dir_names(&tapped.receipts, MAX_PACKAGES + 1, "tap receipt")?;
    let mut receipts = Vec::new();
    for entry in entries {
        let Some(stem) = entry.strip_suffix(".json") else {
            bail!(
                "refusing '{}': unexpected tap receipt entry '{entry}'",
                tapped.receipts.display()
            );
        };
        let path = tapped.receipts.join(&entry);
        let Some(receipt) = read_optional_receipt(&path)? else {
            continue;
        };
        if receipt.tap != tapped.record.tap || receipt.token != stem {
            bail!(
                "tap receipt '{}' does not match tap '{}' and token '{stem}'",
                path.display(),
                tapped.record.tap
            );
        }
        receipts.push(receipt);
    }
    Ok(receipts)
}

fn read_optional_receipt(path: &Path) -> Result<Option<TapReceipt>> {
    match fs::symlink_metadata(path) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("failed to inspect '{}'", path.display()));
        }
    }
    let bytes = read_bounded_regular(path, MAX_RECEIPT_BYTES, "tap receipt")?;
    let receipt: TapReceipt = serde_json::from_slice(&bytes)
        .with_context(|| format!("failed to decode the tap receipt '{}'", path.display()))?;
    receipt.validate()?;
    Ok(Some(receipt))
}

fn read_source_manifest(path: &Path) -> Result<(PathBuf, Vec<u8>)> {
    if path.as_os_str().is_empty() {
        bail!("refusing an empty tap manifest path");
    }
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("failed to inspect tap manifest '{}'", path.display()))?;
    if metadata.file_type().is_symlink() {
        bail!(
            "refusing tap manifest '{}': the source path is a symlink; pass the real file",
            path.display()
        );
    }
    if !metadata.is_file() {
        bail!(
            "refusing tap manifest '{}': the source path is not a regular file",
            path.display()
        );
    }
    if metadata.len() == 0 || metadata.len() > MAX_MANIFEST_BYTES {
        bail!(
            "refusing tap manifest '{}': size {} is outside the supported range",
            path.display(),
            metadata.len()
        );
    }
    let canonical = fs::canonicalize(path)
        .with_context(|| format!("failed to resolve tap manifest '{}'", path.display()))?;
    let bytes = read_bounded_regular(&canonical, MAX_MANIFEST_BYTES, "tap manifest")?;
    if bytes.is_empty() {
        bail!("refusing empty tap manifest '{}'", canonical.display());
    }
    Ok((canonical, bytes))
}

fn read_bounded_regular(path: &Path, limit: u64, what: &str) -> Result<Vec<u8>> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("failed to inspect {what} '{}'", path.display()))?;
    if metadata.file_type().is_symlink() {
        bail!("refusing {what} '{}': it is a symlink", path.display());
    }
    if !metadata.is_file() {
        bail!(
            "refusing {what} '{}': it is not a regular file",
            path.display()
        );
    }
    if metadata.len() > limit {
        bail!(
            "refusing {what} '{}': {} bytes exceeds the {limit} byte limit",
            path.display(),
            metadata.len()
        );
    }
    let bytes =
        fs::read(path).with_context(|| format!("failed to read {what} '{}'", path.display()))?;
    if bytes.len() as u64 > limit {
        bail!(
            "refusing {what} '{}': it exceeds the {limit} byte limit",
            path.display()
        );
    }
    Ok(bytes)
}

/// Verify that every existing ancestor component below `prefix` is a real
/// directory, never a symlink. Missing components are reported as absent.
///
/// The final component is intentionally not inspected here; callers use
/// `symlink_metadata` for it so a symlinked executable or link is handled
/// explicitly.
fn verify_ancestor_dirs(prefix: &Path, relative: &Path, what: &str) -> Result<bool> {
    if relative.as_os_str().is_empty() {
        bail!("refusing {what}: empty relative path");
    }
    if relative.is_absolute() {
        bail!("refusing {what}: path must be relative to the brewrs prefix");
    }
    let prefix_metadata = fs::symlink_metadata(prefix)
        .with_context(|| format!("failed to inspect brewrs prefix '{}'", prefix.display()))?;
    if prefix_metadata.file_type().is_symlink() || !prefix_metadata.is_dir() {
        bail!(
            "refusing {what}: brewrs prefix '{}' is not a real directory",
            prefix.display()
        );
    }

    let mut components = relative.components();
    let Some(final_component) = components.next_back() else {
        bail!("refusing {what}: empty relative path");
    };
    if !matches!(final_component, Component::Normal(_)) {
        bail!("refusing {what}: path does not end in a normal component");
    }

    let mut ancestors = Vec::new();
    for component in components {
        match component {
            Component::Normal(name) => ancestors.push(name.to_os_string()),
            _ => bail!(
                "refusing {what}: relative path contains '{}'",
                component.as_os_str().to_string_lossy()
            ),
        }
    }

    let mut current = prefix.to_path_buf();
    for name in ancestors {
        current.push(&name);
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => bail!(
                "refusing {what}: ancestor '{}' is a symlink",
                current.display()
            ),
            Ok(metadata) if metadata.is_dir() => {}
            Ok(_) => bail!(
                "refusing {what}: ancestor '{}' is not a directory",
                current.display()
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("failed to inspect '{}'", current.display()));
            }
        }
    }
    Ok(true)
}

fn ensure_real_dir(path: &Path, what: &str) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => bail!(
            "refusing '{}': the {what} is not a real directory",
            path.display()
        ),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            bail!("{what} '{}' is missing", path.display())
        }
        Err(error) => Err(error).with_context(|| format!("failed to inspect '{}'", path.display())),
    }
}

fn read_dir_names(directory: &Path, limit: usize, what: &str) -> Result<Vec<String>> {
    ensure_real_dir(directory, what)?;
    let mut names = Vec::new();
    for entry in fs::read_dir(directory)
        .with_context(|| format!("failed to read '{}'", directory.display()))?
    {
        let entry = entry
            .with_context(|| format!("failed to read an entry in '{}'", directory.display()))?;
        let name = entry.file_name().into_string().map_err(|_| {
            anyhow::anyhow!(
                "refusing non-UTF-8 {what} entry in '{}'",
                directory.display()
            )
        })?;
        names.push(name);
        if names.len() > limit {
            bail!(
                "refusing '{}': more than {limit} {what} entries",
                directory.display()
            );
        }
    }
    names.sort();
    Ok(names)
}

fn directory_entries_exact(directory: &Path, expected: &[&str], what: &str) -> Result<()> {
    let names = read_dir_names(directory, expected.len() + 1, what)?;
    let mut expected: Vec<&str> = expected.to_vec();
    expected.sort_unstable();
    if names.len() != expected.len()
        || names
            .iter()
            .map(String::as_str)
            .ne(expected.iter().copied())
    {
        bail!(
            "refusing '{}': expected exactly [{}], found [{}]",
            directory.display(),
            expected.join(", "),
            names.join(", ")
        );
    }
    Ok(())
}

fn taps_root(prefix: &Path) -> PathBuf {
    state_dir(prefix).join(TAPS)
}

fn tap_dir(prefix: &Path, owner: &str, repo: &str) -> PathBuf {
    taps_root(prefix).join(owner).join(repo)
}

fn render_plan(formatter: &DnfStyleFormatter, plan: &TapPlan, title: &str) {
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

fn describe_path(metadata: &fs::Metadata) -> &'static str {
    if metadata.file_type().is_symlink() {
        "symlink"
    } else if metadata.is_file() {
        "regular file"
    } else if metadata.is_dir() {
        "directory"
    } else {
        "special file"
    }
}

fn short_digest(digest: &str) -> &str {
    digest.get(..12).unwrap_or(digest)
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn encode_json<T: Serialize>(value: &T) -> Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec(value).context("failed to encode tap state")?;
    bytes.push(b'\n');
    Ok(bytes)
}

#[cfg(test)]
mod tests;
