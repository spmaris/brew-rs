//! Read-only migration audit. Compatibility means metadata eligibility, not
//! installation, runtime parity, or permission to change either prefix.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Result, bail};
use tokio::time::{Instant, timeout_at};

use crate::{config::Config, ui::DnfStyleFormatter};

use super::install::prefix_path;
use crate::domain::cask_plan::{AuditCaskPlanner, PlannedCask};
use crate::domain::migration_inventory::{self, Cask, Formula, Inventory};
use crate::domain::plan::default_target;
use crate::domain::plan::{AuditPlanner, PlannedFormula};
use crate::infra::store;

const AUDIT_TIMEOUT: Duration = Duration::from_secs(90);
const CORE: &str = "homebrew/core";

type Rows = Vec<Vec<String>>;

pub(crate) async fn audit(
    config: &Config,
    destination: Option<&Path>,
    source: Option<&Path>,
    service_dirs: Vec<PathBuf>,
    check: bool,
) -> Result<()> {
    let source = source.map(Path::to_path_buf).unwrap_or_else(source_prefix);
    let destination = prefix_path(destination)?;
    ensure_separated(&source, &destination)?;
    let service_dirs = if service_dirs.is_empty() {
        launchd_dirs()
    } else {
        service_dirs
    };
    let inventory = migration_inventory::collect(&source, &service_dirs)?;
    let tag = if check { Some(default_target()?) } else { None };
    let deadline = Instant::now() + AUDIT_TIMEOUT;
    let assessments = match &tag {
        Some(tag) => assess_all(config, &inventory.formulae, tag, deadline).await?,
        None => inventory.formulae.iter().map(initial_assessment).collect(),
    };
    let cask_assessments = match &tag {
        Some(tag) => assess_all_casks(config, &inventory.casks, tag, deadline).await?,
        None => vec![Assessment::Unchecked; inventory.casks.len()],
    };
    print_report(
        config,
        &inventory,
        &assessments,
        &cask_assessments,
        &source,
        &destination,
        tag.as_deref(),
    );
    Ok(())
}

fn source_prefix() -> PathBuf {
    if let Some(value) = std::env::var_os("HOMEBREW_PREFIX") {
        return value.into();
    }
    if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        "/opt/homebrew".into()
    } else if cfg!(target_os = "macos") {
        "/usr/local".into()
    } else {
        "/home/linuxbrew/.linuxbrew".into()
    }
}

fn launchd_dirs() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if cfg!(target_os = "macos") {
        if let Some(home) = dirs::home_dir() {
            paths.push(home.join("Library/LaunchAgents"));
        }
        paths.extend([
            PathBuf::from("/Library/LaunchAgents"),
            PathBuf::from("/Library/LaunchDaemons"),
        ]);
    }
    paths
}

fn ensure_separated(source: &Path, destination: &Path) -> Result<()> {
    if !source.is_absolute() || !destination.is_absolute() {
        bail!("The Homebrew source and brewrs destination prefixes must be absolute paths");
    }
    let from = store::canonical_candidate(source)?;
    let to = store::canonical_candidate(destination)?;
    if from.starts_with(&to) || to.starts_with(&from) {
        bail!("The Homebrew source and brewrs destination prefixes must not overlap");
    }
    store::reject_protected_prefix(destination)
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Assessment {
    Unchecked,
    Unresolved(String),
    Blocked(String),
    Candidate {
        version: String,
        dependencies: usize,
    },
    CaskCandidate {
        version: String,
        artifacts: usize,
        auto_updates: bool,
    },
}

fn initial_assessment(formula: &Formula) -> Assessment {
    match formula.tap.as_deref() {
        Some(CORE) => Assessment::Unchecked,
        Some(other) => Assessment::Blocked(format!("Tap {other} is not supported")),
        None => Assessment::Unresolved("Receipt does not identify one source tap".into()),
    }
}

async fn assess_all(
    config: &Config,
    formulae: &[Formula],
    tag: &str,
    deadline: Instant,
) -> Result<Vec<Assessment>> {
    let mut results: Vec<_> = formulae.iter().map(initial_assessment).collect();
    if !results.contains(&Assessment::Unchecked) {
        return Ok(results);
    }
    let planner = AuditPlanner::new(config)?;
    let mut order: Vec<_> = (0..formulae.len()).collect();
    order.sort_by_key(|&index| formulae[index].requested != Some(true));
    for i in order {
        if results[i] != Assessment::Unchecked {
            continue;
        }
        if !cfg!(target_os = "macos") {
            results[i] = Assessment::Blocked("Native bottle installation requires macOS".into());
            continue;
        }
        if Instant::now() >= deadline {
            results[i] = Assessment::Unresolved(
                "Migration audit reached its 90-second metadata deadline".into(),
            );
            continue;
        }
        results[i] = match timeout_at(deadline, planner.resolve(&formulae[i].name, tag)).await {
            Ok(Ok(plan)) => assess_plan(&plan),
            Ok(Err(error)) => Assessment::Unresolved(format!("{error:#}")),
            Err(_) => Assessment::Unresolved(
                "Migration audit reached its 90-second metadata deadline".into(),
            ),
        };
    }
    Ok(results)
}

async fn assess_all_casks(
    config: &Config,
    casks: &[Cask],
    tag: &str,
    deadline: Instant,
) -> Result<Vec<Assessment>> {
    if casks.is_empty() {
        return Ok(Vec::new());
    }
    let planner = AuditCaskPlanner::new(config)?;
    let mut results = Vec::with_capacity(casks.len());
    for cask in casks {
        if !cfg!(target_os = "macos") {
            results.push(Assessment::Blocked(
                "Native cask installation requires macOS".into(),
            ));
            continue;
        }
        if Instant::now() >= deadline {
            results.push(Assessment::Unresolved(
                "Migration audit reached its 90-second metadata deadline".into(),
            ));
            continue;
        }
        results.push(
            match timeout_at(deadline, planner.resolve_one(&cask.token, tag)).await {
                Ok(Ok(planned)) => assess_cask_plan(&planned),
                Ok(Err(error)) => Assessment::Unresolved(format!("{error:#}")),
                Err(_) => Assessment::Unresolved(
                    "Migration audit reached its 90-second metadata deadline".into(),
                ),
            },
        );
    }
    Ok(results)
}

fn assess_cask_plan(planned: &PlannedCask) -> Assessment {
    if !planned.install_blockers.is_empty() {
        return Assessment::Blocked(planned.install_blockers.join("; "));
    }
    Assessment::CaskCandidate {
        version: planned.version.clone(),
        artifacts: planned.artifacts.len(),
        auto_updates: planned.auto_updates,
    }
}

fn assess_plan(plan: &[PlannedFormula]) -> Assessment {
    let blockers: Vec<_> = plan
        .iter()
        .filter(|item| !item.install_blockers.is_empty())
        .map(|item| format!("{}: {}", item.name, item.install_blockers.join("; ")))
        .collect();
    if !blockers.is_empty() {
        let mut detail = blockers
            .iter()
            .take(3)
            .cloned()
            .collect::<Vec<_>>()
            .join("; ");
        if blockers.len() > 3 {
            detail.push_str(&format!("; {} other blocked formulae", blockers.len() - 3));
        }
        return Assessment::Blocked(detail);
    }
    let Some(target) = plan.iter().find(|item| item.requested) else {
        return Assessment::Unresolved("Planner did not return the requested formula".into());
    };
    Assessment::Candidate {
        version: target.version.clone(),
        dependencies: plan.len().saturating_sub(1),
    }
}

fn print_report(
    config: &Config,
    inventory: &Inventory,
    assessments: &[Assessment],
    cask_assessments: &[Assessment],
    source: &Path,
    destination: &Path,
    tag: Option<&str>,
) {
    let ui = DnfStyleFormatter::new(config);
    println!("{}", ui.section("Homebrew migration audit"));
    println!("{}", ui.field("Source", &clean(&source.to_string_lossy())));
    println!(
        "{}",
        ui.field("Destination", &clean(&destination.to_string_lossy()))
    );
    println!(
        "{}",
        ui.field(
            "Mode",
            tag.unwrap_or("Offline inventory; add --check for bottle metadata")
        )
    );
    let formula_rows: Rows = inventory
        .formulae
        .iter()
        .zip(assessments)
        .map(|(formula, assessment)| {
            let (state, detail) = format_assessment(assessment);
            vec![
                clean(&formula.name),
                clean(&formula.versions.join(", ")),
                intention(formula.requested).into(),
                state.into(),
                clean(&detail),
            ]
        })
        .collect();
    section(
        &ui,
        "Formulae",
        &[
            "Formula",
            "Recorded versions",
            "Intent",
            "Assessment",
            "Detail",
        ],
        &formula_rows,
    );
    let cask_rows: Rows = inventory
        .casks
        .iter()
        .zip(cask_assessments)
        .map(|(cask, assessment)| {
            let (state, detail) = format_assessment(assessment);
            vec![
                clean(&cask.token),
                clean(&cask.versions.join(", ")),
                state.into(),
                clean(&detail),
            ]
        })
        .collect();
    section(
        &ui,
        "Caskroom records (app presence not checked)",
        &["Cask", "Recorded versions", "Assessment", "Detail"],
        &cask_rows,
    );
    let tap_rows = inventory
        .taps
        .iter()
        .map(|tap| {
            vec![
                clean(tap),
                if tap == CORE || tap == "homebrew/cask" {
                    "Official metadata available"
                } else {
                    "Third-party tap unsupported"
                }
                .into(),
            ]
        })
        .collect();
    section(&ui, "Local taps", &["Tap", "Support"], &tap_rows);
    let service_rows = inventory
        .services
        .iter()
        .map(|service| {
            vec![
                clean(&service.label),
                clean(&service.path.to_string_lossy()),
            ]
        })
        .collect();
    section(
        &ui,
        "Homebrew launchd records (running state not checked)",
        &["Label", "Plist"],
        &service_rows,
    );

    if !inventory.warnings.is_empty() {
        println!("\n{}", ui.section("Inventory gaps"));
        for warning in &inventory.warnings {
            println!("- {}", clean(warning));
        }
    }
    let formula_candidates = assessments
        .iter()
        .filter(|status| matches!(status, Assessment::Candidate { .. }))
        .count();
    let cask_candidates = cask_assessments
        .iter()
        .filter(|status| matches!(status, Assessment::CaskCandidate { .. }))
        .count();
    println!(
        "\n{} formulae, {formula_candidates} formula candidates, {} casks, {cask_candidates} cask candidates, {} local taps, {} launchd records.",
        inventory.formulae.len(),
        inventory.casks.len(),
        inventory.taps.len(),
        inventory.services.len()
    );
    println!(
        "Candidate means the planner accepts the metadata, not that the package was installed or tested. Versions can differ."
    );
    println!(
        "No changes made. Keep Homebrew until you have tested packages and services side by side."
    );
}

fn format_assessment(assessment: &Assessment) -> (&'static str, String) {
    match assessment {
        Assessment::Unchecked => ("Unchecked", "Metadata not queried".into()),
        Assessment::Unresolved(reason) => ("Unresolved", reason.clone()),
        Assessment::Blocked(reason) => ("Blocked", reason.clone()),
        Assessment::Candidate {
            version,
            dependencies,
        } => (
            "Candidate",
            format!("{version} + {dependencies} dependency bottles"),
        ),
        Assessment::CaskCandidate {
            version,
            artifacts,
            auto_updates,
        } => {
            let noun = if *artifacts == 1 {
                "artifact"
            } else {
                "artifacts"
            };
            let suffix = if *auto_updates { ", auto-updates" } else { "" };
            (
                "Candidate",
                format!("{version} ({artifacts} {noun}{suffix})"),
            )
        }
    }
}

fn intention(requested: Option<bool>) -> &'static str {
    match requested {
        Some(true) => "requested",
        Some(false) => "dependency",
        None => "Unknown",
    }
}

fn clean(data: &str) -> String {
    // Logs and path names can contain terminal escape sequences. Render them
    // as data, not control characters, and bound any single table cell.
    let mut chars = data.chars();
    let mut value: String = chars
        .by_ref()
        .take(512)
        .map(|ch| {
            if ch.is_control() || matches!(ch, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}') {
                '�'
            } else {
                ch
            }
        })
        .collect();
    if chars.next().is_some() {
        value.push('…');
    }
    value
}

fn section(ui: &DnfStyleFormatter, title: &str, headers: &[&str], rows: &Rows) {
    println!();
    if rows.is_empty() {
        println!("{}", ui.section(title));
        println!("None observed.");
        return;
    }
    // Plain output has no panel title, so keep the section heading.
    if !ui.boxed() {
        println!("{}", ui.section(title));
    }
    println!(
        "{}",
        ui.table(&format!("{title} ({})", rows.len()), headers, rows)
    );
}

#[cfg(test)]
mod tests;
