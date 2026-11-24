use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{self, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::{config::Config, ui::DnfStyleFormatter};

use super::plan::PlannedFormula;
use super::store::{
    Dependency, Prepared, Receipt, Store, validate_conflicts, validate_dependencies_with_retained,
    validate_retained_pin_closure,
};
use super::{archive, default_target, download, plan, relocate};

pub(crate) async fn install(
    config: &Config,
    prefix: Option<&Path>,
    packages: &[String],
    dry_run: bool,
    yes: bool,
) -> Result<()> {
    if std::env::consts::OS != "macos" {
        bail!("Native installation is supported on macOS only; plan and fetch are still available");
    }
    let path = prefix_path(prefix)?;
    let store = Store::open(&path, false)?;
    let tag = default_target()?;
    let existing = store.installed()?;
    let retained = store.retained()?;
    let existing_casks = super::cask_store::installed(&store)?;
    let plan = plan::resolve(config, packages, &tag).await?;
    let pending = pending_packages(&plan, &existing, &retained)?;
    let mut projected = existing.clone();
    for item in &pending {
        projected.push(item.receipt.clone());
    }
    super::cask_store::validate_cask_conflicts(&existing_casks, &projected)?;
    let promotions = requested_promotions(&plan, &existing);
    let formatter = DnfStyleFormatter::new(config);
    let rows: Vec<_> = plan
        .iter()
        .map(|formula| {
            let action = if pending
                .iter()
                .any(|entry| entry.receipt.name == formula.name)
            {
                if formula.cellar == ":any" {
                    if formula.keg_only {
                        "Relocate & install (keg-only)"
                    } else {
                        "Relocate & install"
                    }
                } else if formula.keg_only {
                    "Install (keg-only)"
                } else {
                    "Install"
                }
            } else if promotions
                .iter()
                .any(|receipt| receipt.name == formula.name)
            {
                "Mark requested"
            } else {
                "Already installed"
            };
            let reason = if formula.requested
                || existing
                    .iter()
                    .any(|receipt| receipt.name == formula.name && receipt.requested)
            {
                "Requested"
            } else {
                "Dependency"
            };
            vec![
                formula.name.clone(),
                formula.version.clone(),
                action.into(),
                reason.into(),
            ]
        })
        .collect();
    println!(
        "{}  {}",
        formatter.section("Native install"),
        formatter.info(&tag)
    );
    println!(
        "{}",
        formatter.table(&["Package", "Version", "Action", "Reason"], &rows)
    );
    println!(
        "{}",
        formatter.field("Prefix", &store.prefix().display().to_string())
    );

    if pending.is_empty() && promotions.is_empty() {
        println!("\nNothing to install.");
        return Ok(());
    }
    if dry_run {
        let count = pending.len() + promotions.len();
        let noun = if count == 1 { "change" } else { "changes" };
        println!("\n{count} {noun} planned. No packages changed.");
        return Ok(());
    }
    io::stdout().flush()?;
    if config.general.confirm_operations && !yes && !confirm()? {
        println!("Cancelled. No packages changed.");
        return Ok(());
    }

    if pending.iter().any(|item| item.formula.cellar == ":any") {
        relocate::preflight()?;
    }
    let service_names: Vec<String> = pending
        .iter()
        .filter(|item| item.formula.has_service)
        .map(|item| item.formula.name.clone())
        .collect();
    let mut downloaded = Vec::new();
    for candidate in pending {
        let bottle = fetch_formula(config, candidate.formula).await?;
        downloaded.push((candidate, bottle));
    }
    let store = if promotions.is_empty() {
        Store::open(&path, true)?
    } else {
        Store::open_existing(&path)?
    };
    let mut staged = Vec::new();
    for (candidate, bottle) in downloaded {
        let needs_relocation = candidate.formula.cellar == ":any";
        staged.push((needs_relocation, stage_formula(&store, candidate, &bottle)?));
    }
    relocate::staged(
        store.prefix(),
        staged
            .iter()
            .map(|(needs_relocation, package)| relocate::Item {
                package,
                needs_relocation: *needs_relocation,
            }),
    )?;
    let prepared: Vec<_> = staged.into_iter().map(|(_, package)| package).collect();
    let count = prepared.len();
    let any_keg_only = prepared.iter().any(|package| package.receipt.keg_only);
    if promotions.is_empty() {
        store.commit(prepared)?;
    } else {
        store.commit_with_promotions(prepared, &promotions)?;
    }
    if count != 0 {
        println!(
            "\n{} {count} {}.",
            formatter.success("Installed"),
            if count == 1 { "formula" } else { "formulae" }
        );
        println!(
            "{}",
            formatter.field("Bin", &store.prefix().join("bin").display().to_string())
        );
        if any_keg_only {
            println!(
                "{}",
                formatter.field("Opt", &store.prefix().join("opt").display().to_string())
            );
        }
        if !service_names.is_empty() {
            println!(
                "Note: {} {} an optional service (not started automatically).",
                service_names.join(", "),
                if service_names.len() == 1 {
                    "declares"
                } else {
                    "declare"
                }
            );
        }
    }
    if !promotions.is_empty() {
        let names = promotions
            .iter()
            .map(|receipt| receipt.name.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        println!("{}: {names}", formatter.success("Marked as requested"));
    }
    Ok(())
}

fn requested_promotions(plan: &[PlannedFormula], installed: &[Receipt]) -> Vec<Receipt> {
    plan.iter()
        .filter(|formula| formula.requested)
        .filter_map(|formula| {
            installed
                .iter()
                .find(|receipt| receipt.name == formula.name && !receipt.requested)
                .cloned()
        })
        .collect()
}

async fn fetch_formula(config: &Config, formula: &PlannedFormula) -> Result<PathBuf> {
    download::fetch(config, &formula.url, &formula.sha256)
        .await
        .with_context(|| format!("Failed to fetch {} {}", formula.name, formula.version))
}

#[derive(Debug)]
struct Candidate<'a> {
    formula: &'a PlannedFormula,
    receipt: Receipt,
}

/// One planned upgrade: the installed receipt, its replacement receipt, and the
/// planned formula that supplies the replacement bottle.
///
/// Both [`upgrade`] and [`check_updates`] build this through
/// [`resolve_changes`], so the preview table, the eligibility rules and the
/// dependency-pin checks can never drift between them.
#[derive(Debug)]
struct UpgradeChange {
    old: Receipt,
    new: Receipt,
    formula: PlannedFormula,
}

fn stage_formula(store: &Store, candidate: Candidate<'_>, bottle: &Path) -> Result<Prepared> {
    let stage = store.stage()?;
    let receipt = candidate.receipt;
    archive::unpack(
        bottle,
        stage.path(),
        &receipt.name,
        &receipt.version,
        &receipt.sha256,
    )
    .with_context(|| format!("Failed to stage {} {}", receipt.name, receipt.version))?;
    if receipt.name == "nvm" {
        relocate_nvm_wrapper(store.prefix(), stage.path(), &receipt.version)
            .context("Failed to relocate the staged nvm wrapper")?;
    }
    Ok(Prepared { receipt, stage })
}

// Homebrew's portable nvm bottle still ships a wrapper with these markers.
// Match the reviewed wrapper, not a general shell grammar. The source is tied
// to this keg; the managed opt path lets user-created nvm helper links survive
// future upgrades, as the original wrapper intended.
fn relocate_nvm_wrapper(prefix: &Path, stage: &Path, version: &str) -> Result<()> {
    const LIMIT: u64 = 4096;
    let wrapper = stage.join("nvm.sh");
    if !fs::symlink_metadata(&wrapper)?.is_file() {
        bail!("nvm.sh must be a regular staged file");
    }
    let mut text = String::new();
    fs::File::open(&wrapper)?
        .take(LIMIT + 1)
        .read_to_string(&mut text)?;
    if text.len() as u64 > LIMIT {
        bail!("nvm.sh exceeds the wrapper size limit");
    }
    if !text.contains("@@HOMEBREW_") {
        return Ok(());
    }
    let cellar = format!("@@HOMEBREW_CELLAR@@/nvm/{version}/libexec/nvm.sh");
    const NVM_SH: &str = "@@HOMEBREW_PREFIX@@/opt/nvm/libexec/nvm.sh";
    const NVM_EXEC: &str = "@@HOMEBREW_PREFIX@@/opt/nvm/libexec/nvm-exec";
    let expected = format!(
        "# $NVM_DIR should be \"$HOME/.nvm\" by default to avoid user-installed nodes destroyed every update\n\
         [ -z \"$NVM_DIR\" ] && export NVM_DIR=\"$HOME/.nvm\"\n\
         \\. {cellar}\n\
         # \"nvm exec\" and certain 3rd party scripts expect \"nvm.sh\" and \"nvm-exec\" to exist under $NVM_DIR\n\
         [ -e \"$NVM_DIR\" ] || mkdir -p \"$NVM_DIR\"\n\
         [ -e \"$NVM_DIR/nvm.sh\" ] || ln -s {NVM_SH} \"$NVM_DIR/nvm.sh\"\n\
         [ -e \"$NVM_DIR/nvm-exec\" ] || ln -s {NVM_EXEC} \"$NVM_DIR/nvm-exec\"\n"
    );
    if text != expected {
        bail!("nvm.sh contains an unreviewed Homebrew wrapper");
    }
    let libexec = stage.join("libexec");
    if !fs::symlink_metadata(&libexec)?.is_dir()
        || !fs::symlink_metadata(libexec.join("nvm.sh"))?.is_file()
        || !fs::symlink_metadata(libexec.join("nvm-exec"))?.is_file()
    {
        bail!("nvm wrapper requires regular staged libexec scripts");
    }
    let prefix = prefix.canonicalize()?;
    let quote = |path: PathBuf| -> Result<String> {
        let value = path.to_str().context("nvm requires UTF-8 paths")?;
        if value.chars().any(char::is_control) || value.contains("@@HOMEBREW_") {
            bail!("unsupported path characters in nvm wrapper");
        }
        Ok(posix_quote(value))
    };
    let helpers = prefix.join("opt/nvm/libexec");
    let fixed = text
        .replace(
            &cellar,
            &quote(
                prefix
                    .join("Cellar/nvm")
                    .join(version)
                    .join("libexec/nvm.sh"),
            )?,
        )
        .replace(NVM_SH, &quote(helpers.join("nvm.sh"))?)
        .replace(NVM_EXEC, &quote(helpers.join("nvm-exec"))?);
    fs::write(wrapper, fixed)?;
    Ok(())
}

#[cfg(test)]
#[test]
fn nvm_wrapper_relocation_quotes_paths_and_rejects_unknown_wrappers() {
    let temp = tempfile::tempdir().unwrap();
    let prefix = temp.path().join("Brewrs User's Prefix");
    let stage = temp.path().join("stage");
    fs::create_dir(&prefix).unwrap();
    fs::create_dir_all(stage.join("libexec")).unwrap();
    fs::write(stage.join("libexec/nvm.sh"), "").unwrap();
    fs::write(stage.join("libexec/nvm-exec"), "").unwrap();
    let wrapper = stage.join("nvm.sh");
    let original = r#"# $NVM_DIR should be "$HOME/.nvm" by default to avoid user-installed nodes destroyed every update
[ -z "$NVM_DIR" ] && export NVM_DIR="$HOME/.nvm"
\. @@HOMEBREW_CELLAR@@/nvm/0.41.0/libexec/nvm.sh
# "nvm exec" and certain 3rd party scripts expect "nvm.sh" and "nvm-exec" to exist under $NVM_DIR
[ -e "$NVM_DIR" ] || mkdir -p "$NVM_DIR"
[ -e "$NVM_DIR/nvm.sh" ] || ln -s @@HOMEBREW_PREFIX@@/opt/nvm/libexec/nvm.sh "$NVM_DIR/nvm.sh"
[ -e "$NVM_DIR/nvm-exec" ] || ln -s @@HOMEBREW_PREFIX@@/opt/nvm/libexec/nvm-exec "$NVM_DIR/nvm-exec"
"#;
    fs::write(&wrapper, original).unwrap();
    relocate_nvm_wrapper(&prefix, &stage, "0.41.0").unwrap();
    let fixed = fs::read_to_string(&wrapper).unwrap();
    assert!(!fixed.contains("@@HOMEBREW_"));
    assert!(fixed.contains("Brewrs User'\\''s Prefix/Cellar/nvm/0.41.0/libexec/nvm.sh'"));
    assert!(fixed.contains("Brewrs User'\\''s Prefix/opt/nvm/libexec/nvm-exec'"));

    let unknown = original.replace("ln -s", "ln -sf");
    fs::write(&wrapper, &unknown).unwrap();
    assert!(relocate_nvm_wrapper(&prefix, &stage, "0.41.0").is_err());
    assert_eq!(fs::read_to_string(&wrapper).unwrap(), unknown);
}

fn receipt_for(
    formula: &PlannedFormula,
    plan: &[PlannedFormula],
    requested: bool,
) -> Result<Receipt> {
    let mut dependencies = Vec::new();
    for name in &formula.dependencies {
        let dependency = plan
            .iter()
            .find(|entry| entry.name == *name)
            .with_context(|| {
                format!(
                    "The plan for '{}' omitted dependency '{name}'",
                    formula.name
                )
            })?;
        dependencies.push(Dependency {
            name: dependency.name.clone(),
            version: dependency.version.clone(),
            sha256: dependency.sha256.clone(),
        });
    }
    dependencies.sort_by(|left, right| left.name.cmp(&right.name));
    let version_scheme = formula.version_scheme.with_context(|| {
        format!(
            "'{}' does not declare a Homebrew version scheme",
            formula.name
        )
    })?;
    Ok(Receipt {
        name: formula.name.clone(),
        version: formula.version.clone(),
        version_scheme,
        sha256: formula.sha256.clone(),
        requested,
        keg_only: formula.keg_only,
        conflicts_with: formula.conflicts_with.clone(),
        dependencies,
    })
}

pub(crate) async fn upgrade(
    config: &Config,
    prefix: Option<&Path>,
    packages: &[String],
    dry_run: bool,
    yes: bool,
) -> Result<()> {
    if std::env::consts::OS != "macos" {
        bail!("Native upgrades are supported on macOS only");
    }
    let path = prefix_path(prefix)?;
    let store = Store::open(&path, false)?;
    let installed = store.installed()?;
    let retained = store.retained()?;
    let selected = if packages.is_empty() {
        installed.clone()
    } else {
        removal_plan(packages, &installed)?
    };
    if selected.is_empty() {
        println!("No brewrs-managed packages to upgrade.");
        return Ok(());
    }

    let (tag, changes) = resolve_changes(config, &installed, &selected, &retained).await?;
    if changes.is_empty() {
        println!("All selected packages are up to date.");
        return Ok(());
    }

    let formatter = DnfStyleFormatter::new(config);
    render_upgrade_preview(&formatter, "Native upgrade", &tag, store.prefix(), &changes);
    if dry_run {
        println!("\nNo packages changed.");
        return Ok(());
    }
    io::stdout().flush()?;
    if config.general.confirm_operations && !yes && !confirm()? {
        println!("Cancelled. No packages changed.");
        return Ok(());
    }

    if changes.iter().any(|item| item.formula.cellar == ":any") {
        relocate::preflight()?;
    }
    let store = Store::open_existing(&path)?;
    let mut staged = Vec::new();
    for change in changes {
        let bottle = fetch_formula(config, &change.formula).await?;
        let needs_relocation = change.formula.cellar == ":any";
        let candidate = Candidate {
            formula: &change.formula,
            receipt: change.new,
        };
        staged.push((
            needs_relocation,
            change.old,
            stage_formula(&store, candidate, &bottle)?,
        ));
    }
    relocate::staged(
        store.prefix(),
        staged
            .iter()
            .map(|(needs_relocation, _, package)| relocate::Item {
                package,
                needs_relocation: *needs_relocation,
            }),
    )?;
    let replacements: Vec<_> = staged
        .into_iter()
        .map(|(_, old, package)| (old, package))
        .collect();
    let count = replacements.len();
    store.upgrade(replacements)?;
    println!(
        "\n{} {count} {}.",
        formatter.success("Upgraded"),
        if count == 1 { "formula" } else { "formulae" }
    );
    Ok(())
}

/// Report the native packages that have a newer bottle available.
///
/// This is the read-only counterpart of [`upgrade`]. It binds the prefix
/// without creating or modifying it, resolves the same plan, applies the same
/// eligibility and dependency-pin checks, and prints the same compact
/// `Package`/`Installed`/`Available` table as the upgrade preview. It never
/// downloads a bottle, stages a keg, writes a file, or invokes Homebrew.
///
/// Returns `true` when at least one upgrade is available. An empty prefix, or
/// one whose packages are all current, reports that and returns `false`.
pub(crate) async fn check_updates(config: &Config, prefix: Option<&Path>) -> Result<bool> {
    if std::env::consts::OS != "macos" {
        bail!("Native upgrades are supported on macOS only");
    }
    let path = prefix_path(prefix)?;
    let store = Store::open(&path, false)?;
    let installed = store.installed()?;
    if installed.is_empty() {
        println!("No brewrs-managed packages to upgrade.");
        return Ok(false);
    }
    let retained = store.retained()?;

    let (tag, changes) = resolve_changes(config, &installed, &installed, &retained).await?;
    let formatter = DnfStyleFormatter::new(config);
    if changes.is_empty() {
        println!("All brewrs-managed packages are up to date.");
        return Ok(false);
    }
    render_upgrade_preview(&formatter, "Native updates", &tag, store.prefix(), &changes);
    println!(
        "\n{} {} available.",
        formatter.success(&changes.len().to_string()),
        if changes.len() == 1 {
            "update"
        } else {
            "updates"
        }
    );
    Ok(true)
}

/// Resolve the current bottle plan for `selected` and reduce it to upgrades.
///
/// Shared by [`upgrade`] and [`check_updates`] so both commands plan exactly
/// once and agree on every accepted or rejected change.
async fn resolve_changes(
    config: &Config,
    installed: &[Receipt],
    selected: &[Receipt],
    retained: &[Receipt],
) -> Result<(String, Vec<UpgradeChange>)> {
    let targets: Vec<_> = selected
        .iter()
        .map(|receipt| receipt.name.clone())
        .collect();
    let tag = default_target()?;
    let plan = plan::resolve(config, &targets, &tag).await?;
    let changes = planned_changes(&plan, installed, selected, retained)?;
    Ok((tag, changes))
}

/// Reduce an already-resolved plan to the ordered changes it would apply.
///
/// Eligibility is decided by [`upgrade_plan`]; the projected receipt graph is
/// then validated so neither the preview nor a check reports an upgrade that
/// would break an installed dependency pin. A pin to the exact old version and
/// digest of a replaced package is carried by retaining that old keg, exactly
/// as the store applies it, so the preview agrees with the applied upgrade.
fn planned_changes(
    plan: &[PlannedFormula],
    installed: &[Receipt],
    selected: &[Receipt],
    retained: &[Receipt],
) -> Result<Vec<UpgradeChange>> {
    let changes: Vec<_> = upgrade_plan(plan, selected)?
        .into_iter()
        .map(|(old, candidate)| UpgradeChange {
            old,
            new: candidate.receipt,
            formula: candidate.formula.clone(),
        })
        .collect();
    let survivors: Vec<Receipt> = installed
        .iter()
        .filter(|receipt| !changes.iter().any(|change| change.old.name == receipt.name))
        .cloned()
        .collect();
    let pinned_by = |target: &Receipt, owners: &[Receipt]| {
        owners.iter().any(|receipt| {
            receipt.dependencies.iter().any(|pin| {
                pin.name == target.name
                    && pin.version == target.version
                    && pin.sha256 == target.sha256
            })
        })
    };
    // Retention candidates mirror the store's decision: surviving active
    // receipts plus every retained record on disk force retention, and the
    // transitive closure a retained old receipt's own pins force.
    let mut owners = survivors.clone();
    owners.extend(retained.iter().cloned());
    let mut new_retained: Vec<Receipt> = changes
        .iter()
        .filter(|change| pinned_by(&change.old, &owners))
        .map(|change| change.old.clone())
        .collect();
    loop {
        let mut changed = false;
        for change in &changes {
            if new_retained.contains(&change.old) {
                continue;
            }
            if pinned_by(&change.old, &new_retained) {
                new_retained.push(change.old.clone());
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    let mut all_retained = retained.to_vec();
    all_retained.extend(new_retained);
    let replacements: Vec<Receipt> = changes.iter().map(|change| change.new.clone()).collect();
    validate_dependencies_with_retained(&survivors, &replacements, &all_retained).context(
        "Upgrade would leave unmet dependency pins; select compatible updates together or install missing requirements first",
    )?;
    let mut projected = installed.to_vec();
    for change in &changes {
        let receipt = projected
            .iter_mut()
            .find(|entry| entry.name == change.old.name)
            .context("An upgrade target was missing from the installed snapshot")?;
        *receipt = change.new.clone();
    }
    validate_retained_pin_closure(&all_retained, &projected, &all_retained)?;
    validate_conflicts(&projected)?;
    Ok(changes)
}

/// Print the compact upgrade preview shared by `upgrade` and `check_updates`.
fn render_upgrade_preview(
    formatter: &DnfStyleFormatter,
    title: &str,
    tag: &str,
    prefix: &Path,
    changes: &[UpgradeChange],
) {
    println!("{}  {}", formatter.section(title), formatter.info(tag));
    println!(
        "{}",
        formatter.table(
            &["Package", "Installed", "Available"],
            &upgrade_rows(changes)
        )
    );
    println!(
        "{}",
        formatter.field("Prefix", &prefix.display().to_string())
    );
}

fn upgrade_rows(changes: &[UpgradeChange]) -> Vec<Vec<String>> {
    changes
        .iter()
        .map(|change| {
            vec![
                change.old.name.clone(),
                change.old.version.clone(),
                change.new.version.clone(),
            ]
        })
        .collect()
}

/// Render a digest prefix for conflict messages, keeping full digests out of
/// the terminal while still naming each side of the conflict.
fn short_digest(digest: &str) -> &str {
    digest.get(..12).unwrap_or(digest)
}

fn upgrade_plan<'a>(
    plan: &'a [PlannedFormula],
    installed: &[Receipt],
) -> Result<Vec<(Receipt, Candidate<'a>)>> {
    let mut changes = Vec::new();
    for old in installed {
        let new = plan
            .iter()
            .find(|formula| formula.name == old.name && formula.requested)
            .with_context(|| {
                format!(
                    "No current bottle was resolved for installed formula '{}'",
                    old.name
                )
            })?;
        // An unchanged bottle keeps its original pins. The projected graph
        // still rejects upgrading any library that this bottle depends on.
        if new.version_scheme == Some(old.version_scheme)
            && new.version == old.version
            && new.sha256 == old.sha256
        {
            continue;
        }
        let Some(new_scheme) = new.version_scheme else {
            bail!(
                "Cannot upgrade {} natively: {}",
                old.name,
                if new.install_blockers.is_empty() {
                    "unknown version scheme".into()
                } else {
                    new.install_blockers.join("; ")
                }
            );
        };
        if !new.install_blockers.is_empty() {
            bail!(
                "Cannot upgrade {} natively: {}",
                old.name,
                new.install_blockers.join("; ")
            );
        }
        let receipt = receipt_for(new, plan, old.requested)?;
        let ordering = match new_scheme.cmp(&old.version_scheme) {
            Ordering::Equal => super::version::compare(&new.version, &old.version)
                .with_context(|| format!("Cannot compare native versions for '{}'", old.name))?,
            other => other,
        };
        match ordering {
            Ordering::Greater => changes.push((
                old.clone(),
                Candidate {
                    formula: new,
                    receipt,
                },
            )),
            Ordering::Less => bail!(
                "Refusing to downgrade {} from installed {} (bottle {}) to available {} (bottle {})",
                old.name,
                old.version,
                short_digest(&old.sha256),
                new.version,
                short_digest(&new.sha256)
            ),
            Ordering::Equal => bail!("{}", same_version_conflict(old, new)),
        }
    }
    Ok(changes)
}

/// Explain why an installed formula cannot be replaced by a bottle that
/// compares equal to its version, naming the installed and available pins.
fn same_version_conflict(old: &Receipt, new: &PlannedFormula) -> String {
    if old.version == new.version {
        format!(
            "{} {} is installed from bottle {}, but the available bottle for the same version is {}; refusing to replace it in place",
            old.name,
            old.version,
            short_digest(&old.sha256),
            short_digest(&new.sha256)
        )
    } else {
        format!(
            "{} {} is installed from bottle {}, but the available version {} is an equivalent spelling of the same version and resolves bottle {}; refusing to replace it in place",
            old.name,
            old.version,
            short_digest(&old.sha256),
            new.version,
            short_digest(&new.sha256)
        )
    }
}

pub(crate) fn remove(
    config: &Config,
    prefix: Option<&Path>,
    packages: &[String],
    dry_run: bool,
    yes: bool,
    autoremove: bool,
) -> Result<()> {
    let store = Store::open(&prefix_path(prefix)?, false)?;
    let installed = store.installed()?;
    let retained = store.retained()?;
    let pending = planned_removals(packages, &installed, autoremove, &retained)?;
    apply_removal(config, &store, &pending, dry_run, yes, "Remove packages")
}

fn planned_removals(
    packages: &[String],
    installed: &[Receipt],
    autoremove: bool,
    retained: &[Receipt],
) -> Result<Vec<Receipt>> {
    let mut pending = removal_plan(packages, installed)?;
    let names: HashSet<_> = pending
        .iter()
        .map(|receipt| receipt.name.as_str())
        .collect();
    let mut remaining: Vec<_> = installed
        .iter()
        .filter(|entry| !names.contains(entry.name.as_str()))
        .cloned()
        .collect();
    validate_dependencies_with_retained(&remaining, &[], retained)
        .context("Removal would leave unmet dependencies")?;
    if autoremove {
        validate_dependencies_with_retained(installed, &[], retained)
            .context("Cannot determine dependency ownership safely")?;
        let previously_needed = reachable_packages(installed, |entry| entry.requested);
        // Keep previously unused packages and whatever they still need.
        let still_needed: HashSet<String> = reachable_packages(&remaining, |entry| {
            entry.requested || !previously_needed.contains(entry.name.as_str())
        })
        .into_iter()
        .map(str::to_owned)
        .collect();
        pending.extend(
            remaining
                .iter()
                .filter(|entry| !still_needed.contains(entry.name.as_str()))
                .cloned(),
        );
        remaining.retain(|entry| still_needed.contains(entry.name.as_str()));
    }
    validate_retained_pin_closure(retained, &remaining, retained)
        .context("Removal would strand a retained keg's recorded dependency")?;
    Ok(pending)
}

pub(crate) fn autoremove(
    config: &Config,
    prefix: Option<&Path>,
    dry_run: bool,
    yes: bool,
) -> Result<()> {
    let store = Store::open(&prefix_path(prefix)?, false)?;
    let installed = store.installed()?;
    let retained = store.retained()?;
    let pending = autoremove_plan(&installed, &retained)?;
    apply_removal(
        config,
        &store,
        &pending,
        dry_run,
        yes,
        "Remove unused dependencies",
    )
}

fn autoremove_plan(installed: &[Receipt], retained: &[Receipt]) -> Result<Vec<Receipt>> {
    validate_dependencies_with_retained(installed, &[], retained)
        .context("Cannot determine dependency ownership safely")?;
    let needed = reachable_packages(installed, |entry| entry.requested);
    let pending: Vec<Receipt> = installed
        .iter()
        .filter(|entry| !needed.contains(entry.name.as_str()))
        .cloned()
        .collect();
    let remaining: Vec<Receipt> = installed
        .iter()
        .filter(|entry| needed.contains(entry.name.as_str()))
        .cloned()
        .collect();
    validate_retained_pin_closure(retained, &remaining, retained)
        .context("Autoremove would strand a retained keg's recorded dependency")?;
    Ok(pending)
}

// Exact pins are validated before the walk; visit each node at most once.
//
// The walk traverses the *active* receipt set only. A pin whose name has no
// active receipt (it is satisfied by a retained keg) has nothing to visit, so
// it is skipped instead of panicking or pretending a retained keg is a
// removable active package.
fn reachable_packages(
    installed: &[Receipt],
    keep_root: impl Fn(&Receipt) -> bool,
) -> HashSet<&str> {
    let by_name: HashMap<_, _> = installed
        .iter()
        .map(|entry| (entry.name.as_str(), entry))
        .collect();
    let mut to_visit: Vec<_> = installed.iter().filter(|entry| keep_root(entry)).collect();
    let mut needed = HashSet::new();
    while let Some(entry) = to_visit.pop() {
        if needed.insert(entry.name.as_str()) {
            to_visit.extend(
                entry
                    .dependencies
                    .iter()
                    .filter_map(|dep| by_name.get(dep.name.as_str()).copied()),
            );
        }
    }
    needed
}

fn apply_removal(
    config: &Config,
    store: &Store,
    pending: &[Receipt],
    dry_run: bool,
    yes: bool,
    title: &str,
) -> Result<()> {
    if pending.is_empty() {
        println!("No unused dependencies to remove.");
        return Ok(());
    }
    let formatter = DnfStyleFormatter::new(config);
    let rows: Vec<_> = pending
        .iter()
        .map(|receipt| {
            vec![
                receipt.name.clone(),
                receipt.version.clone(),
                "Remove".into(),
            ]
        })
        .collect();
    println!("{}", formatter.section(title));
    println!(
        "{}",
        formatter.table(&["Package", "Version", "Action"], &rows)
    );
    println!(
        "{}",
        formatter.field("Prefix", &store.prefix().display().to_string())
    );
    if dry_run {
        println!("\nNo packages changed.");
        return Ok(());
    }
    io::stdout().flush()?;
    if config.general.confirm_operations && !yes && !confirm()? {
        println!("Cancelled. No packages changed.");
        return Ok(());
    }
    store.remove(pending)?;
    println!(
        "\n{} {} {}.",
        formatter.success("Removed"),
        pending.len(),
        if pending.len() == 1 {
            "formula"
        } else {
            "formulae"
        }
    );
    Ok(())
}

fn removal_plan(packages: &[String], installed: &[Receipt]) -> Result<Vec<Receipt>> {
    let mut selected = std::collections::HashSet::new();
    let mut pending = Vec::new();
    for target in packages {
        let name = plan::parse_target(target)?;
        if selected.insert(name) {
            let receipt = installed
                .iter()
                .find(|entry| entry.name == name)
                .with_context(|| format!("'{name}' is not installed in this brewrs prefix"))?;
            pending.push(receipt.clone());
        }
    }
    Ok(pending)
}

pub(crate) fn list(config: &Config, prefix: Option<&Path>) -> Result<()> {
    let store = Store::open(&prefix_path(prefix)?, false)?;
    let receipts = store.installed()?;
    if receipts.is_empty() {
        println!(
            "No brewrs-managed packages in {}.",
            store.prefix().display()
        );
        return Ok(());
    }
    let formatter = DnfStyleFormatter::new(config);
    render_formula_list(&formatter, receipts);
    Ok(())
}

pub(crate) fn list_all(
    config: &Config,
    prefix: Option<&Path>,
    formula_only: bool,
    cask_only: bool,
) -> Result<()> {
    if cask_only {
        return super::cask_install::list(config, prefix);
    }
    if formula_only {
        return list(config, prefix);
    }
    let store = Store::open(&prefix_path(prefix)?, false)?;
    let formulae = store.installed()?;
    let casks = super::cask_store::installed(&store)?;
    if formulae.is_empty() && casks.is_empty() {
        println!(
            "No brewrs-managed packages in {}.",
            store.prefix().display()
        );
        return Ok(());
    }
    let formatter = DnfStyleFormatter::new(config);
    let has_formulae = !formulae.is_empty();
    if has_formulae {
        render_formula_list(&formatter, formulae);
    }
    if !casks.is_empty() {
        if has_formulae {
            println!();
        }
        super::cask_install::render_cask_list(&formatter, &casks);
    }
    Ok(())
}

pub(crate) async fn check_all_updates(
    config: &Config,
    prefix: Option<&Path>,
    formula_only: bool,
    cask_only: bool,
    greedy: bool,
) -> Result<bool> {
    if cask_only {
        return super::cask_install::check_updates(config, prefix, greedy, false).await;
    }
    if formula_only {
        return check_updates(config, prefix).await;
    }
    let store = Store::open(&prefix_path(prefix)?, false)?;
    let formulae = store.installed()?;
    let casks = super::cask_store::installed(&store)?;
    if formulae.is_empty() && casks.is_empty() {
        println!("No brewrs-managed packages to upgrade.");
        return Ok(false);
    }
    let mut has_updates = false;
    if !formulae.is_empty() {
        has_updates |= check_updates(config, prefix).await?;
    }
    if !casks.is_empty() {
        has_updates |=
            super::cask_install::check_updates(config, prefix, greedy, !formulae.is_empty())
                .await?;
    }
    Ok(has_updates)
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn upgrade_all(
    config: &Config,
    prefix: Option<&Path>,
    packages: &[String],
    formula_only: bool,
    cask_only: bool,
    greedy: bool,
    dry_run: bool,
    yes: bool,
) -> Result<()> {
    if cask_only {
        return super::cask_install::upgrade(config, prefix, packages, greedy, dry_run, yes).await;
    }
    if formula_only {
        return upgrade(config, prefix, packages, dry_run, yes).await;
    }
    let path = prefix_path(prefix)?;
    let store = Store::open(&path, false)?;
    let installed_formulae = store.installed()?;
    let installed_casks = super::cask_store::installed(&store)?;
    if packages.is_empty() {
        if installed_formulae.is_empty() && installed_casks.is_empty() {
            println!("No brewrs-managed packages to upgrade.");
            return Ok(());
        }
        if !installed_formulae.is_empty() {
            upgrade(config, prefix, &[], dry_run, yes).await?;
        }
        if !installed_casks.is_empty() {
            super::cask_install::upgrade(config, prefix, &[], greedy, dry_run, yes).await?;
        }
        return Ok(());
    }

    let mut formula_targets = Vec::new();
    let mut cask_targets = Vec::new();
    for raw in packages {
        if raw.starts_with("homebrew/core/") {
            formula_targets.push(raw.clone());
        } else if raw.starts_with("homebrew/cask/") {
            cask_targets.push(raw.clone());
        } else {
            let formula_name = plan::parse_target(raw);
            let cask_token = super::cask_plan::parse_cask_target(raw);
            let is_formula = formula_name
                .as_ref()
                .is_ok_and(|name| installed_formulae.iter().any(|r| r.name == *name));
            let is_cask = cask_token
                .as_ref()
                .is_ok_and(|token| installed_casks.iter().any(|r| r.token == *token));
            match (is_formula, is_cask) {
                (true, true) => bail!(
                    "'{raw}' is installed as both a formula and a cask; pass --formula or --cask"
                ),
                (true, false) => formula_targets.push(raw.clone()),
                (false, true) => cask_targets.push(raw.clone()),
                (false, false) => {
                    formula_name?;
                    bail!("'{raw}' is not installed in this brewrs prefix");
                }
            }
        }
    }

    if !formula_targets.is_empty() {
        upgrade(config, prefix, &formula_targets, dry_run, yes).await?;
    }
    if !cask_targets.is_empty() {
        super::cask_install::upgrade(config, prefix, &cask_targets, greedy, dry_run, yes).await?;
    }
    Ok(())
}

fn render_formula_list(formatter: &DnfStyleFormatter, mut receipts: Vec<Receipt>) {
    receipts.sort_by(|left, right| left.name.cmp(&right.name));
    let rows: Vec<_> = receipts
        .into_iter()
        .map(|receipt| {
            vec![
                receipt.name,
                receipt.version,
                if receipt.requested {
                    "Requested"
                } else {
                    "Dependency"
                }
                .into(),
            ]
        })
        .collect();
    println!("{}", formatter.section("Native packages"));
    println!(
        "{}",
        formatter.table(&["Package", "Version", "Reason"], &rows)
    );
}

fn pending_packages<'a>(
    plan: &'a [PlannedFormula],
    installed: &[Receipt],
    retained: &[Receipt],
) -> Result<Vec<Candidate<'a>>> {
    let mut pending = Vec::new();
    let mut projected = installed.to_vec();
    for formula in plan {
        if let Some(existing) = installed
            .iter()
            .find(|receipt| receipt.name == formula.name)
        {
            let receipt = receipt_for(formula, plan, existing.requested)?;
            if receipt == *existing {
                continue;
            }
            bail!("{}", replacement_conflict(existing, &receipt));
        }
        if !formula.install_blockers.is_empty() {
            bail!(
                "Cannot install {} natively: {}",
                formula.name,
                formula.install_blockers.join("; ")
            );
        }
        let receipt = receipt_for(formula, plan, formula.requested)?;
        projected.push(receipt.clone());
        pending.push(Candidate { formula, receipt });
    }
    let newcomers: Vec<Receipt> = pending.iter().map(|entry| entry.receipt.clone()).collect();
    validate_dependencies_with_retained(installed, &newcomers, retained)?;
    validate_conflicts(&projected)?;
    Ok(pending)
}

/// Explain why `install` refuses to replace an installed package in place,
/// naming the installed and available versions and bottle digests instead of
/// lumping them into one vague "different version, bottle, or dependencies".
fn replacement_conflict(installed: &Receipt, planned: &Receipt) -> String {
    if installed.version_scheme != planned.version_scheme {
        if planned.version_scheme > installed.version_scheme {
            format!(
                "{} {} (scheme {}) is installed but {} {} (scheme {}) is available; run native upgrade to replace the installed version",
                installed.name,
                installed.version,
                installed.version_scheme,
                planned.name,
                planned.version,
                planned.version_scheme
            )
        } else {
            format!(
                "{} {} (scheme {}) is installed but the plan resolves older {} {} (scheme {}); refusing to replace it in place",
                installed.name,
                installed.version,
                installed.version_scheme,
                planned.name,
                planned.version,
                planned.version_scheme
            )
        }
    } else if installed.version != planned.version {
        match super::version::compare(&planned.version, &installed.version) {
            Ok(Ordering::Greater) => format!(
                "{} {} is installed but {} {} is available; run native upgrade to replace the installed version",
                installed.name, installed.version, planned.name, planned.version
            ),
            Ok(Ordering::Less) => format!(
                "{} {} is installed but the plan resolves older {} {}; refusing to replace it in place",
                installed.name, installed.version, planned.name, planned.version
            ),
            _ => format!(
                "{} {} is installed but the plan resolves {} {}; refusing to replace it in place",
                installed.name, installed.version, planned.name, planned.version
            ),
        }
    } else if installed.sha256 != planned.sha256 {
        format!(
            "{} {} is installed from bottle {}, but the plan resolves bottle {} at the same version; refusing to replace it",
            installed.name,
            installed.version,
            short_digest(&installed.sha256),
            short_digest(&planned.sha256)
        )
    } else {
        format!(
            "{} {} is installed with different dependency pins than the plan resolves; refusing to replace it in place",
            installed.name, installed.version
        )
    }
}

pub(super) fn prefix_path(prefix: Option<&Path>) -> Result<PathBuf> {
    if let Some(path) = prefix {
        return Ok(path.to_path_buf());
    }
    dirs::data_local_dir()
        .map(|path| path.join("brew-rs").join("native"))
        .context("Cannot determine the native prefix; pass --prefix with an absolute path")
}

/// Print shell environment exports for the selected brewrs prefix without
/// creating or modifying any files.
pub(crate) fn shellenv(prefix: Option<&Path>, shell: Option<&str>) -> Result<()> {
    let path = prefix_path(prefix)?;
    let script = render_shellenv(&path, shell, std::env::var_os("SHELL").as_deref())?;
    print!("{script}");
    Ok(())
}

fn render_shellenv(
    prefix: &Path,
    shell: Option<&str>,
    shell_env: Option<&std::ffi::OsStr>,
) -> Result<String> {
    let raw = prefix
        .to_str()
        .context("brewrs prefix must be valid UTF-8 for shellenv")?;
    if raw.is_empty() {
        bail!("refusing an empty brewrs prefix");
    }
    if !prefix.is_absolute() {
        bail!(
            "refusing relative brewrs prefix '{}'; use an absolute path",
            prefix.display()
        );
    }
    if raw.bytes().any(|byte| byte < 0x20 || byte == 0x7f) {
        bail!("refusing brewrs prefix with control characters for shellenv");
    }
    let normalized = if raw.len() > 1 {
        raw.trim_end_matches('/')
    } else {
        raw
    };
    let normalized = if normalized.is_empty() {
        "/"
    } else {
        normalized
    };
    let bin = if normalized == "/" {
        "/bin".to_string()
    } else {
        format!("{normalized}/bin")
    };
    let sbin = if normalized == "/" {
        "/sbin".to_string()
    } else {
        format!("{normalized}/sbin")
    };

    let is_fish = match shell {
        Some("fish") => true,
        Some(_) => false,
        None => {
            shell_env
                .and_then(|value| Path::new(value).file_name())
                .and_then(|name| name.to_str())
                == Some("fish")
        }
    };

    if is_fish {
        Ok(format!(
            "set -gx BREWRS_PREFIX {};\nfish_add_path -gP {} {};\n",
            fish_quote(normalized),
            fish_quote(&bin),
            fish_quote(&sbin),
        ))
    } else {
        let path_entries = format!("{bin}:{sbin}");
        Ok(format!(
            "export BREWRS_PREFIX={};\nexport PATH={}:\"$PATH\";\n",
            posix_quote(normalized),
            posix_quote(&path_entries),
        ))
    }
}

fn posix_quote(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('\'');
    for ch in value.chars() {
        if ch == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

fn fish_quote(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('\'');
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '\'' => out.push_str("\\'"),
            _ => out.push(ch),
        }
    }
    out.push('\'');
    out
}

pub(super) fn confirm() -> Result<bool> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() || !io::stderr().is_terminal() {
        bail!("Confirmation requires a visible terminal; pass --assumeyes or --dry-run");
    }
    eprint!("Apply this plan? [y/N] ");
    io::stderr().flush()?;
    let mut answer = String::new();
    io::stdin().read_line(&mut answer)?;
    Ok(matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn formula() -> PlannedFormula {
        PlannedFormula {
            name: "hello".into(),
            version: "1.2".into(),
            version_scheme: Some(0),
            keg_only: false,
            has_service: false,
            conflicts_with: Vec::new(),
            requested: true,
            url: String::new(),
            sha256: "a".repeat(64),
            cellar: ":any_skip_relocation".into(),
            install_blockers: Vec::new(),
            dependencies: Vec::new(),
        }
    }

    #[test]
    fn preview_and_apply_share_eligibility_checks() {
        let mut package = formula();
        package.install_blockers.push("has a post-install hook");
        let plan = [package];
        assert!(
            pending_packages(&plan, &[], &[])
                .unwrap_err()
                .to_string()
                .contains("post-install")
        );
    }

    fn installed_hello(version: &str) -> Receipt {
        Receipt {
            name: "hello".into(),
            version: version.into(),
            version_scheme: 0,
            sha256: "a".repeat(64),
            requested: true,
            keg_only: false,
            conflicts_with: Vec::new(),
            dependencies: Vec::new(),
        }
    }

    fn package_receipt(name: &str, requested: bool, dependencies: &[&str]) -> Receipt {
        Receipt {
            name: name.into(),
            requested,
            dependencies: dependencies
                .iter()
                .map(|name| Dependency {
                    name: (*name).into(),
                    version: "1.2".into(),
                    sha256: "a".repeat(64),
                })
                .collect(),
            ..installed_hello("1.2")
        }
    }

    #[test]
    fn autoremove_keeps_the_full_requested_closure() {
        let installed = [
            package_receipt("app", true, &["middle"]),
            package_receipt("middle", false, &["leaf"]),
            package_receipt("leaf", false, &[]),
            package_receipt("unused", false, &["leaf"]),
        ];
        assert_eq!(
            autoremove_plan(&installed, &[]).unwrap(),
            [installed[3].clone()]
        );
        assert!(autoremove_plan(&[], &[]).unwrap().is_empty());
        assert!(autoremove_plan(&installed[..2], &[]).is_err());
    }

    #[test]
    fn combined_removal_leaves_existing_orphans_and_their_dependencies() {
        let installed = [
            package_receipt("app", true, &["shared", "sole"]),
            package_receipt("old-orphan", false, &["shared"]),
            package_receipt("shared", false, &[]),
            package_receipt("sole", false, &[]),
        ];
        let targets = ["app".into()];
        assert_eq!(
            planned_removals(&targets, &installed, true, &[]).unwrap(),
            [installed[0].clone(), installed[3].clone()]
        );
        assert_eq!(
            planned_removals(&targets, &installed, false, &[]).unwrap(),
            [installed[0].clone()]
        );
        assert!(planned_removals(&["shared".into()], &installed, true, &[]).is_err());
    }

    #[test]
    fn explicitly_installing_a_dependency_plans_a_promotion_without_a_download() {
        let installed = [package_receipt("hello", false, &[])];
        let mut plan = [formula()];
        assert!(pending_packages(&plan, &installed, &[]).unwrap().is_empty());
        assert_eq!(requested_promotions(&plan, &installed), installed);
        plan[0].requested = false;
        assert!(requested_promotions(&plan, &installed).is_empty());
    }

    #[test]
    fn upgrade_only_selects_strictly_newer_compatible_bottles() {
        let mut formula = formula();
        let mut old = [installed_hello("1.2")];
        assert!(upgrade_plan(&[formula.clone()], &old).unwrap().is_empty());
        formula.version = "1.2_1".into();
        formula.sha256 = "b".repeat(64);
        let planned = [formula.clone()];
        let changes = upgrade_plan(&planned, &old).unwrap();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].0.version, "1.2");
        assert_eq!(changes[0].1.receipt.version, "1.2_1");

        // Higher version_scheme upgrades even when numeric version resets.
        formula.version_scheme = Some(1);
        formula.version = "1.0".into();
        let scheme_planned = [formula.clone()];
        let scheme_upgrade = upgrade_plan(&scheme_planned, &old).unwrap();
        assert_eq!(scheme_upgrade.len(), 1);
        assert_eq!(scheme_upgrade[0].1.receipt.version_scheme, 1);

        // Lower version_scheme is rejected as a downgrade.
        old[0].version_scheme = 1;
        old[0].version = "1.0".into();
        formula.version_scheme = Some(0);
        formula.version = "2025.0".into();
        assert!(
            upgrade_plan(&[formula.clone()], &old)
                .unwrap_err()
                .to_string()
                .contains("downgrade")
        );

        // Unknown version_scheme is rejected.
        old[0].version_scheme = 0;
        formula.version_scheme = None;
        assert!(
            upgrade_plan(&[formula.clone()], &old)
                .unwrap_err()
                .to_string()
                .contains("version scheme")
        );
        formula.version_scheme = Some(0);
        formula.install_blockers.push("needs relocation");
        assert!(
            upgrade_plan(&[formula], &old)
                .unwrap_err()
                .to_string()
                .contains("needs relocation")
        );
    }

    #[test]
    fn installing_a_dependency_records_its_exact_bottle_identity() {
        let mut app = formula();
        app.dependencies.push("library".into());
        let mut library = formula();
        library.name = "library".into();
        library.requested = false;
        library.sha256 = "c".repeat(64);

        assert!(pending_packages(&[app.clone()], &[], &[]).is_err());
        let plan = [library.clone(), app];
        let pending = pending_packages(&plan, &[], &[]).unwrap();
        assert_eq!(pending.len(), 2);
        assert!(!pending[0].receipt.requested);
        assert_eq!(
            pending[1].receipt.dependencies,
            [Dependency {
                name: library.name,
                version: library.version,
                sha256: library.sha256,
            }]
        );
        let already_installed = [pending[0].receipt.clone()];
        let pending = pending_packages(&plan, &already_installed, &[]).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].receipt.name, "hello");
    }

    #[test]
    fn unchanged_bottle_keeps_its_recorded_dependency_pin_during_upgrade() {
        let mut app = formula();
        app.dependencies.push("library".into());
        let mut library = formula();
        library.name = "library".into();
        let mut old_library = installed_hello("1.1");
        old_library.name = "library".into();
        let mut old_app = installed_hello("1.2");
        old_app.dependencies.push(Dependency {
            name: old_library.name.clone(),
            version: old_library.version.clone(),
            sha256: old_library.sha256.clone(),
        });
        let plan = [library, app];
        let installed = [old_library, old_app];
        assert!(upgrade_plan(&plan, &installed[1..]).unwrap().is_empty());
        let changes = upgrade_plan(&plan, &installed).unwrap();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].1.receipt.name, "library");
        assert!(
            crate::native::store::validate_dependencies(&[
                changes[0].1.receipt.clone(),
                installed[1].clone()
            ])
            .is_err()
        );
    }

    #[test]
    fn upgrade_rejects_equal_version_replacements_and_downgrades() {
        let old = [installed_hello("1.2")];
        let mut formula = formula();
        formula.sha256 = "b".repeat(64);
        assert!(
            upgrade_plan(&[formula.clone()], &old)
                .unwrap_err()
                .to_string()
                .contains("same version")
        );
        formula.version = "1.2.0".into();
        assert!(
            upgrade_plan(&[formula.clone()], &old)
                .unwrap_err()
                .to_string()
                .contains("same version")
        );
        formula.version = "1.1".into();
        assert!(
            upgrade_plan(&[formula.clone()], &old)
                .unwrap_err()
                .to_string()
                .contains("downgrade")
        );
        formula.version = "1.3-beta".into();
        assert!(
            upgrade_plan(&[formula], &old)
                .unwrap_err()
                .to_string()
                .contains("Cannot compare")
        );
    }

    #[test]
    fn reinstall_is_a_noop_only_for_the_same_version_and_digest() {
        let plan = [formula()];
        let mut receipt = Receipt {
            name: "hello".into(),
            version: "1.2".into(),
            version_scheme: 0,
            sha256: "a".repeat(64),
            requested: true,
            keg_only: false,
            conflicts_with: Vec::new(),
            dependencies: Vec::new(),
        };
        assert!(
            pending_packages(&plan, std::slice::from_ref(&receipt), &[])
                .unwrap()
                .is_empty()
        );
        receipt.sha256 = "b".repeat(64);
        assert!(pending_packages(&plan, &[receipt], &[]).is_err());
    }

    fn receipt(name: &str, version: &str, digest: char) -> Receipt {
        Receipt {
            name: name.into(),
            version: version.into(),
            version_scheme: 0,
            sha256: digest.to_string().repeat(64),
            requested: true,
            keg_only: false,
            conflicts_with: Vec::new(),
            dependencies: Vec::new(),
        }
    }

    #[test]
    fn check_updates_and_upgrade_share_the_same_change_plan() {
        let current = [receipt("hello", "1.2", 'a')];
        let mut available = formula();
        assert!(
            planned_changes(&[available.clone()], &current, &current, &[])
                .unwrap()
                .is_empty()
        );
        assert!(
            upgrade_plan(&[available.clone()], &current)
                .unwrap()
                .is_empty()
        );

        available.version = "1.3".into();
        available.sha256 = "b".repeat(64);
        let changes = planned_changes(&[available], &current, &current, &[]).unwrap();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].old.name, "hello");
        assert_eq!(changes[0].old.version, "1.2");
        assert_eq!(changes[0].new.version, "1.3");
        assert_eq!(changes[0].formula.version, "1.3");
        assert_eq!(
            upgrade_rows(&changes),
            [vec![
                "hello".to_string(),
                "1.2".to_string(),
                "1.3".to_string()
            ]]
        );
    }

    #[test]
    fn a_same_version_bottle_change_names_both_installed_and_available_pins() {
        let current = [receipt("hello", "1.2", 'a')];
        let mut available = formula();
        available.sha256 = "b".repeat(64);
        let message = planned_changes(&[available], &current, &current, &[])
            .unwrap_err()
            .to_string();
        assert!(message.contains("same version"), "{message}");
        assert!(message.contains("1.2"), "{message}");
        assert!(message.contains("aaaaaaaaaaaa"), "{message}");
        assert!(message.contains("bbbbbbbbbbbb"), "{message}");
    }

    #[test]
    fn install_explains_newer_versions_and_differing_pins() {
        let current = [receipt("hello", "1.2", 'a')];
        let mut available = formula();
        available.version = "1.3".into();
        available.sha256 = "b".repeat(64);
        let message = pending_packages(&[available], &current, &[])
            .unwrap_err()
            .to_string();
        assert!(message.contains("hello 1.2 is installed"), "{message}");
        assert!(message.contains("1.3 is available"), "{message}");
        assert!(message.contains("native upgrade"), "{message}");

        let mut available = formula();
        available.sha256 = "b".repeat(64);
        let message = pending_packages(&[available], &current, &[])
            .unwrap_err()
            .to_string();
        assert!(message.contains("same version"), "{message}");
        assert!(message.contains("aaaaaaaaaaaa"), "{message}");
        assert!(message.contains("bbbbbbbbbbbb"), "{message}");

        // A receipt written before dependency pins existed records none, while
        // the plan now resolves a dependency for the identical bottle.
        let mut available = formula();
        available.dependencies.push("library".into());
        let mut library = formula();
        library.name = "library".into();
        library.requested = false;
        let message = pending_packages(&[library, available], &current, &[])
            .unwrap_err()
            .to_string();
        assert!(message.contains("hello 1.2"), "{message}");
        assert!(message.contains("dependency pins"), "{message}");
    }

    #[test]
    fn upgrade_preview_accepts_retaining_an_exact_old_pin() {
        let mut library = formula();
        library.name = "library".into();
        library.version = "1.2".into();
        library.sha256 = "d".repeat(64);
        let mut app = formula();
        app.dependencies.push("library".into());

        let old_library = receipt("library", "1.1", 'c');
        let mut old_app = receipt("hello", "1.2", 'a');
        old_app.dependencies.push(Dependency {
            name: "library".into(),
            version: "1.1".into(),
            sha256: "c".repeat(64),
        });
        let mut installed = [old_library, old_app];
        let plan = [library, app];

        let changes = planned_changes(&plan, &installed, &installed, &[]).unwrap();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].old.version, "1.1");
        assert_eq!(changes[0].new.version, "1.2");
        assert_eq!(installed[1].dependencies[0].sha256, "c".repeat(64));

        installed[1].dependencies[0].sha256 = "e".repeat(64);
        let error = planned_changes(&plan, &installed, &installed, &[]).unwrap_err();
        assert!(format!("{error:#}").contains("unmet dependency pins"));
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn check_updates_is_read_only_and_absent_for_an_empty_prefix() {
        let home = tempfile::tempdir().unwrap();
        let prefix = home.path().join("native");
        let updates = check_updates(&Config::default(), Some(&prefix))
            .await
            .unwrap();
        assert!(!updates);
        assert!(!prefix.exists());
    }

    #[test]
    fn install_and_upgrade_reject_symmetric_conflicts() {
        let mut app = formula();
        app.name = "app".into();
        app.conflicts_with = vec!["other".into()];
        let mut other = formula();
        other.name = "other".into();

        // Conflict within one install batch.
        assert!(
            pending_packages(&[app.clone(), other.clone()], &[], &[])
                .unwrap_err()
                .to_string()
                .contains("conflicts with")
        );

        // Installed package declares conflict with newly requested package.
        let mut installed_app = receipt("app", "1.2", 'a');
        installed_app.conflicts_with = vec!["other".into()];
        assert!(
            pending_packages(&[other.clone()], std::slice::from_ref(&installed_app), &[])
                .unwrap_err()
                .to_string()
                .contains("conflicts with")
        );

        // Upgrade introduces a conflict with another installed package.
        let installed_other = receipt("other", "1.2", 'a');
        let mut app_v2 = app.clone();
        app_v2.version = "1.3".into();
        app_v2.sha256 = "b".repeat(64);
        assert!(
            planned_changes(
                &[app_v2],
                &[receipt("app", "1.2", 'a'), installed_other],
                &[receipt("app", "1.2", 'a')],
                &[],
            )
            .unwrap_err()
            .to_string()
            .contains("conflicts with")
        );
    }

    #[test]
    fn render_shellenv_quotes_paths_for_posix_and_fish() {
        let prefix = Path::new("/tmp/brew's space/native/");
        let posix = render_shellenv(prefix, Some("zsh"), None).unwrap();
        assert_eq!(
            posix,
            "export BREWRS_PREFIX='/tmp/brew'\\''s space/native';\nexport PATH='/tmp/brew'\\''s space/native/bin:/tmp/brew'\\''s space/native/sbin':\"$PATH\";\n"
        );

        let fish_auto = render_shellenv(
            Path::new("/tmp/space/native"),
            None,
            Some(std::ffi::OsStr::new("/opt/homebrew/bin/fish")),
        )
        .unwrap();
        assert_eq!(
            fish_auto,
            "set -gx BREWRS_PREFIX '/tmp/space/native';\nfish_add_path -gP '/tmp/space/native/bin' '/tmp/space/native/sbin';\n"
        );

        let fish_quoted = render_shellenv(prefix, Some("fish"), None).unwrap();
        assert_eq!(
            fish_quoted,
            "set -gx BREWRS_PREFIX '/tmp/brew\\'s space/native';\nfish_add_path -gP '/tmp/brew\\'s space/native/bin' '/tmp/brew\\'s space/native/sbin';\n"
        );

        assert!(render_shellenv(Path::new("relative/path"), Some("sh"), None).is_err());
        assert!(render_shellenv(Path::new("/tmp/bad\npath"), Some("sh"), None).is_err());
    }
}
