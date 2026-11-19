use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::{config::Config, ui::DnfStyleFormatter};

use super::plan::PlannedFormula;
use super::store::{Dependency, Prepared, Receipt, Store, validate_dependencies};
use super::{archive, default_target, download, plan};

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
    let plan = plan::resolve(config, packages, &tag).await?;
    let pending = pending_packages(&plan, &existing)?;
    let promotions = requested_promotions(&plan, &existing);
    let formatter = DnfStyleFormatter::new(config);
    let rows: Vec<_> = plan
        .iter()
        .map(|formula| {
            let action = if pending
                .iter()
                .any(|entry| entry.receipt.name == formula.name)
            {
                "Install"
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
    let mut prepared = Vec::new();
    for (candidate, bottle) in downloaded {
        prepared.push(stage_formula(&store, candidate, &bottle)?);
    }
    let count = prepared.len();
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
    Ok(Prepared { receipt, stage })
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
    Ok(Receipt {
        name: formula.name.clone(),
        version: formula.version.clone(),
        sha256: formula.sha256.clone(),
        requested,
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
    let selected = if packages.is_empty() {
        installed.clone()
    } else {
        removal_plan(packages, &installed)?
    };
    if selected.is_empty() {
        println!("No brewrs-managed packages to upgrade.");
        return Ok(());
    }

    let (tag, changes) = resolve_changes(config, &installed, &selected).await?;
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

    let store = Store::open_existing(&path)?;
    let mut replacements = Vec::new();
    for change in changes {
        let bottle = fetch_formula(config, &change.formula).await?;
        let candidate = Candidate {
            formula: &change.formula,
            receipt: change.new,
        };
        replacements.push((change.old, stage_formula(&store, candidate, &bottle)?));
    }
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

    let (tag, changes) = resolve_changes(config, &installed, &installed).await?;
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
) -> Result<(String, Vec<UpgradeChange>)> {
    let targets: Vec<_> = selected
        .iter()
        .map(|receipt| receipt.name.clone())
        .collect();
    let tag = default_target()?;
    let plan = plan::resolve(config, &targets, &tag).await?;
    let changes = planned_changes(&plan, installed, selected)?;
    Ok((tag, changes))
}

/// Reduce an already-resolved plan to the ordered changes it would apply.
///
/// Eligibility is decided by [`upgrade_plan`]; the projected receipt graph is
/// then validated so neither the preview nor a check reports an upgrade that
/// would break an installed dependency pin.
fn planned_changes(
    plan: &[PlannedFormula],
    installed: &[Receipt],
    selected: &[Receipt],
) -> Result<Vec<UpgradeChange>> {
    let changes: Vec<_> = upgrade_plan(plan, selected)?
        .into_iter()
        .map(|(old, candidate)| UpgradeChange {
            old,
            new: candidate.receipt,
            formula: candidate.formula.clone(),
        })
        .collect();
    let mut projected = installed.to_vec();
    for change in &changes {
        let receipt = projected
            .iter_mut()
            .find(|entry| entry.name == change.old.name)
            .context("An upgrade target was missing from the installed snapshot")?;
        *receipt = change.new.clone();
    }
    validate_dependencies(&projected).context(
        "Upgrade would leave unmet dependency pins; select compatible updates together or install missing requirements first",
    )?;
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
        if new.version == old.version && new.sha256 == old.sha256 {
            continue;
        }
        if new.version_scheme != Some(0) || !new.install_blockers.is_empty() {
            bail!(
                "Cannot upgrade {} natively: {}",
                old.name,
                if new.install_blockers.is_empty() {
                    "unsupported version scheme".into()
                } else {
                    new.install_blockers.join("; ")
                }
            );
        }
        let receipt = receipt_for(new, plan, old.requested)?;
        match super::version::compare(&new.version, &old.version)
            .with_context(|| format!("Cannot compare native versions for '{}'", old.name))?
        {
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
    let pending = planned_removals(packages, &installed, autoremove)?;
    apply_removal(config, &store, &pending, dry_run, yes, "Remove packages")
}

fn planned_removals(
    packages: &[String],
    installed: &[Receipt],
    autoremove: bool,
) -> Result<Vec<Receipt>> {
    let mut pending = removal_plan(packages, installed)?;
    let names: HashSet<_> = pending
        .iter()
        .map(|receipt| receipt.name.as_str())
        .collect();
    let retained: Vec<_> = installed
        .iter()
        .filter(|entry| !names.contains(entry.name.as_str()))
        .cloned()
        .collect();
    validate_dependencies(&retained).context("Removal would leave unmet dependencies")?;
    if autoremove {
        validate_dependencies(installed).context("Cannot determine dependency ownership safely")?;
        let previously_needed = reachable_packages(installed, |entry| entry.requested);
        // Keep previously unused packages and whatever they still need.
        let still_needed = reachable_packages(&retained, |entry| {
            entry.requested || !previously_needed.contains(entry.name.as_str())
        });
        pending.extend(
            retained
                .iter()
                .filter(|entry| !still_needed.contains(entry.name.as_str()))
                .cloned(),
        );
    }
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
    let pending = autoremove_plan(&installed)?;
    apply_removal(
        config,
        &store,
        &pending,
        dry_run,
        yes,
        "Remove unused dependencies",
    )
}

fn autoremove_plan(installed: &[Receipt]) -> Result<Vec<Receipt>> {
    validate_dependencies(installed).context("Cannot determine dependency ownership safely")?;
    let needed = reachable_packages(installed, |entry| entry.requested);
    Ok(installed
        .iter()
        .filter(|entry| !needed.contains(entry.name.as_str()))
        .cloned()
        .collect())
}

// Exact pins are validated before the walk; visit each node at most once.
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
                    .map(|dep| by_name[dep.name.as_str()]),
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
    let mut receipts = store.installed()?;
    if receipts.is_empty() {
        println!(
            "No brewrs-managed packages in {}.",
            store.prefix().display()
        );
        return Ok(());
    }
    receipts.sort_by(|left, right| left.name.cmp(&right.name));
    let formatter = DnfStyleFormatter::new(config);
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
    Ok(())
}

fn pending_packages<'a>(
    plan: &'a [PlannedFormula],
    installed: &[Receipt],
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
    validate_dependencies(&projected)?;
    Ok(pending)
}

/// Explain why `install` refuses to replace an installed package in place,
/// naming the installed and available versions and bottle digests instead of
/// lumping them into one vague "different version, bottle, or dependencies".
fn replacement_conflict(installed: &Receipt, planned: &Receipt) -> String {
    if installed.version != planned.version {
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

fn confirm() -> Result<bool> {
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
            pending_packages(&plan, &[])
                .unwrap_err()
                .to_string()
                .contains("post-install")
        );
    }

    fn installed_hello(version: &str) -> Receipt {
        Receipt {
            name: "hello".into(),
            version: version.into(),
            sha256: "a".repeat(64),
            requested: true,
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
        assert_eq!(autoremove_plan(&installed).unwrap(), [installed[3].clone()]);
        assert!(autoremove_plan(&[]).unwrap().is_empty());
        assert!(autoremove_plan(&installed[..2]).is_err());
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
            planned_removals(&targets, &installed, true).unwrap(),
            [installed[0].clone(), installed[3].clone()]
        );
        assert_eq!(
            planned_removals(&targets, &installed, false).unwrap(),
            [installed[0].clone()]
        );
        assert!(planned_removals(&["shared".into()], &installed, true).is_err());
    }

    #[test]
    fn explicitly_installing_a_dependency_plans_a_promotion_without_a_download() {
        let installed = [package_receipt("hello", false, &[])];
        let mut plan = [formula()];
        assert!(pending_packages(&plan, &installed).unwrap().is_empty());
        assert_eq!(requested_promotions(&plan, &installed), installed);
        plan[0].requested = false;
        assert!(requested_promotions(&plan, &installed).is_empty());
    }

    #[test]
    fn upgrade_only_selects_strictly_newer_compatible_bottles() {
        let mut formula = formula();
        let old = [installed_hello("1.2")];
        assert!(upgrade_plan(&[formula.clone()], &old).unwrap().is_empty());
        formula.version = "1.2_1".into();
        formula.sha256 = "b".repeat(64);
        let planned = [formula.clone()];
        let changes = upgrade_plan(&planned, &old).unwrap();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].0.version, "1.2");
        assert_eq!(changes[0].1.receipt.version, "1.2_1");

        formula.version_scheme = Some(1);
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

        assert!(pending_packages(&[app.clone()], &[]).is_err());
        let plan = [library.clone(), app];
        let pending = pending_packages(&plan, &[]).unwrap();
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
        let pending = pending_packages(&plan, &already_installed).unwrap();
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
            validate_dependencies(&[changes[0].1.receipt.clone(), installed[1].clone()]).is_err()
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
            sha256: "a".repeat(64),
            requested: true,
            dependencies: Vec::new(),
        };
        assert!(
            pending_packages(&plan, std::slice::from_ref(&receipt))
                .unwrap()
                .is_empty()
        );
        receipt.sha256 = "b".repeat(64);
        assert!(pending_packages(&plan, &[receipt]).is_err());
    }

    fn receipt(name: &str, version: &str, digest: char) -> Receipt {
        Receipt {
            name: name.into(),
            version: version.into(),
            sha256: digest.to_string().repeat(64),
            requested: true,
            dependencies: Vec::new(),
        }
    }

    #[test]
    fn check_updates_and_upgrade_share_the_same_change_plan() {
        let current = [receipt("hello", "1.2", 'a')];
        let mut available = formula();
        assert!(
            planned_changes(&[available.clone()], &current, &current)
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
        let changes = planned_changes(&[available], &current, &current).unwrap();
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
        let message = planned_changes(&[available], &current, &current)
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
        let message = pending_packages(&[available], &current)
            .unwrap_err()
            .to_string();
        assert!(message.contains("hello 1.2 is installed"), "{message}");
        assert!(message.contains("1.3 is available"), "{message}");
        assert!(message.contains("native upgrade"), "{message}");

        let mut available = formula();
        available.sha256 = "b".repeat(64);
        let message = pending_packages(&[available], &current)
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
        let message = pending_packages(&[library, available], &current)
            .unwrap_err()
            .to_string();
        assert!(message.contains("hello 1.2"), "{message}");
        assert!(message.contains("dependency pins"), "{message}");
    }

    #[test]
    fn upgrade_preview_rejects_a_change_that_breaks_a_dependency_pin() {
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
        let installed = [old_library, old_app];
        let plan = [library, app];

        let error = planned_changes(&plan, &installed, &installed).unwrap_err();
        // The CLI renders the whole anyhow chain, so assert on that form.
        let message = format!("{error:#}");
        assert!(message.contains("unmet dependency pins"), "{message}");
        assert!(message.contains("1.1"), "{message}");
        assert!(message.contains("1.2"), "{message}");
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
}
