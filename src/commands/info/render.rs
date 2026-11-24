use std::path::Path;

use crate::config::Config;
use crate::ui::DnfStyleFormatter;

use super::analytics::{InlineAnalytics, inline_analytics_rows};
use super::fields::{
    cask_conflicts_value, cask_dependency_rows, clean_block, formula_dependency_rows, one_line,
};
use super::installed::{
    Installed, InstalledCask, describe_installed, describe_installed_cask, read_installed,
    read_installed_cask,
};
use super::request::Kind;
use super::response::{CaskResponse, FormulaResponse};
use super::source::{CASK_TAP, CORE_TAP, Metadata};
use super::status::{
    cask_display_name, cask_status_messages, cask_version, formula_status_messages,
    formula_versions, keg_only_value, optional_text, tap_or,
};

pub(super) fn print_info(
    config: &Config,
    prefix: &Path,
    metadata: &Metadata,
    deps: bool,
    show_analytics: bool,
) {
    let formatter = DnfStyleFormatter::new(config);
    println!(
        "{} {}",
        formatter.header(&metadata.name),
        formatter.info(&format!("({})", metadata.kind.label()))
    );
    match metadata.kind {
        Kind::Formula => {
            if let Some(formula) = metadata.formula.as_ref() {
                print_formula(&formatter, prefix, metadata, formula, deps, show_analytics);
            }
        }
        Kind::Cask => {
            if let Some(cask) = metadata.cask.as_ref() {
                print_cask(&formatter, prefix, metadata, cask, deps, show_analytics);
            }
        }
    }
}

pub(super) fn print_formula(
    formatter: &DnfStyleFormatter,
    prefix: &Path,
    metadata: &Metadata,
    formula: &FormulaResponse,
    deps: bool,
    show_analytics: bool,
) {
    let revision = formula.revision.unwrap_or(0);

    println!(
        "{}",
        formatter.field("Repository", &tap_or(&formula.tap, CORE_TAP))
    );
    println!(
        "{}",
        formatter.field(
            "Description",
            optional_text(formula.desc.as_deref(), "none")
        )
    );
    println!(
        "{}",
        formatter.field("Versions", &formula_versions(formula))
    );
    println!(
        "{}",
        formatter.field(
            "Bottle",
            if formula.versions.bottle {
                "available"
            } else {
                "none (source only)"
            }
        )
    );
    println!(
        "{}",
        formatter.field(
            "License",
            optional_text(formula.license.as_deref(), "not specified")
        )
    );
    println!(
        "{}",
        formatter.field(
            "Homepage",
            optional_text(formula.homepage.as_deref(), "none")
        )
    );

    if !formula.full_name.trim().is_empty() && formula.full_name != metadata.name {
        println!("{}", formatter.field("Full name", &formula.full_name));
    }
    if !formula.aliases.is_empty() {
        println!(
            "{}",
            formatter.field("Aliases", &formula.aliases.join(", "))
        );
    }
    if !formula.oldnames.is_empty() {
        println!(
            "{}",
            formatter.field("Old names", &formula.oldnames.join(", "))
        );
    }
    if formula.keg_only {
        println!("{}", formatter.field("Keg-only", &keg_only_value(formula)));
    }
    if !formula.conflicts_with.is_empty() {
        println!(
            "{}",
            formatter.field("Conflicts", &formula.conflicts_with.join(", "))
        );
    }

    let installed = read_installed(Some(prefix), &metadata.name);
    if let Installed::Unknown(reason) = &installed {
        eprintln!(
            "{} could not read brewrs receipts: {}",
            formatter.warning("warning:"),
            one_line(reason)
        );
    }
    println!(
        "{}",
        formatter.field(
            "Installed",
            &describe_installed(&installed, formula.versions.stable.as_deref(), revision)
        )
    );

    for message in formula_status_messages(formula, &metadata.name) {
        eprintln!("{} {}", formatter.warning("warning:"), one_line(&message));
    }

    if deps {
        print_dependencies(formatter, &formula_dependency_rows(formula));
    }
    if show_analytics {
        print_inline_analytics(formatter, metadata, formula.analytics.as_ref());
    }
    print_caveats(formatter, prefix, formula.caveats.as_deref());
}

pub(super) fn print_cask(
    formatter: &DnfStyleFormatter,
    prefix: &Path,
    metadata: &Metadata,
    cask: &CaskResponse,
    deps: bool,
    show_analytics: bool,
) {
    let display_name = cask_display_name(cask);
    if let Some(name) = display_name {
        println!("{}", formatter.field("Name", &name));
    }
    println!(
        "{}",
        formatter.field("Repository", &tap_or(&cask.tap, CASK_TAP))
    );
    println!(
        "{}",
        formatter.field("Description", optional_text(cask.desc.as_deref(), "none"))
    );
    println!("{}", formatter.field("Versions", &cask_version(cask)));
    println!(
        "{}",
        formatter.field("Homepage", optional_text(cask.homepage.as_deref(), "none"))
    );

    if !cask.full_token.trim().is_empty() && cask.full_token != metadata.name {
        println!("{}", formatter.field("Full token", &cask.full_token));
    }
    if !cask.old_tokens.is_empty() {
        println!(
            "{}",
            formatter.field("Old tokens", &cask.old_tokens.join(", "))
        );
    }

    if let Some(conflicts) = cask_conflicts_value(&cask.conflicts_with) {
        println!("{}", formatter.field("Conflicts", &conflicts));
    }
    let installed = read_installed_cask(Some(prefix), &metadata.name);
    if let InstalledCask::Unknown(reason) = &installed {
        eprintln!(
            "{} could not read brewrs cask receipts: {}",
            formatter.warning("warning:"),
            one_line(reason)
        );
    }
    println!(
        "{}",
        formatter.field(
            "Installed",
            &describe_installed_cask(&installed, &cask.version)
        )
    );

    for message in cask_status_messages(cask, &metadata.name) {
        eprintln!("{} {}", formatter.warning("warning:"), one_line(&message));
    }

    if deps {
        print_dependencies(formatter, &cask_dependency_rows(&cask.depends_on));
    }
    if show_analytics {
        print_inline_analytics(formatter, metadata, cask.analytics.as_ref());
    }
    print_caveats(formatter, prefix, cask.caveats.as_deref());
}

pub(super) fn print_dependencies(formatter: &DnfStyleFormatter, rows: &[Vec<String>]) {
    if rows.is_empty() {
        println!("{}", formatter.field("Dependencies", "none"));
        return;
    }
    println!();
    println!(
        "{}",
        formatter.table(
            &format!("Dependencies ({})", rows.len()),
            &["Kind", "Dependency"],
            rows
        )
    );
}

pub(super) fn print_inline_analytics(
    formatter: &DnfStyleFormatter,
    metadata: &Metadata,
    analytics: Option<&InlineAnalytics>,
) {
    let rows = analytics
        .map(|analytics| inline_analytics_rows(&metadata.name, analytics))
        .unwrap_or_default();
    if rows.is_empty() {
        println!(
            "{}",
            formatter.field("Analytics", "no install data published for this package")
        );
        return;
    }
    println!();
    println!(
        "{}",
        formatter.table(
            "Analytics (Homebrew install events)",
            &["Period", "Installs", "Variants"],
            &rows
        )
    );
}

pub(in crate::commands) fn print_caveats(
    formatter: &DnfStyleFormatter,
    prefix: &Path,
    caveats: Option<&str>,
) {
    if let Some(block) = caveat_block(formatter, prefix, "Caveats", caveats) {
        println!("{block}");
    }
}

/// The rendered caveat block, if the package has non-empty caveats.
///
/// Callers that route human-readable output to stderr (an install run in plain
/// mode) print this through the formatter's mode-aware `note` helper.
pub(in crate::commands) fn caveat_block(
    formatter: &DnfStyleFormatter,
    prefix: &Path,
    title: &str,
    caveats: Option<&str>,
) -> Option<String> {
    let text = caveats.map(str::trim).filter(|text| !text.is_empty())?;
    let text = localize_caveats(text, prefix);
    Some(format!(
        "\n{}\n{}",
        formatter.section(title),
        clean_block(&text)
    ))
}

/// Replace Homebrew prefix placeholders in caveat text with the brewrs prefix.
///
/// Caveats quote shell snippets, so the prefix stays absolute: `~` does not
/// expand inside double quotes.
pub(super) fn localize_caveats(text: &str, prefix: &Path) -> String {
    let prefix = prefix.display().to_string();
    let cellar = format!("{prefix}/Cellar");
    text.replace("${HOMEBREW_PREFIX}", &prefix)
        .replace("$HOMEBREW_PREFIX", &prefix)
        .replace("${HOMEBREW_CELLAR}", &cellar)
        .replace("$HOMEBREW_CELLAR", &cellar)
        .replace("$(brew --prefix)", &prefix)
}
