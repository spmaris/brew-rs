use std::cmp::Ordering;
use std::io::{self, Write};
use std::path::Path;

use anyhow::{Context, Result, bail};

use crate::config::Config;
use crate::domain::plan::{PackageSource, PlannedFormula};
use crate::infra::platform::default_target;
use crate::infra::relocate;
use crate::infra::store::{
    Receipt, Store, validate_conflicts, validate_dependencies_with_retained,
    validate_retained_pin_closure,
};
use crate::ui::DnfStyleFormatter;
use crate::ui::table::{Row, Section, display_path};

use super::format::{formula_note, plural, upgraded_summary};
use super::remove::removal_plan;
use super::sources::validate_sources;
use super::stage::{Candidate, fetch_formula, receipt_for, stage_formula};
use super::{confirm, prefix_path};
use crate::ui::text::short_digest;

/// One planned upgrade: the installed receipt, its replacement receipt, and the
/// planned formula that supplies the replacement bottle.
///
/// Both [`upgrade`] and [`check_updates`] build this through
/// [`resolve_changes`], so the preview table, the eligibility rules and the
/// dependency-pin checks can never drift between them.
#[derive(Debug)]
pub(super) struct UpgradeChange {
    pub(super) old: Receipt,
    pub(super) new: Receipt,
    pub(super) formula: PlannedFormula,
}

pub(crate) async fn upgrade(
    config: &Config,
    prefix: Option<&Path>,
    packages: &[String],
    dry_run: bool,
    yes: bool,
    announce_current: bool,
) -> Result<bool> {
    if std::env::consts::OS != "macos" {
        bail!("Native upgrades are supported on macOS only");
    }
    let path = prefix_path(prefix)?;
    let store = Store::open(&path, false)?;
    let installed = store.installed()?;
    let retained = store.retained()?;
    let formatter = DnfStyleFormatter::new(config);
    let selected = if packages.is_empty() {
        installed.clone()
    } else {
        removal_plan(packages, &installed)?
    };
    if selected.is_empty() {
        formatter.note("No packages installed yet.");
        return Ok(false);
    }

    let (_, changes) = resolve_changes(config, &installed, &selected, &retained).await?;
    if changes.is_empty() {
        if announce_current {
            formatter.note("All packages are up to date.");
        }
        return Ok(false);
    }

    render_upgrade_preview(
        &formatter,
        "Upgrade plan",
        store.prefix(),
        prefix.is_some(),
        &changes,
    );
    if dry_run {
        let count = changes.len();
        let noun = plural(count, "change", "changes");
        formatter.note(&format!(
            "\nDry run: {count} {noun} planned, nothing applied."
        ));
        return Ok(true);
    }
    io::stdout().flush()?;
    let question = format!(
        "Upgrade {} {}?",
        changes.len(),
        plural(changes.len(), "package", "packages")
    );
    if config.general.confirm_operations && !yes && !confirm(&question)? {
        eprintln!("Cancelled. No packages changed.");
        return Ok(true);
    }

    if changes.iter().any(|item| item.formula.cellar == ":any") {
        relocate::preflight()?;
    }
    let store = Store::open_existing(&path)?;
    eprintln!();
    let steps = crate::ui::steps::Steps::new(config);
    let upgraded: Vec<String> = changes
        .iter()
        .map(|change| {
            format!(
                "{} {} → {}",
                change.formula.name, change.old.version, change.new.version
            )
        })
        .collect();
    let mut staged = Vec::new();
    for change in changes {
        let name = change.formula.name.clone();
        let version = change.formula.version.clone();
        let step = steps.start(&format!("Downloading {name} {version}"));
        let bottle = match fetch_formula(config, &change.formula).await {
            Ok(bottle) => {
                step.done(&format!("Downloaded {name} {version} (checksum verified)"));
                bottle
            }
            Err(error) => {
                step.fail();
                return Err(error);
            }
        };
        let needs_relocation = change.formula.cellar == ":any";
        let step = steps.start(&format!("Unpacking {name}"));
        let candidate = Candidate {
            formula: &change.formula,
            receipt: change.new,
        };
        match stage_formula(&store, candidate, &bottle) {
            Ok(package) => {
                step.done(&format!("Unpacked {name}"));
                staged.push((needs_relocation, change.old, package));
            }
            Err(error) => {
                step.fail();
                return Err(error);
            }
        }
    }
    let relocate_items = || {
        staged
            .iter()
            .map(|(needs_relocation, _, package)| relocate::Item {
                package,
                needs_relocation: *needs_relocation,
            })
    };
    let relocate_count = staged.iter().filter(|(needs, _, _)| *needs).count();
    if relocate_count != 0 {
        let noun = plural(relocate_count, "package", "packages");
        let step = steps.start(&format!("Relocating {relocate_count} {noun}"));
        match relocate::staged(store.prefix(), relocate_items()) {
            Ok(()) => step.done(&format!("Relocated {relocate_count} {noun}")),
            Err(error) => {
                step.fail();
                return Err(error);
            }
        }
    } else {
        relocate::staged(store.prefix(), relocate_items())?;
    }
    let replacements: Vec<_> = staged
        .into_iter()
        .map(|(_, old, package)| (old, package))
        .collect();
    let step = steps.start("Replacing installed versions");
    match store.upgrade(replacements) {
        Ok(()) => step.done("Replaced old versions"),
        Err(error) => {
            step.fail();
            return Err(error);
        }
    }
    formatter.note(&format!(
        "\n{}",
        formatter.result(&upgraded_summary(&upgraded))
    ));
    Ok(true)
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
    let formatter = DnfStyleFormatter::new(config);
    if installed.is_empty() {
        formatter.note("No packages installed yet.");
        return Ok(false);
    }
    let retained = store.retained()?;

    let (_, changes) = resolve_changes(config, &installed, &installed, &retained).await?;
    if changes.is_empty() {
        formatter.note("All packages are up to date.");
        return Ok(false);
    }
    render_upgrade_preview(
        &formatter,
        "Available updates",
        store.prefix(),
        prefix.is_some(),
        &changes,
    );
    formatter.note(&format!(
        "\n{} {} available.",
        formatter.success(&changes.len().to_string()),
        if changes.len() == 1 {
            "update"
        } else {
            "updates"
        }
    ));
    Ok(true)
}

/// Resolve the current bottle plan for `selected` and reduce it to upgrades.
///
/// Shared by [`upgrade`] and [`check_updates`] so both commands plan exactly
/// once and agree on every accepted or rejected change.
pub(super) async fn resolve_changes(
    config: &Config,
    installed: &[Receipt],
    selected: &[Receipt],
    retained: &[Receipt],
) -> Result<(String, Vec<UpgradeChange>)> {
    let tag = default_target()?;
    // Upgrades follow the recorded source; a package never switches source.
    let homebrew: Vec<String> = selected
        .iter()
        .filter(|receipt| receipt.source == PackageSource::Homebrew)
        .map(|receipt| format!("homebrew/core/{}", receipt.name))
        .collect();
    let upstream: Vec<String> = selected
        .iter()
        .filter(|receipt| receipt.source == PackageSource::Upstream)
        .map(|receipt| receipt.name.clone())
        .collect();
    let mut plan = if upstream.is_empty() {
        Vec::new()
    } else {
        crate::domain::portable::catalog()?
            .plan_targets(&upstream, &tag)
            .context("Cannot plan upgrades for upstream-installed packages")?
    };
    if !homebrew.is_empty() {
        plan.extend(crate::commands::plan::resolve(config, &homebrew, &tag).await?);
    }
    validate_sources(&plan, installed)?;
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
pub(super) fn planned_changes(
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
pub(super) fn render_upgrade_preview(
    formatter: &DnfStyleFormatter,
    title: &str,
    prefix: &Path,
    show_prefix: bool,
    changes: &[UpgradeChange],
) {
    let count = changes.len();
    let mut summary = vec![(
        "Upgrade",
        format!("{count} {}", plural(count, "package", "packages")),
    )];
    if show_prefix {
        summary.push(("Prefix", display_path(prefix)));
    }
    let rendered = formatter.plan_table(
        title,
        &["Package", "Version", "Source"],
        vec![Section::new("Upgrading", upgrade_rows(changes))],
        &summary,
    );
    println!("{}", rendered.table);
    if let Some(summary) = rendered.summary {
        eprintln!("{summary}");
    }
}

pub(super) fn upgrade_rows(changes: &[UpgradeChange]) -> Vec<Row> {
    changes
        .iter()
        .map(|change| {
            Row::plan(
                "upgrade",
                formula_note(change.formula.keg_only),
                vec![
                    change.old.name.clone(),
                    format!("{} → {}", change.old.version, change.new.version),
                    change.new.source.label().to_ascii_lowercase(),
                ],
            )
            .requested(change.formula.requested)
        })
        .collect()
}

pub(super) fn upgrade_plan<'a>(
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
        if old.source != new.source {
            bail!(
                "Refusing to change the installed source for '{}' during upgrade",
                old.name
            );
        }
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
            Ordering::Equal => crate::domain::version::compare(&new.version, &old.version)
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
pub(super) fn same_version_conflict(old: &Receipt, new: &PlannedFormula) -> String {
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
