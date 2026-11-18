//! `brew-rs info` — package metadata for formulae and casks.
//!
//! Homebrew remains the source of truth: metadata, install state and analytics
//! all come from `brew info --json=v2` / `brew info --analytics`, never from a
//! local inventory.

use std::collections::HashSet;

use anyhow::{Result, bail};
use serde::Deserialize;
use serde_json::Value;

use crate::brew::Brew;
use crate::config::Config;
use crate::ui::DnfStyleFormatter;

use super::PackageKind;

/// Handle the `info` command.
pub async fn handle_info(
    config: &Config,
    brew: Brew,
    package: &str,
    analytics: bool,
    deps: bool,
    kind: PackageKind,
) -> Result<()> {
    let formatter = DnfStyleFormatter::new(config);

    let mut args: Vec<&str> = vec!["--json=v2"];
    if let Some(flag) = kind.flag() {
        args.push(flag);
    }
    args.push("--");
    args.push(package);

    let info: InfoOutput = brew.json("info", &args).await?;

    if info.formulae.is_empty() && info.casks.is_empty() {
        bail!("No formula or cask named '{package}' was found");
    }

    let mut printed_any = false;
    for formula in &info.formulae {
        if printed_any {
            println!();
        }
        print_formula(&formatter, formula, deps);
        printed_any = true;
    }
    for cask in &info.casks {
        if printed_any {
            println!();
        }
        print_cask(&formatter, cask, deps);
        printed_any = true;
    }

    if analytics {
        println!();

        let mut args: Vec<&str> = vec!["--analytics"];
        if let Some(flag) = kind.flag() {
            args.push(flag);
        }
        args.push("--");
        args.push(package);

        // Homebrew owns the analytics tables and their API access. Analytics
        // are a read-only query, so suppress Homebrew's automatic update check
        // the same way `Brew::json` does for metadata.
        brew.run("info", &args, true).await?;
    }

    Ok(())
}

/// The JSON envelope emitted by `brew info --json=v2`.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct InfoOutput {
    formulae: Vec<FormulaInfo>,
    casks: Vec<CaskInfo>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct FormulaInfo {
    name: String,
    full_name: String,
    tap: Option<String>,
    desc: Option<String>,
    license: Option<String>,
    homepage: Option<String>,
    versions: FormulaVersions,
    installed: Vec<InstalledKeg>,
    linked_keg: Option<String>,
    pinned: bool,
    outdated: bool,
    deprecated: bool,
    disabled: bool,
    keg_only: bool,
    keg_only_reason: Option<KegOnlyReason>,
    caveats: Option<String>,
    dependencies: Vec<String>,
    build_dependencies: Vec<String>,
    recommended_dependencies: Vec<String>,
    optional_dependencies: Vec<String>,
    uses_from_macos: Vec<String>,
    conflicts_with: Vec<String>,
    conflicts_with_reasons: Vec<Option<String>>,
    aliases: Vec<String>,
    oldnames: Vec<String>,
    requirements: Vec<RequirementInfo>,
    revision: u64,
    deprecation_date: Option<String>,
    deprecation_reason: Option<String>,
    deprecation_replacement_formula: Option<String>,
    deprecation_replacement_cask: Option<String>,
    disable_date: Option<String>,
    disable_reason: Option<String>,
    disable_replacement_formula: Option<String>,
    disable_replacement_cask: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct FormulaVersions {
    stable: Option<String>,
    head: Option<String>,
    bottle: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct InstalledKeg {
    version: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct KegOnlyReason {
    reason: String,
    explanation: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RequirementInfo {
    name: String,
    cask: Option<String>,
    version: Value,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct CaskInfo {
    token: String,
    full_token: String,
    tap: Option<String>,
    name: Vec<String>,
    desc: Option<String>,
    homepage: Option<String>,
    version: String,
    installed: Option<String>,
    pinned: bool,
    pinned_version: Option<String>,
    outdated: bool,
    caveats: Option<String>,
    depends_on: Value,
    conflicts_with: Value,
    auto_updates: Option<bool>,
    deprecated: bool,
    deprecation_date: Option<String>,
    deprecation_reason: Option<String>,
    deprecation_replacement_formula: Option<String>,
    deprecation_replacement_cask: Option<String>,
    disabled: bool,
    disable_date: Option<String>,
    disable_reason: Option<String>,
    disable_replacement_formula: Option<String>,
    disable_replacement_cask: Option<String>,
}

impl FormulaInfo {
    fn is_installed(&self) -> bool {
        !self.installed.is_empty()
    }

    fn display_name(&self) -> &str {
        if self.name.is_empty() {
            &self.full_name
        } else {
            &self.name
        }
    }

    fn installed_summary(&self) -> String {
        let versions = self
            .installed
            .iter()
            .filter(|keg| !keg.version.trim().is_empty())
            .map(|keg| keg.version.clone())
            .collect::<Vec<_>>();

        let mut flags = Vec::new();
        if let Some(linked) = self.linked_keg.as_deref() {
            if versions.iter().any(|version| version == linked) {
                flags.push("linked");
            }
        }
        if self.outdated {
            flags.push("outdated");
        }

        let mut summary = versions.join(", ");
        if !flags.is_empty() {
            summary.push_str(&format!(" ({})", flags.join(", ")));
        }
        summary
    }
}

impl CaskInfo {
    fn is_installed(&self) -> bool {
        as_present(&self.installed).is_some()
    }

    fn display_name(&self) -> &str {
        if self.token.is_empty() {
            &self.full_token
        } else {
            &self.token
        }
    }
}

fn print_formula(formatter: &DnfStyleFormatter, formula: &FormulaInfo, show_dependencies: bool) {
    println!(
        "{}",
        formatter.section(if formula.is_installed() {
            "Installed Packages"
        } else {
            "Available Packages"
        })
    );
    print_fields(formatter, &formula_fields(formula));

    if show_dependencies {
        print_dependency_section(formatter, "Runtime Dependencies", &formula.dependencies);
        print_dependency_section(formatter, "Build Dependencies", &formula.build_dependencies);
        print_dependency_section(
            formatter,
            "Recommended Dependencies",
            &formula.recommended_dependencies,
        );
        print_dependency_section(
            formatter,
            "Optional Dependencies",
            &formula.optional_dependencies,
        );
        print_dependency_section(formatter, "macOS Dependencies", &formula.uses_from_macos);
    }

    if let Some(caveats) = as_present(&formula.caveats) {
        println!();
        println!("{}", formatter.section("Caveats"));
        println!("{}", caveats.trim_end());
    }
}

fn formula_fields(formula: &FormulaInfo) -> Vec<(&'static str, String)> {
    let mut fields = Vec::new();

    push_field(&mut fields, "Name", formula.display_name());
    if !formula.full_name.is_empty() && formula.full_name != formula.display_name() {
        push_field(&mut fields, "Full Name", &formula.full_name);
    }
    if let Some(stable) = as_present(&formula.versions.stable) {
        push_field(&mut fields, "Version", stable);
    }
    if formula.revision > 0 {
        push_field(&mut fields, "Revision", formula.revision);
    }
    if let Some(head) = as_present(&formula.versions.head) {
        // Homebrew reports a git HEAD as the literal string "HEAD".
        push_field(
            &mut fields,
            "Head",
            if head == "HEAD" { "yes" } else { head },
        );
    }
    if formula.is_installed() {
        push_field(&mut fields, "Installed", formula.installed_summary());
    }
    if let Some(tap) = as_present(&formula.tap) {
        push_field(&mut fields, "Repository", tap);
    }
    if let Some(description) = as_present(&formula.desc) {
        push_field(&mut fields, "Summary", description);
    }
    if let Some(homepage) = as_present(&formula.homepage) {
        push_field(&mut fields, "URL", homepage);
    }
    if let Some(license) = as_present(&formula.license) {
        push_field(&mut fields, "License", license);
    }
    if let Some(bottle) = formula.versions.bottle {
        push_field(&mut fields, "Bottle", if bottle { "yes" } else { "no" });
    }
    if formula.keg_only {
        let reason = formula
            .keg_only_reason
            .as_ref()
            .map(format_keg_only_reason)
            .filter(|reason| !reason.is_empty());
        match reason {
            Some(reason) => push_field(&mut fields, "Keg Only", format!("yes ({reason})")),
            None => push_field(&mut fields, "Keg Only", "yes"),
        }
    }
    if formula.pinned {
        push_field(&mut fields, "Pinned", "yes");
    }
    if let Some(status) = flag_status(
        formula.deprecated,
        &formula.deprecation_date,
        &formula.deprecation_reason,
        &formula.deprecation_replacement_formula,
        &formula.deprecation_replacement_cask,
    ) {
        push_field(&mut fields, "Deprecated", status);
    }
    if let Some(status) = flag_status(
        formula.disabled,
        &formula.disable_date,
        &formula.disable_reason,
        &formula.disable_replacement_formula,
        &formula.disable_replacement_cask,
    ) {
        push_field(&mut fields, "Disabled", status);
    }
    if !formula.aliases.is_empty() {
        push_field(&mut fields, "Aliases", formula.aliases.join(", "));
    }
    if !formula.oldnames.is_empty() {
        push_field(&mut fields, "Old Names", formula.oldnames.join(", "));
    }
    if let Some(conflicts) = format_formula_conflicts(formula) {
        push_field(&mut fields, "Conflicts", conflicts);
    }
    let requirements = formula
        .requirements
        .iter()
        .filter_map(format_requirement)
        .collect::<Vec<_>>();
    if !requirements.is_empty() {
        push_field(&mut fields, "Requirements", requirements.join(", "));
    }

    fields
}

fn print_cask(formatter: &DnfStyleFormatter, cask: &CaskInfo, show_dependencies: bool) {
    let dependencies = cask_dependencies(&cask.depends_on);

    println!(
        "{}",
        formatter.section(if cask.is_installed() {
            "Installed Packages"
        } else {
            "Available Packages"
        })
    );
    print_fields(formatter, &cask_fields(cask, &dependencies));

    if show_dependencies {
        print_dependency_section(formatter, "Formula Dependencies", &dependencies.formulae);
        print_dependency_section(formatter, "Cask Dependencies", &dependencies.casks);
    }

    if let Some(caveats) = as_present(&cask.caveats) {
        println!();
        println!("{}", formatter.section("Caveats"));
        println!("{}", caveats.trim_end());
    }
}

fn cask_fields(cask: &CaskInfo, dependencies: &CaskDependencies) -> Vec<(&'static str, String)> {
    let mut fields = Vec::new();

    push_field(&mut fields, "Name", cask.display_name());
    if !cask.full_token.is_empty() && cask.full_token != cask.display_name() {
        push_field(&mut fields, "Full Name", &cask.full_token);
    }
    if !cask.version.trim().is_empty() {
        push_field(&mut fields, "Version", &cask.version);
    }
    if let Some(installed) = as_present(&cask.installed) {
        let installed = if cask.outdated {
            format!("{installed} (outdated)")
        } else {
            installed.to_string()
        };
        push_field(&mut fields, "Installed", installed);
    }
    if let Some(tap) = as_present(&cask.tap) {
        push_field(&mut fields, "Repository", tap);
    }
    if let Some(description) = as_present(&cask.desc) {
        push_field(&mut fields, "Summary", description);
    }
    if !cask.name.is_empty() {
        push_field(&mut fields, "App Name", cask.name.join(", "));
    }
    if let Some(homepage) = as_present(&cask.homepage) {
        push_field(&mut fields, "URL", homepage);
    }
    if cask.auto_updates == Some(true) {
        push_field(&mut fields, "Auto Updates", "yes");
    }
    if cask.pinned {
        match as_present(&cask.pinned_version) {
            Some(version) => push_field(&mut fields, "Pinned", format!("yes ({version})")),
            None => push_field(&mut fields, "Pinned", "yes"),
        }
    }
    if let Some(status) = flag_status(
        cask.deprecated,
        &cask.deprecation_date,
        &cask.deprecation_reason,
        &cask.deprecation_replacement_formula,
        &cask.deprecation_replacement_cask,
    ) {
        push_field(&mut fields, "Deprecated", status);
    }
    if let Some(status) = flag_status(
        cask.disabled,
        &cask.disable_date,
        &cask.disable_reason,
        &cask.disable_replacement_formula,
        &cask.disable_replacement_cask,
    ) {
        push_field(&mut fields, "Disabled", status);
    }
    if !dependencies.requirements.is_empty() {
        push_field(
            &mut fields,
            "Requirements",
            dependencies.requirements.join(", "),
        );
    }
    if let Some(conflicts) = format_cask_conflicts(&cask.conflicts_with) {
        push_field(&mut fields, "Conflicts", conflicts);
    }

    fields
}

fn print_fields(formatter: &DnfStyleFormatter, fields: &[(&'static str, String)]) {
    for (label, value) in fields {
        println!("{}", formatter.field(label, value));
    }
}

fn push_field(
    fields: &mut Vec<(&'static str, String)>,
    label: &'static str,
    value: impl std::fmt::Display,
) {
    let value = value.to_string();
    if !value.trim().is_empty() {
        fields.push((label, value));
    }
}

fn print_dependency_section(formatter: &DnfStyleFormatter, title: &str, dependencies: &[String]) {
    let dependencies = unique_names(dependencies);
    if dependencies.is_empty() {
        return;
    }

    println!();
    println!(
        "{}",
        formatter.section(&format!("{title} ({})", dependencies.len()))
    );
    for dependency in dependencies {
        println!("  {} {}", formatter.bullet_available(), dependency);
    }
}

fn unique_names(names: &[String]) -> Vec<&str> {
    let mut seen = HashSet::new();
    names
        .iter()
        .map(String::as_str)
        .filter(|name| !name.trim().is_empty() && seen.insert(*name))
        .collect()
}

fn as_present(value: &Option<String>) -> Option<&str> {
    value.as_deref().filter(|text| !text.trim().is_empty())
}

fn format_keg_only_reason(reason: &KegOnlyReason) -> String {
    let summary = match reason.reason.trim_start_matches(':') {
        "shadowed_by_macos" => "shadowed by macOS".to_string(),
        "provided_by_macos" => "provided by macOS".to_string(),
        other => other.replace('_', " "),
    };
    let explanation = reason.explanation.trim();

    if explanation.is_empty() {
        summary
    } else if summary.is_empty() {
        explanation.to_string()
    } else {
        format!("{summary}: {explanation}")
    }
}

fn flag_status(
    flagged: bool,
    date: &Option<String>,
    reason: &Option<String>,
    replacement_formula: &Option<String>,
    replacement_cask: &Option<String>,
) -> Option<String> {
    if !flagged {
        return None;
    }

    let mut details = Vec::new();
    if let Some(reason) = as_present(reason) {
        details.push(reason.to_string());
    }
    if let Some(date) = as_present(date) {
        details.push(format!("since {date}"));
    }
    if let Some(replacement) =
        as_present(replacement_formula).or_else(|| as_present(replacement_cask))
    {
        details.push(format!("use {replacement}"));
    }

    Some(if details.is_empty() {
        "yes".to_string()
    } else {
        format!("yes ({})", details.join("; "))
    })
}

fn format_formula_conflicts(formula: &FormulaInfo) -> Option<String> {
    if formula.conflicts_with.is_empty() {
        return None;
    }

    let conflicts = formula
        .conflicts_with
        .iter()
        .enumerate()
        .map(|(index, name)| {
            let reason = formula
                .conflicts_with_reasons
                .get(index)
                .and_then(|reason| reason.as_deref())
                .map(str::trim)
                .filter(|reason| !reason.is_empty());

            match reason {
                Some(reason) => format!("{name} ({reason})"),
                None => name.clone(),
            }
        })
        .collect::<Vec<_>>();

    (!conflicts.is_empty()).then(|| conflicts.join(", "))
}

fn format_requirement(requirement: &RequirementInfo) -> Option<String> {
    if let Some(cask) = as_present(&requirement.cask) {
        return Some(format!("{cask} (cask)"));
    }
    if requirement.name.is_empty() {
        return None;
    }

    let label = match requirement.name.as_str() {
        "macos" | "maximum_macos" => "macOS",
        "linux" => "Linux",
        "xcode" => "Xcode",
        other => other,
    };
    let version = json_scalar(&requirement.version);

    Some(match (requirement.name.as_str(), version) {
        ("arch", Some(version)) => format!("{version} architecture"),
        ("macos", Some(version)) => format!("{label} >= {version}"),
        ("maximum_macos", Some(version)) => format!("{label} <= {version}"),
        (_, Some(version)) => format!("{label} {version}"),
        (_, None) => label.to_string(),
    })
}

fn json_scalar(value: &Value) -> Option<String> {
    match value {
        Value::Null => None,
        Value::String(text) if text.trim().is_empty() => None,
        Value::String(text) => Some(text.clone()),
        Value::Bool(flag) => Some(flag.to_string()),
        Value::Number(number) => Some(number.to_string()),
        Value::Array(items) => {
            let items = items.iter().filter_map(json_scalar).collect::<Vec<_>>();
            (!items.is_empty()).then(|| items.join(" / "))
        }
        Value::Object(_) => None,
    }
}

fn json_strings(value: &Value) -> Vec<String> {
    match value {
        Value::Array(items) => items.iter().filter_map(json_scalar).collect(),
        _ => json_scalar(value).into_iter().collect(),
    }
}

#[derive(Debug, Default)]
struct CaskDependencies {
    formulae: Vec<String>,
    casks: Vec<String>,
    requirements: Vec<String>,
}

fn cask_dependencies(depends_on: &Value) -> CaskDependencies {
    let mut dependencies = CaskDependencies::default();

    let Some(depends_on) = depends_on.as_object() else {
        return dependencies;
    };

    for (key, value) in depends_on {
        match key.as_str() {
            "formula" => dependencies.formulae = json_strings(value),
            "cask" => dependencies.casks = json_strings(value),
            "arch" => dependencies
                .requirements
                .extend(format_arch_requirements(value)),
            "macos" | "maximum_macos" => dependencies
                .requirements
                .extend(format_requirement_group("macOS", value)),
            "linux" => dependencies
                .requirements
                .extend(format_requirement_group("Linux", value)),
            _ => {}
        }
    }

    dependencies
}

fn format_requirement_group(label: &str, value: &Value) -> Vec<String> {
    match value {
        Value::Object(comparators) if comparators.is_empty() => vec![label.to_string()],
        Value::Object(comparators) => comparators
            .iter()
            .map(|(comparator, versions)| {
                let versions = json_strings(versions);
                if versions.is_empty() {
                    format!("{label} {comparator}")
                } else {
                    format!("{label} {comparator} {}", versions.join(" / "))
                }
            })
            .collect(),
        _ => match json_scalar(value) {
            Some(version) => vec![format!("{label} {version}")],
            None => vec![label.to_string()],
        },
    }
}

fn format_arch_requirements(value: &Value) -> Vec<String> {
    let Some(arches) = value.as_array() else {
        return Vec::new();
    };

    let mut requirements = Vec::new();
    for arch in arches {
        let Some(arch) = arch.as_object() else {
            if let Some(name) = json_scalar(arch) {
                requirements.push(name);
            }
            continue;
        };

        let name = match arch.get("type").and_then(Value::as_str) {
            Some("arm") => "ARM",
            Some("intel") => "Intel",
            Some(other) => other,
            None => "Architecture",
        };
        requirements.push(match arch.get("bits").and_then(Value::as_u64) {
            Some(bits) => format!("{name} {bits}-bit"),
            None => name.to_string(),
        });
    }

    requirements
}

fn format_cask_conflicts(conflicts_with: &Value) -> Option<String> {
    let conflicts = match conflicts_with {
        Value::Object(kinds) => kinds
            .iter()
            .flat_map(|(kind, names)| {
                json_strings(names)
                    .into_iter()
                    .map(|name| format!("{name} ({kind})"))
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>(),
        _ => json_strings(conflicts_with),
    };

    (!conflicts.is_empty()).then(|| conflicts.join(", "))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn field<'a>(fields: &'a [(&'static str, String)], label: &str) -> Option<&'a str> {
        fields
            .iter()
            .find(|(name, _)| *name == label)
            .map(|(_, value)| value.as_str())
    }

    #[test]
    fn empty_envelope_is_valid() {
        let info: InfoOutput =
            serde_json::from_str(r#"{"formulae":[],"casks":[]}"#).expect("valid envelope");
        assert!(info.formulae.is_empty());
        assert!(info.casks.is_empty());
    }

    #[test]
    fn parses_formula_json_and_formats_metadata() {
        let info: InfoOutput = serde_json::from_str(
            r#"{
              "formulae": [{
                "name": "readline",
                "full_name": "readline",
                "tap": "homebrew/core",
                "desc": "Library for editing typed command lines",
                "homepage": "https://tiswww.case.edu/php/chet/readline/rltop.html",
                "license": "GPL-3.0-or-later",
                "versions": {"stable": "8.3.6", "head": "HEAD", "bottle": true},
                "installed": [{"version": "8.3.6"}],
                "linked_keg": "8.3.6",
                "pinned": false,
                "outdated": false,
                "keg_only": true,
                "keg_only_reason": {
                  "reason": ":shadowed_by_macos",
                  "explanation": "macOS provides BSD libedit"
                },
                "dependencies": ["ncurses"],
                "build_dependencies": ["pkgconf"],
                "uses_from_macos": [],
                "conflicts_with": ["libedit", "openssl@1.1"],
                "conflicts_with_reasons": ["because macOS provides libedit", null],
                "aliases": [],
                "oldnames": [],
                "requirements": [],
                "revision": 1,
                "deprecated": false,
                "disabled": false
              }],
              "casks": []
            }"#,
        )
        .expect("valid formula JSON");

        let formula = &info.formulae[0];
        assert_eq!(formula.name, "readline");
        assert_eq!(formula.installed_summary(), "8.3.6 (linked)");
        assert_eq!(
            formula
                .keg_only_reason
                .as_ref()
                .map(format_keg_only_reason)
                .as_deref(),
            Some("shadowed by macOS: macOS provides BSD libedit")
        );
        assert_eq!(
            format_formula_conflicts(formula).as_deref(),
            Some("libedit (because macOS provides libedit), openssl@1.1")
        );

        let fields = formula_fields(formula);
        assert_eq!(field(&fields, "Name"), Some("readline"));
        assert_eq!(field(&fields, "Version"), Some("8.3.6"));
        assert_eq!(field(&fields, "Revision"), Some("1"));
        assert_eq!(field(&fields, "Installed"), Some("8.3.6 (linked)"));
        assert_eq!(field(&fields, "Repository"), Some("homebrew/core"));
        assert_eq!(field(&fields, "License"), Some("GPL-3.0-or-later"));
        assert_eq!(field(&fields, "Bottle"), Some("yes"));
        assert_eq!(
            field(&fields, "Keg Only"),
            Some("yes (shadowed by macOS: macOS provides BSD libedit)")
        );
        assert_eq!(
            field(&fields, "Conflicts"),
            Some("libedit (because macOS provides libedit), openssl@1.1")
        );
    }

    #[test]
    fn flagged_status_includes_reason_date_and_replacement() {
        let status = flag_status(
            true,
            &Some("2024-01-01".into()),
            &Some("unmaintained".into()),
            &Some("new-pkg".into()),
            &None,
        );

        assert_eq!(
            status.as_deref(),
            Some("yes (unmaintained; since 2024-01-01; use new-pkg)")
        );
        assert_eq!(
            flag_status(
                false,
                &Some("2024-01-01".into()),
                &Some("unmaintained".into()),
                &None,
                &None
            ),
            None
        );
    }

    #[test]
    fn installed_summary_combines_linked_and_outdated_flags() {
        let formula = FormulaInfo {
            installed: vec![InstalledKeg {
                version: "14.5.0".into(),
            }],
            linked_keg: Some("14.5.0".into()),
            outdated: true,
            ..Default::default()
        };

        assert_eq!(formula.installed_summary(), "14.5.0 (linked, outdated)");
    }

    #[test]
    fn formats_requirement_variants() {
        let macos = RequirementInfo {
            name: "macos".into(),
            cask: None,
            version: Value::from("11"),
        };
        let arch = RequirementInfo {
            name: "arch".into(),
            cask: None,
            version: Value::from("arm64"),
        };
        let java = RequirementInfo {
            name: "java".into(),
            cask: Some("openjdk".into()),
            version: Value::Null,
        };

        assert_eq!(format_requirement(&macos).as_deref(), Some("macOS >= 11"));
        assert_eq!(
            format_requirement(&arch).as_deref(),
            Some("arm64 architecture")
        );
        assert_eq!(format_requirement(&java).as_deref(), Some("openjdk (cask)"));
    }

    #[test]
    fn parses_cask_json_and_dependency_shapes() {
        let info: InfoOutput = serde_json::from_str(
            r#"{
              "formulae": [],
              "casks": [{
                "token": "firefox",
                "full_token": "firefox",
                "tap": "homebrew/cask",
                "name": ["Mozilla Firefox"],
                "desc": "Web browser",
                "homepage": "https://www.mozilla.org/firefox/",
                "version": "160.0",
                "installed": "159.0",
                "pinned": false,
                "outdated": true,
                "auto_updates": true,
                "depends_on": {
                  "macos": {">=": ["13"]},
                  "arch": [{"type": "arm", "bits": 64}],
                  "formula": ["openssl@3"],
                  "cask": ["some-cask"]
                },
                "conflicts_with": {"cask": ["firefox@beta"]},
                "caveats": null,
                "deprecated": false,
                "disabled": false
              }]
            }"#,
        )
        .expect("valid cask JSON");

        let cask = &info.casks[0];
        assert!(cask.is_installed());
        assert_eq!(cask.installed.as_deref(), Some("159.0"));

        let dependencies = cask_dependencies(&cask.depends_on);
        assert_eq!(dependencies.formulae, vec!["openssl@3"]);
        assert_eq!(dependencies.casks, vec!["some-cask"]);
        assert!(
            dependencies
                .requirements
                .iter()
                .any(|req| req == "macOS >= 13")
        );
        assert!(
            dependencies
                .requirements
                .iter()
                .any(|req| req == "ARM 64-bit")
        );
        assert_eq!(
            format_cask_conflicts(&cask.conflicts_with).as_deref(),
            Some("firefox@beta (cask)")
        );

        let fields = cask_fields(cask, &dependencies);
        assert_eq!(field(&fields, "Name"), Some("firefox"));
        assert_eq!(field(&fields, "Version"), Some("160.0"));
        assert_eq!(field(&fields, "Installed"), Some("159.0 (outdated)"));
        assert_eq!(field(&fields, "App Name"), Some("Mozilla Firefox"));
        assert_eq!(field(&fields, "Auto Updates"), Some("yes"));
        assert_eq!(field(&fields, "Repository"), Some("homebrew/cask"));
        assert_eq!(
            field(&fields, "Requirements"),
            Some("ARM 64-bit, macOS >= 13")
        );
    }

    #[test]
    fn bare_macos_dependency_and_empty_depends_on_are_handled() {
        let dependencies = cask_dependencies(&serde_json::json!({"macos": {}}));
        assert_eq!(dependencies.requirements, vec!["macOS"]);

        let empty = cask_dependencies(&Value::Null);
        assert!(empty.formulae.is_empty());
        assert!(empty.casks.is_empty());
        assert!(empty.requirements.is_empty());
    }
}
