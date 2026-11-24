use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};

use crate::commands::install::prefix_path;
use crate::config::Config;
use crate::domain::tap_manifest::{
    MAX_PACKAGES, TAPROOM, TapManifest, TapRecord, host_arch, parse_tap_name,
};
use crate::infra::store::{InstallLock, Store, ensure_directory, write_new_file_atomic};
use crate::ui::DnfStyleFormatter;

use super::fsguard::{directory_entries_exact, read_dir_names, verify_ancestor_dirs};
use super::state::{
    MANIFEST_FILE, RECEIPTS, RECORD_FILE, TAPS, all_tapped, encode_json, read_receipts,
    read_source_manifest, read_tapped, sha256_hex, short_digest, tap_dir, taps_root,
};

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
