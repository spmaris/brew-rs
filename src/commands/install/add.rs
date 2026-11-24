use std::collections::HashSet;
use std::io::{self, Write};
use std::path::Path;

use anyhow::{Result, bail};

use crate::config::Config;
use crate::domain::plan::{self, PlannedFormula, SourceFlag};
use crate::infra::platform::default_target;
use crate::infra::relocate;
use crate::infra::store::{Receipt, Store, reject_protected_prefix};
use crate::ui::DnfStyleFormatter;
use crate::ui::table::{Row, Section, display_path};

use super::format::{
    formula_note, install_question, installed_summary, next_path_hint, package_count, plural,
};
use super::pending::pending_packages;
use super::sources::{partition_upstream_installed, validate_sources, with_upstream_hint};
use super::stage::{fetch_formula, stage_formula};
use super::{confirm, prefix_path};

pub(crate) async fn install(
    config: &Config,
    prefix: Option<&Path>,
    packages: &[String],
    dry_run: bool,
    yes: bool,
    source: Option<SourceFlag>,
) -> Result<()> {
    if std::env::consts::OS != "macos" {
        bail!("Native installation is supported on macOS only; plan and fetch are still available");
    }
    let path = prefix_path(prefix)?;
    reject_protected_prefix(&path)?;
    let store = Store::open(&path, false)?;
    let tag = default_target()?;
    let existing = store.installed()?;
    let retained = store.retained()?;
    let existing_casks = crate::infra::cask_store::installed(&store)?;
    let formatter = DnfStyleFormatter::new(config);
    let remaining;
    let packages = if source.is_none() {
        let (rest, notes) =
            partition_upstream_installed(crate::domain::portable::catalog()?, packages, &existing);
        for note in &notes {
            formatter.note(note);
        }
        if rest.is_empty() {
            return Ok(());
        }
        remaining = rest;
        &remaining[..]
    } else {
        packages
    };
    let plan = match source {
        Some(SourceFlag::Upstream) => {
            crate::domain::portable::catalog()?.plan_targets(packages, &tag)?
        }
        Some(SourceFlag::Homebrew) | None => {
            crate::commands::plan::resolve(config, packages, &tag).await?
        }
    };
    validate_sources(&plan, &existing)?;
    let pending = pending_packages(&plan, &existing, &retained).map_err(|error| {
        with_upstream_hint(
            crate::domain::portable::catalog(),
            error,
            &plan,
            &existing,
            &tag,
        )
    })?;
    let mut projected = existing.clone();
    for item in &pending {
        projected.push(item.receipt.clone());
    }
    crate::infra::cask_store::validate_cask_conflicts(&existing_casks, &projected)?;
    let installed_requested: Vec<String> = pending
        .iter()
        .filter(|item| item.formula.requested)
        .map(|item| format!("{} {}", item.formula.name, item.formula.version))
        .collect();
    let installed_dependencies = pending
        .iter()
        .filter(|item| !item.formula.requested)
        .count();
    let installed_caveats: Vec<(&str, &str)> = pending
        .iter()
        .filter(|item| item.formula.requested)
        .filter_map(|item| {
            item.formula
                .caveats
                .as_deref()
                .map(str::trim)
                .filter(|text| !text.is_empty())
                .map(|text| (item.formula.name.as_str(), text))
        })
        .collect();
    let promotions = requested_promotions(&plan, &existing);
    if pending.is_empty() && promotions.is_empty() {
        for (formula, alias) in requested_installed(packages, &plan)? {
            let name = match alias {
                Some(typed) => format!("{typed} → {}", formula.name),
                None => formula.name.clone(),
            };
            formatter.note(&format!("{name} {} is already installed.", formula.version));
        }
        if dry_run {
            formatter.note("\nDry run: nothing to do.");
        }
        return Ok(());
    }
    let mut installing = Vec::new();
    let mut dependencies = Vec::new();
    let mut promoting = Vec::new();
    let mut present = Vec::new();
    for formula in &plan {
        let requested = formula.requested
            || existing
                .iter()
                .any(|receipt| receipt.name == formula.name && receipt.requested);
        let (action, group) = if pending
            .iter()
            .any(|entry| entry.receipt.name == formula.name)
        {
            (
                "install",
                if requested {
                    &mut installing
                } else {
                    &mut dependencies
                },
            )
        } else if promotions
            .iter()
            .any(|receipt| receipt.name == formula.name)
        {
            ("mark-requested", &mut promoting)
        } else {
            ("already-installed", &mut present)
        };
        let note = formula_note(formula.keg_only);
        let cells = vec![
            formula.name.clone(),
            formula.version.clone(),
            formula.source.label().to_ascii_lowercase(),
        ];
        group.push(Row::plan(action, note, cells).requested(requested));
    }
    let (requested_count, dependency_count) = (installing.len(), dependencies.len());
    let mut summary = Vec::new();
    if requested_count + dependency_count != 0 {
        summary.push(("Install", package_count(requested_count, dependency_count)));
    }
    if !promotions.is_empty() {
        let count = promotions.len();
        summary.push((
            "Mark",
            format!(
                "{count} {} as requested",
                plural(count, "package", "packages")
            ),
        ));
    }
    if prefix.is_some() {
        summary.push(("Prefix", display_path(store.prefix())));
    }
    let rendered = formatter.plan_table(
        "Install plan",
        &["Package", "Version", "Source"],
        vec![
            Section::new("Installing", installing),
            Section::new("Installing dependencies", dependencies),
            Section::new("Promoting", promoting),
            Section::new("Already installed", present),
        ],
        &summary,
    );
    println!("{}", rendered.table);
    if let Some(summary) = rendered.summary {
        eprintln!("{summary}");
    }

    if dry_run {
        let count = pending.len() + promotions.len();
        let noun = plural(count, "change", "changes");
        formatter.note(&format!(
            "\nDry run: {count} {noun} planned, nothing applied."
        ));
        return Ok(());
    }
    io::stdout().flush()?;
    let question = install_question(pending.len(), promotions.len());
    if config.general.confirm_operations && !yes && !confirm(&question)? {
        eprintln!("Cancelled. No packages changed.");
        return Ok(());
    }

    if pending.iter().any(|item| item.formula.cellar == ":any") {
        relocate::preflight()?;
    }
    if !pending.is_empty() {
        eprintln!();
    }
    let service_names: Vec<String> = pending
        .iter()
        .filter(|item| item.formula.has_service)
        .map(|item| item.formula.name.clone())
        .collect();
    let steps = crate::ui::steps::Steps::new(config);
    let mut downloaded = Vec::new();
    for candidate in pending {
        let formula = candidate.formula;
        let step = steps.start(&format!("Downloading {} {}", formula.name, formula.version));
        match fetch_formula(config, formula).await {
            Ok(bottle) => {
                step.done(&format!(
                    "Downloaded {} {} (checksum verified)",
                    formula.name, formula.version
                ));
                downloaded.push((candidate, bottle));
            }
            Err(error) => {
                step.fail();
                return Err(error);
            }
        }
    }
    let store = if promotions.is_empty() {
        Store::open(&path, true)?
    } else {
        Store::open_existing(&path)?
    };
    let mut staged = Vec::new();
    for (candidate, bottle) in downloaded {
        let needs_relocation = candidate.formula.cellar == ":any";
        let name = candidate.formula.name.clone();
        let step = steps.start(&format!("Unpacking {name}"));
        match stage_formula(&store, candidate, &bottle) {
            Ok(package) => {
                step.done(&format!("Unpacked {name}"));
                staged.push((needs_relocation, package));
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
            .map(|(needs_relocation, package)| relocate::Item {
                package,
                needs_relocation: *needs_relocation,
            })
    };
    let relocate_count = staged.iter().filter(|(needs, _)| *needs).count();
    if relocate_count != 0 {
        let noun = plural(relocate_count, "package", "packages");
        let step = steps.start(&format!("Relocating {relocate_count} {noun}"));
        match relocate::staged(store.prefix(), relocate_items()) {
            Ok(()) => step.done(&format!("Relocated {relocate_count} {noun}")),
            Err(error) => {
                step.fail();
                return Err(error.into());
            }
        }
    } else {
        relocate::staged(store.prefix(), relocate_items())?;
    }
    let prepared: Vec<_> = staged.into_iter().map(|(_, package)| package).collect();
    let count = prepared.len();
    let any_keg_only = prepared.iter().any(|package| package.receipt.keg_only);
    if count != 0 {
        let step = steps.start("Linking into prefix");
        let result = if promotions.is_empty() {
            store.commit(prepared)
        } else {
            store.commit_with_promotions(prepared, &promotions)
        };
        match result {
            Ok(()) => step.done(&format!(
                "Linked {count} {}",
                plural(count, "package", "packages")
            )),
            Err(error) => {
                step.fail();
                return Err(error.into());
            }
        }
    } else if promotions.is_empty() {
        store.commit(prepared)?;
    } else {
        store.commit_with_promotions(prepared, &promotions)?;
    }
    if count != 0 {
        let bin = store.prefix().join("bin");
        formatter.note(&format!(
            "\n{}",
            formatter.result(&installed_summary(
                &installed_requested,
                installed_dependencies
            ))
        ));
        let mut follow_up = vec![("Bin", display_path(&bin))];
        if any_keg_only {
            follow_up.push(("Opt", display_path(&store.prefix().join("opt"))));
        }
        if !service_names.is_empty() {
            follow_up.push((
                "Service",
                format!("{} (not started automatically)", service_names.join(", ")),
            ));
        }
        let path_value = std::env::var_os("PATH");
        if let Some(hint) = next_path_hint(
            path_value.as_deref().unwrap_or(std::ffi::OsStr::new("")),
            &bin,
            prefix.map(|_| store.prefix()),
        ) {
            follow_up.push(("Next", hint));
        }
        formatter.note(&formatter.fields(&follow_up));
        print_installed_caveats(&formatter, store.prefix(), &installed_caveats);
    }
    if !promotions.is_empty() {
        let names = promotions
            .iter()
            .map(|receipt| receipt.name.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        formatter.note(&format!(
            "{}: {names}",
            formatter.success("Marked as requested")
        ));
    }
    Ok(())
}

/// Print install caveats the way `info` does; when more than one requested
/// formula carries notes, each block is titled with its formula. Caveats are
/// human-readable, so plain mode sends them to stderr.
pub(super) fn print_installed_caveats(
    formatter: &DnfStyleFormatter,
    prefix: &Path,
    caveats: &[(&str, &str)],
) {
    match caveats {
        [] => {}
        [(_, text)] => {
            if let Some(block) =
                crate::commands::info::caveat_block(formatter, prefix, "Caveats", Some(text))
            {
                formatter.note(&block);
            }
        }
        _ => {
            for (name, text) in caveats {
                let title = format!("Caveats for {name}");
                if let Some(block) =
                    crate::commands::info::caveat_block(formatter, prefix, &title, Some(text))
                {
                    formatter.note(&block);
                }
            }
        }
    }
}

/// Pair each requested target with the planned formula it resolved to.
///
/// The plan only carries canonical names, so an alias (`nvim`) is mapped back
/// through the bundled catalog; the typed name is returned alongside the
/// formula when it differs from the canonical name.
pub(super) fn requested_installed<'a>(
    targets: &[String],
    planned: &'a [PlannedFormula],
) -> Result<Vec<(&'a PlannedFormula, Option<String>)>> {
    let catalog = crate::domain::portable::catalog()?;
    let mut matched: HashSet<&str> = HashSet::new();
    let mut paired = Vec::with_capacity(targets.len());
    for target in targets {
        let typed = plan::parse_target(target)?;
        let formula = planned
            .iter()
            .find(|formula| formula.name == typed)
            .or_else(|| {
                catalog
                    .find(typed)
                    .and_then(|package| planned.iter().find(|formula| formula.name == package.name))
            })
            .or_else(|| {
                planned
                    .iter()
                    .find(|formula| formula.requested && !matched.contains(formula.name.as_str()))
            });
        let Some(formula) = formula else {
            continue;
        };
        matched.insert(formula.name.as_str());
        let alias = (formula.name != typed).then(|| typed.to_string());
        paired.push((formula, alias));
    }
    Ok(paired)
}

pub(super) fn requested_promotions(plan: &[PlannedFormula], installed: &[Receipt]) -> Vec<Receipt> {
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
