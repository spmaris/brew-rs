use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{self, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::{
    config::Config,
    ui::{
        DnfStyleFormatter,
        table::{Row, Section, display_path},
    },
};

use super::plan::PlannedFormula;
pub(crate) use super::plan::SourceFlag;
use super::portable::Catalog;
use super::store::{
    Dependency, PackageSource, Prepared, Receipt, Store, validate_conflicts,
    validate_dependencies_with_retained, validate_retained_pin_closure,
};
use super::{archive, default_target, download, plan, relocate};

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
    let store = Store::open(&path, false)?;
    let tag = default_target()?;
    let existing = store.installed()?;
    let retained = store.retained()?;
    let existing_casks = super::cask_store::installed(&store)?;
    let formatter = DnfStyleFormatter::new(config);
    let remaining;
    let packages = if source.is_none() {
        let (rest, notes) =
            partition_upstream_installed(super::portable::catalog()?, packages, &existing);
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
        Some(SourceFlag::Upstream) => super::portable::catalog()?.plan_targets(packages, &tag)?,
        Some(SourceFlag::Homebrew) | None => plan::resolve(config, packages, &tag).await?,
    };
    validate_sources(&plan, &existing)?;
    let pending = pending_packages(&plan, &existing, &retained).map_err(|error| {
        with_upstream_hint(super::portable::catalog(), error, &plan, &existing, &tag)
    })?;
    let mut projected = existing.clone();
    for item in &pending {
        projected.push(item.receipt.clone());
    }
    super::cask_store::validate_cask_conflicts(&existing_casks, &projected)?;
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
                return Err(error);
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
                return Err(error);
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

/// The exceptional note shown for a plan row, or an empty string.
fn formula_note(keg_only: bool) -> String {
    if keg_only {
        "keg-only".to_string()
    } else {
        String::new()
    }
}

/// The confirmation question for an install with pending packages and/or
/// promotions; `confirm` appends the `[y/N]` suffix.
fn install_question(pending: usize, promotions: usize) -> String {
    match (pending, promotions) {
        (0, count) => format!(
            "Mark {count} {} as requested?",
            plural(count, "package", "packages")
        ),
        (count, 0) => format!("Install {count} {}?", plural(count, "package", "packages")),
        (installs, marks) => format!(
            "Install {installs} {} and mark {marks} as requested?",
            plural(installs, "package", "packages")
        ),
    }
}

/// `1 package`, or `2 packages (1 requested, 1 dependency)`; the breakdown is
/// dropped when nothing was pulled in as a dependency.
fn package_count(requested: usize, dependencies: usize) -> String {
    let count = requested + dependencies;
    let total = format!("{count} {}", plural(count, "package", "packages"));
    if dependencies == 0 {
        total
    } else {
        format!(
            "{total} ({requested} requested, {dependencies} {})",
            plural(dependencies, "dependency", "dependencies")
        )
    }
}

fn plural<'a>(count: usize, one: &'a str, many: &'a str) -> &'a str {
    if count == 1 { one } else { many }
}

/// Comma-joined items, keeping the first four and summarising the rest as
/// `, and N more` so a wide result line stays readable.
fn limited_list(items: &[String]) -> String {
    const LIMIT: usize = 4;
    if items.len() <= LIMIT {
        return items.join(", ");
    }
    format!(
        "{}, and {} more",
        items[..LIMIT].join(", "),
        items.len() - LIMIT
    )
}

/// The `Installed ...` outcome text: each requested formula as `name version`,
/// plus a dependency count when the plan also pulled in non-requested formulae.
fn installed_summary(requested: &[String], dependencies: usize) -> String {
    if requested.is_empty() {
        return format!(
            "Installed {dependencies} {}",
            plural(dependencies, "package", "packages")
        );
    }
    let mut text = format!("Installed {}", limited_list(requested));
    if dependencies != 0 {
        text.push_str(&format!(
            " (+{dependencies} {})",
            plural(dependencies, "dependency", "dependencies")
        ));
    }
    text
}

/// The `Upgraded ...` outcome text, with each `name old → new` transition.
fn upgraded_summary(upgraded: &[String]) -> String {
    format!("Upgraded {}", limited_list(upgraded))
}

/// The `Removed ...` outcome text. Explicitly requested removals are named and
/// any autoremoved leftovers are counted; a pure autoremove names its packages.
fn removed_summary(requested: &[String], unused: &[String]) -> String {
    if requested.is_empty() {
        return format!("Removed {}", limited_list(unused));
    }
    let mut text = format!("Removed {}", limited_list(requested));
    if !unused.is_empty() {
        text.push_str(&format!(
            " (+{} unused {})",
            unused.len(),
            plural(unused.len(), "dependency", "dependencies")
        ));
    }
    text
}

/// Whether any `$PATH` entry names `bin`, matching by exact text or by
/// canonical path so a symlinked prefix entry still counts.
fn path_includes(path_value: &std::ffi::OsStr, bin: &Path) -> bool {
    let canonical = fs::canonicalize(bin).ok();
    std::env::split_paths(path_value).any(|entry| {
        entry == bin
            || canonical
                .as_ref()
                .is_some_and(|bin| fs::canonicalize(&entry).is_ok_and(|entry| entry == *bin))
    })
}

/// The follow-up hint shown after an install when `$PATH` does not reach the
/// prefix's `bin`: the shellenv command that adds it.
fn next_path_hint(
    path_value: &std::ffi::OsStr,
    bin: &Path,
    prefix: Option<&Path>,
) -> Option<String> {
    if path_includes(path_value, bin) {
        return None;
    }
    let command = match prefix {
        Some(prefix) => format!(
            "eval \"$(brewrs shellenv --prefix {})\"",
            posix_quote(&prefix.to_string_lossy())
        ),
        None => "eval \"$(brewrs shellenv)\"".to_string(),
    };
    Some(format!("{command}  (add it to ~/.zprofile to keep it)"))
}

/// Print install caveats the way `info` does; when more than one requested
/// formula carries notes, each block is titled with its formula. Caveats are
/// human-readable, so plain mode sends them to stderr.
fn print_installed_caveats(formatter: &DnfStyleFormatter, prefix: &Path, caveats: &[(&str, &str)]) {
    match caveats {
        [] => {}
        [(_, text)] => {
            if let Some(block) = super::info::caveat_block(formatter, prefix, "Caveats", Some(text))
            {
                formatter.note(&block);
            }
        }
        _ => {
            for (name, text) in caveats {
                let title = format!("Caveats for {name}");
                if let Some(block) =
                    super::info::caveat_block(formatter, prefix, &title, Some(text))
                {
                    formatter.note(&block);
                }
            }
        }
    }
}

/// Split install targets into those still to resolve and the notices for
/// catalog packages already installed from upstream. Without `--source` the
/// switch error would be useless advice, so these are reported as satisfied.
fn partition_upstream_installed(
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

/// Pair each requested target with the planned formula it resolved to.
///
/// The plan only carries canonical names, so an alias (`nvim`) is mapped back
/// through the bundled catalog; the typed name is returned alongside the
/// formula when it differs from the canonical name.
fn requested_installed<'a>(
    targets: &[String],
    planned: &'a [PlannedFormula],
) -> Result<Vec<(&'a PlannedFormula, Option<String>)>> {
    let catalog = super::portable::catalog()?;
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

pub(super) async fn fetch_formula(config: &Config, formula: &PlannedFormula) -> Result<PathBuf> {
    let download = match formula.source {
        PackageSource::Upstream => super::portable::fetch(config, formula).await,
        PackageSource::Homebrew => download::fetch(config, &formula.url, &formula.sha256).await,
    };
    download.with_context(|| format!("Failed to fetch {} {}", formula.name, formula.version))
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
    match candidate.formula.source {
        PackageSource::Upstream => super::portable::stage(candidate.formula, bottle, stage.path())
            .with_context(|| {
                format!(
                    "Failed to stage upstream {} {}",
                    receipt.name, receipt.version
                )
            })?,
        PackageSource::Homebrew => archive::unpack(
            bottle,
            stage.path(),
            &receipt.name,
            &receipt.version,
            &receipt.sha256,
        )
        .with_context(|| format!("Failed to stage {} {}", receipt.name, receipt.version))?,
    }
    if receipt.source == PackageSource::Homebrew && receipt.name == "nvm" {
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
        source: formula.source,
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
async fn resolve_changes(
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
        super::portable::catalog()?
            .plan_targets(&upstream, &tag)
            .context("Cannot plan upgrades for upstream-installed packages")?
    };
    if !homebrew.is_empty() {
        plan.extend(plan::resolve(config, &homebrew, &tag).await?);
    }
    validate_sources(&plan, installed)?;
    let changes = planned_changes(&plan, installed, selected, retained)?;
    Ok((tag, changes))
}

/// Reject plans that mix sources for one package or let an upstream build
/// stand in for a Homebrew dependency.
fn validate_sources(plan: &[PlannedFormula], installed: &[Receipt]) -> Result<()> {
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

/// A formula that cannot be installed natively.
#[derive(Debug)]
struct InstallBlocked(String);

impl std::fmt::Display for InstallBlocked {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for InstallBlocked {}

/// Append a hint when the blocker belongs to a requested target (or one of its
/// dependencies) that the reviewed upstream catalog can supply. The original
/// error is otherwise unchanged; nothing is ever installed from upstream
/// without `--source upstream`.
fn with_upstream_hint(
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

fn upgrade_rows(changes: &[UpgradeChange]) -> Vec<Row> {
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
        prefix.is_some(),
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
            return Err(error);
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

fn removal_plan(packages: &[String], installed: &[Receipt]) -> Result<Vec<Receipt>> {
    let mut selected = std::collections::HashSet::new();
    let mut pending = Vec::new();
    for target in packages {
        let name = plan::parse_target(target)?;
        // A catalog alias (e.g. `nvim`) names the upstream package only when no
        // package of that exact name is installed.
        let name = if installed.iter().any(|entry| entry.name == name) {
            name
        } else {
            super::portable::catalog()?
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

pub(crate) fn list(config: &Config, prefix: Option<&Path>) -> Result<()> {
    let store = Store::open(&prefix_path(prefix)?, false)?;
    let receipts = store.installed()?;
    let formatter = DnfStyleFormatter::new(config);
    if receipts.is_empty() {
        formatter.note(&format!("No packages in {}.", store.prefix().display()));
        return Ok(());
    }
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
    let formatter = DnfStyleFormatter::new(config);
    if formulae.is_empty() && casks.is_empty() {
        formatter.note(&format!("No packages in {}.", store.prefix().display()));
        return Ok(());
    }
    if formatter.boxed() {
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
    } else {
        render_inventory(&formatter, &formulae, &casks);
    }
    Ok(())
}

/// Plain `list`: formula and cask rows in one `Type Name Version Source Reason`
/// table so scripts see a single, stable schema.
fn render_inventory(
    formatter: &DnfStyleFormatter,
    formulae: &[Receipt],
    casks: &[super::cask_store::CaskReceipt],
) {
    let mut sorted: Vec<&Receipt> = formulae.iter().collect();
    sorted.sort_by(|left, right| left.name.cmp(&right.name));
    let mut rows: Vec<Vec<String>> = sorted
        .iter()
        .map(|receipt| {
            vec![
                "formula".to_string(),
                receipt.name.clone(),
                receipt.version.clone(),
                receipt.source.label().to_ascii_lowercase(),
                reason_token(receipt).to_string(),
            ]
        })
        .collect();
    let mut sorted_casks: Vec<&super::cask_store::CaskReceipt> = casks.iter().collect();
    sorted_casks.sort_by(|left, right| left.token.cmp(&right.token));
    for cask in sorted_casks {
        rows.push(vec![
            "cask".to_string(),
            cask.token.clone(),
            cask.version.clone(),
            "homebrew".to_string(),
            "requested".to_string(),
        ]);
    }
    println!(
        "{}",
        formatter.table(
            "Packages",
            &["Type", "Name", "Version", "Source", "Reason"],
            &rows
        )
    );
}

fn reason_token(receipt: &Receipt) -> &'static str {
    if receipt.requested {
        "requested"
    } else {
        "dependency"
    }
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
    let formatter = DnfStyleFormatter::new(config);
    if formulae.is_empty() && casks.is_empty() {
        formatter.note("No packages installed yet.");
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
        return super::cask_install::upgrade(config, prefix, packages, greedy, dry_run, yes, true)
            .await
            .map(|_| ());
    }
    if formula_only {
        return upgrade(config, prefix, packages, dry_run, yes, true)
            .await
            .map(|_| ());
    }
    let path = prefix_path(prefix)?;
    let store = Store::open(&path, false)?;
    let installed_formulae = store.installed()?;
    let installed_casks = super::cask_store::installed(&store)?;
    if packages.is_empty() {
        let formatter = DnfStyleFormatter::new(config);
        if installed_formulae.is_empty() && installed_casks.is_empty() {
            formatter.note("No packages installed yet.");
            return Ok(());
        }
        let both = !installed_formulae.is_empty() && !installed_casks.is_empty();
        let mut changed = false;
        if !installed_formulae.is_empty() {
            changed |= upgrade(config, prefix, &[], dry_run, yes, !both).await?;
        }
        if !installed_casks.is_empty() {
            changed |=
                super::cask_install::upgrade(config, prefix, &[], greedy, dry_run, yes, !both)
                    .await?;
        }
        if both && !changed {
            formatter.note("Everything is up to date.");
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

    let both = !formula_targets.is_empty() && !cask_targets.is_empty();
    let mut changed = false;
    if !formula_targets.is_empty() {
        changed |= upgrade(config, prefix, &formula_targets, dry_run, yes, !both).await?;
    }
    if !cask_targets.is_empty() {
        changed |= super::cask_install::upgrade(
            config,
            prefix,
            &cask_targets,
            greedy,
            dry_run,
            yes,
            !both,
        )
        .await?;
    }
    if both && !changed {
        DnfStyleFormatter::new(config).note("Everything is up to date.");
    }
    Ok(())
}

fn render_formula_list(formatter: &DnfStyleFormatter, mut receipts: Vec<Receipt>) {
    receipts.sort_by(|left, right| left.name.cmp(&right.name));
    let rows: Vec<_> = receipts
        .into_iter()
        .map(|receipt| {
            let reason = reason_token(&receipt).to_string();
            vec![
                receipt.name,
                receipt.version,
                receipt.source.label().to_ascii_lowercase(),
                reason,
            ]
        })
        .collect();
    let rendered = if formatter.boxed() {
        formatter.table(
            &format!("Packages ({})", rows.len()),
            &["Package", "Version", "Source", "Reason"],
            &rows,
        )
    } else {
        let rows: Vec<Vec<String>> = rows
            .into_iter()
            .map(|mut cells| {
                cells.insert(0, "formula".to_string());
                cells
            })
            .collect();
        formatter.table(
            "Packages",
            &["Type", "Name", "Version", "Source", "Reason"],
            &rows,
        )
    };
    println!("{rendered}");
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
fn blocker_reason(blocker: &str) -> Option<&'static str> {
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
fn requested_dependents<'a>(plan: &'a [PlannedFormula], blocked: &'a str) -> Vec<&'a str> {
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

pub(super) fn confirm(question: &str) -> Result<bool> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() || !io::stderr().is_terminal() {
        bail!("Confirmation requires a visible terminal; pass --assumeyes or --dry-run");
    }
    eprint!("{question} [y/N] ");
    io::stderr().flush()?;
    let mut answer = String::new();
    io::stdin().read_line(&mut answer)?;
    Ok(matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

#[cfg(test)]
mod tests;
