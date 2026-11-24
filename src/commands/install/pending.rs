use std::cmp::Ordering;
use std::collections::HashSet;

use anyhow::{Result, bail};

use crate::domain::plan::PlannedFormula;
use crate::infra::store::{Receipt, validate_conflicts, validate_dependencies_with_retained};

use super::format::short_digest;
use super::stage::{Candidate, receipt_for};

/// A formula that cannot be installed natively.
#[derive(Debug)]
pub(super) struct InstallBlocked(String);

impl std::fmt::Display for InstallBlocked {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for InstallBlocked {}

pub(super) fn pending_packages<'a>(
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
            let mut message = format!(
                "Cannot install {} natively: {}.",
                formula.name,
                formula.install_blockers.join("; ")
            );
            for blocker in &formula.install_blockers {
                if let Some(reason) = blocker_reason(blocker) {
                    message.push_str("\n  ");
                    message.push_str(reason);
                }
            }
            if !formula.requested {
                let needed_by = requested_dependents(plan, &formula.name);
                if !needed_by.is_empty() {
                    message.push_str("\n  Needed by: ");
                    message.push_str(&needed_by.join(", "));
                }
            }
            return Err(InstallBlocked(message).into());
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

/// A plain-language reason for a blocker whose cause is not obvious.
pub(super) fn blocker_reason(blocker: &str) -> Option<&'static str> {
    match blocker {
        "requires a fixed Homebrew prefix" => Some(
            "Homebrew built this bottle for /opt/homebrew only; brewrs installs into its own prefix, so this bottle can't be used.",
        ),
        "requires bottle relocation not yet supported for this formula or platform" => {
            Some("brewrs can't relocate this bottle on this macOS version yet.")
        }
        _ => None,
    }
}

/// Requested targets that depend on `blocked`, directly or transitively.
pub(super) fn requested_dependents<'a>(
    plan: &'a [PlannedFormula],
    blocked: &'a str,
) -> Vec<&'a str> {
    let mut needed: HashSet<&str> = HashSet::from([blocked]);
    loop {
        let before = needed.len();
        for formula in plan {
            if formula
                .dependencies
                .iter()
                .any(|dependency| needed.contains(dependency.as_str()))
            {
                needed.insert(formula.name.as_str());
            }
        }
        if needed.len() == before {
            break;
        }
    }
    plan.iter()
        .filter(|formula| {
            formula.requested && formula.name != blocked && needed.contains(formula.name.as_str())
        })
        .map(|formula| formula.name.as_str())
        .collect()
}

/// Explain why `install` refuses to replace an installed package in place,
/// naming the installed and available versions and bottle digests instead of
/// lumping them into one vague "different version, bottle, or dependencies".
pub(super) fn replacement_conflict(installed: &Receipt, planned: &Receipt) -> String {
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
        match crate::domain::version::compare(&planned.version, &installed.version) {
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
