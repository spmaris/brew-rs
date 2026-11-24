use std::collections::HashSet;

use anyhow::{Result, bail};

use crate::domain::plan::{PackageSource, PlannedFormula};
use crate::domain::portable::Catalog;
use crate::infra::store::Receipt;

use super::pending::InstallBlocked;

/// Split install targets into those still to resolve and the notices for
/// catalog packages already installed from upstream. Without `--source` the
/// switch error would be useless advice, so these are reported as satisfied.
pub(super) fn partition_upstream_installed(
    catalog: &Catalog,
    targets: &[String],
    installed: &[Receipt],
) -> (Vec<String>, Vec<String>) {
    let mut rest = Vec::new();
    let mut notes = Vec::new();
    for target in targets {
        let present = catalog.find(target).and_then(|package| {
            installed.iter().find(|receipt| {
                receipt.name == package.name && receipt.source == PackageSource::Upstream
            })
        });
        match present {
            Some(receipt) => {
                let name = if target == &receipt.name {
                    receipt.name.clone()
                } else {
                    format!("{target} → {}", receipt.name)
                };
                notes.push(format!(
                    "{name} {} is already installed (official upstream build).",
                    receipt.version
                ));
            }
            None => rest.push(target.clone()),
        }
    }
    (rest, notes)
}

/// Reject plans that mix sources for one package or let an upstream build
/// stand in for a Homebrew dependency.
pub(super) fn validate_sources(plan: &[PlannedFormula], installed: &[Receipt]) -> Result<()> {
    for formula in plan {
        if formula.source == PackageSource::Homebrew {
            for dependency in &formula.dependencies {
                let origin = if plan.iter().any(|other| {
                    other.name == *dependency && other.source == PackageSource::Upstream
                }) {
                    "planned from upstream"
                } else if installed.iter().any(|receipt| {
                    receipt.name == *dependency && receipt.source == PackageSource::Upstream
                }) {
                    "installed from upstream"
                } else {
                    continue;
                };
                bail!(
                    "{} depends on {dependency}, which is {origin}; an upstream build cannot satisfy a Homebrew dependency",
                    formula.name
                );
            }
        }
        if let Some(receipt) = installed
            .iter()
            .find(|receipt| receipt.name == formula.name && receipt.source != formula.source)
        {
            bail!(
                "{} is installed from {}; uninstall it first to switch",
                receipt.name,
                receipt.source.label()
            );
        }
    }
    Ok(())
}

/// Append a hint when the blocker belongs to a requested target (or one of its
/// dependencies) that the reviewed upstream catalog can supply. The original
/// error is otherwise unchanged; nothing is ever installed from upstream
/// without `--source upstream`.
pub(super) fn with_upstream_hint(
    catalog: Result<&Catalog>,
    error: anyhow::Error,
    plan: &[PlannedFormula],
    installed: &[Receipt],
    tag: &str,
) -> anyhow::Error {
    if error.downcast_ref::<InstallBlocked>().is_none() {
        return error;
    }
    let Ok(catalog) = catalog else {
        return error;
    };
    let blocked = |formula: &PlannedFormula| {
        !formula.install_blockers.is_empty()
            && !installed.iter().any(|receipt| receipt.name == formula.name)
    };
    let mut message = error.to_string();
    let mut hinted = HashSet::new();
    for target in plan.iter().filter(|formula| formula.requested) {
        // Dependencies precede dependents, so one forward walk over the plan
        // collects this target's closure.
        let mut closure = HashSet::from([target.name.as_str()]);
        for formula in plan.iter().rev() {
            if closure.contains(formula.name.as_str()) {
                closure.extend(formula.dependencies.iter().map(String::as_str));
            }
        }
        if !plan
            .iter()
            .any(|formula| closure.contains(formula.name.as_str()) && blocked(formula))
        {
            continue;
        }
        if let Some(package) = catalog
            .find(&target.name)
            .filter(|package| package.supports(tag))
            && hinted.insert(package.name.as_str())
        {
            message.push_str(&format!(
                "\nAn official upstream build is available: brewrs install --source upstream {}",
                package.name
            ));
        }
    }
    anyhow::anyhow!(message)
}
