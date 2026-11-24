use std::collections::{HashMap, HashSet};
use std::io::{self, Write};
use std::path::Path;

use anyhow::{Context, Result};

use crate::config::Config;
use crate::domain::plan;
use crate::infra::store::{
    Receipt, Store, validate_dependencies_with_retained, validate_retained_pin_closure,
};
use crate::ui::DnfStyleFormatter;
use crate::ui::table::{Row, Section, display_path};

use super::format::{formula_note, plural, removed_summary};
use super::{confirm, prefix_path};

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
    apply_removal(
        config,
        &store,
        &pending,
        dry_run,
        yes,
        "Remove packages",
        prefix.is_some(),
    )
}

pub(super) fn planned_removals(
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
        prefix.is_some(),
    )
}

pub(super) fn autoremove_plan(installed: &[Receipt], retained: &[Receipt]) -> Result<Vec<Receipt>> {
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
pub(super) fn reachable_packages(
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

pub(super) fn apply_removal(
    config: &Config,
    store: &Store,
    pending: &[Receipt],
    dry_run: bool,
    yes: bool,
    title: &str,
    show_prefix: bool,
) -> Result<()> {
    let formatter = DnfStyleFormatter::new(config);
    if pending.is_empty() {
        formatter.note("No unused dependencies to remove.");
        return Ok(());
    }
    let rows: Vec<_> = pending
        .iter()
        .map(|receipt| {
            Row::plan(
                "remove",
                formula_note(receipt.keg_only),
                vec![
                    receipt.name.clone(),
                    receipt.version.clone(),
                    receipt.source.label().to_ascii_lowercase(),
                ],
            )
            .requested(receipt.requested)
        })
        .collect();
    let label = if title == "Remove unused dependencies" {
        "Removing unused dependencies"
    } else {
        "Removing"
    };
    let count = pending.len();
    let mut summary = vec![(
        "Remove",
        format!("{count} {}", plural(count, "package", "packages")),
    )];
    if show_prefix {
        summary.push(("Prefix", display_path(store.prefix())));
    }
    let rendered = formatter.plan_table(
        "Remove plan",
        &["Package", "Version", "Source"],
        vec![Section::new(label, rows)],
        &summary,
    );
    println!("{}", rendered.table);
    if let Some(summary) = rendered.summary {
        eprintln!("{summary}");
    }
    if dry_run {
        let noun = plural(count, "change", "changes");
        formatter.note(&format!(
            "\nDry run: {count} {noun} planned, nothing applied."
        ));
        return Ok(());
    }
    io::stdout().flush()?;
    let question = format!("Remove {count} {}?", plural(count, "package", "packages"));
    if config.general.confirm_operations && !yes && !confirm(&question)? {
        eprintln!("Cancelled. No packages changed.");
        return Ok(());
    }
    eprintln!();
    let steps = crate::ui::steps::Steps::new(config);
    let noun = plural(count, "package", "packages");
    let step = steps.start(&format!("Unlinking {count} {noun}"));
    match store.remove(pending) {
        Ok(()) => step.done(&format!("Unlinked {count} {noun}")),
        Err(error) => {
            step.fail();
            return Err(error.into());
        }
    }
    let requested_names: Vec<String> = pending
        .iter()
        .filter(|receipt| receipt.requested)
        .map(|receipt| receipt.name.clone())
        .collect();
    let unused_names: Vec<String> = pending
        .iter()
        .filter(|receipt| !receipt.requested)
        .map(|receipt| receipt.name.clone())
        .collect();
    formatter.note(&format!(
        "\n{}",
        formatter.result(&removed_summary(&requested_names, &unused_names))
    ));
    Ok(())
}

pub(super) fn removal_plan(packages: &[String], installed: &[Receipt]) -> Result<Vec<Receipt>> {
    let mut selected = std::collections::HashSet::new();
    let mut pending = Vec::new();
    for target in packages {
        let name = plan::parse_target(target)?;
        // A catalog alias (e.g. `nvim`) names the upstream package only when no
        // package of that exact name is installed.
        let name = if installed.iter().any(|entry| entry.name == name) {
            name
        } else {
            crate::domain::portable::catalog()?
                .find(target)
                .map_or(name, |package| package.name.as_str())
        };
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
