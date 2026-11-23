//! Native package information and install analytics.
//!
//! `info` and `analytics` read the official Homebrew JSON API at
//! `https://formulae.brew.sh/api` through [`HttpClient`]. Homebrew is never
//! executed, no Ruby is evaluated, and no package state is changed.
//!
//! # Namespaces and lookup order
//!
//! A bare name is looked up as a formula first. Only an *explicit* `HTTP 404`
//! from the formula endpoint triggers the cask lookup, so a network failure, a
//! 5xx response or a malformed payload is surfaced instead of being reported as
//! "package not found". When both namespaces contain the same name the formula
//! is shown deterministically; pass `--cask` (or `homebrew/cask/<token>`) to
//! inspect the cask. A tap-qualified identifier must use one of the two
//! official taps, `homebrew/core` or `homebrew/cask`, and must agree with an
//! explicit `--formula`/`--cask` flag.
//!
//! [`HttpClient`] preserves the typed `reqwest::StatusCode` on its error chain,
//! so the HTTP source classifies a failure as "not found" only when a
//! `reqwest::Error` in that chain carries `404 Not Found`. Error text is never
//! inspected, and every other failure is propagated with its context. When a
//! formula endpoint returns `404`, [`HttpMetadataSource`] checks the formula
//! catalog for an exact alias or old name before falling back to casks.
//!
//! # Data shown
//!
//! * Formula: description, tap, stable/head versions (with the `_revision`
//!   suffix), bottle availability, license, homepage, aliases, old names,
//!   keg-only state, conflicts, direct/recommended/optional/build dependencies,
//!   `uses_from_macos`, deprecation or disable state, caveats, and the inline
//!   per-period install counts published with the formula payload.
//! * Cask: human name, description, tap, version (and auto-updating state),
//!   homepage, old tokens, `depends_on`, conflicts, deprecation or disable
//!   state, caveats, and the inline install counts.
//! * Installed state: overlaid from native formula and cask receipts in the
//!   selected prefix.
//!
//! `info --analytics` renders the package payload's inline install map. The
//! standalone [`analytics`] function reads the tap-scoped analytics endpoint
//! (`/analytics/<category>/<tap>/<period>.json`), which adds the reporting
//! window and the tap total. Only the periods the API publishes (`30d`, `90d`
//! and `365d`) are accepted; anything else is rejected before a request.

use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::api::catalog_cache;
use crate::api::http_client::{HttpClient, is_not_found};
use crate::config::Config;
use crate::ui::DnfStyleFormatter;

use super::cask_plan::{CaskVersionOrder, compare_cask_versions};
use super::install::prefix_path;
use super::store::Store;

/// Official Homebrew JSON API base.
const API_BASE: &str = "https://formulae.brew.sh/api";
/// The only formula tap supported by the native backend.
const CORE_TAP: &str = "homebrew/core";
/// The only cask tap supported by the native backend.
const CASK_TAP: &str = "homebrew/cask";
/// Longest accepted formula name or cask token.
const MAX_NAME_LEN: usize = 128;
/// Longest accepted tap prefix.
const MAX_TAP_LEN: usize = 32;
/// Reporting periods published by the analytics API.
const ANALYTICS_PERIODS: [&str; 3] = ["30d", "90d", "365d"];

/// Which Homebrew namespace a request resolved to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Formula,
    Cask,
}

impl Kind {
    fn label(self) -> &'static str {
        match self {
            Self::Formula => "formula",
            Self::Cask => "cask",
        }
    }

    fn tap(self) -> &'static str {
        match self {
            Self::Formula => CORE_TAP,
            Self::Cask => CASK_TAP,
        }
    }

    /// Path segment used by the per-package endpoint.
    fn endpoint(self) -> &'static str {
        match self {
            Self::Formula => "formula",
            Self::Cask => "cask",
        }
    }

    /// Analytics category published for this namespace.
    fn analytics_category(self) -> &'static str {
        match self {
            Self::Formula => "install",
            Self::Cask => "cask-install",
        }
    }

    /// Tap spelling used in analytics endpoint paths.
    fn analytics_tap(self) -> &'static str {
        match self {
            Self::Formula => "homebrew-core",
            Self::Cask => "homebrew-cask",
        }
    }
}

impl std::fmt::Display for Kind {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.label())
    }
}

/// A validated package argument, with the namespace implied by an explicit tap.
struct Requested<'a> {
    name: &'a str,
    kind: Option<Kind>,
}

/// Show metadata for one formula or cask.
///
/// `formula` and `cask` mirror the CLI's `--formula`/`--cask` flags and are
/// mutually exclusive. When neither is set, a bare name is resolved as a
/// formula first and as a cask only after an explicit 404. `deps` adds the
/// dependency tables, and `analytics` adds the inline install counts.
pub(crate) async fn info(
    config: &Config,
    prefix: Option<&Path>,
    package: &str,
    formula: bool,
    cask: bool,
    deps: bool,
    analytics: bool,
) -> Result<()> {
    let requested = parse_package(package)?;
    let kind = requested_kind(package, &requested, formula, cask)?;
    let source = HttpMetadataSource::new(config)?;
    let metadata = lookup(&source, requested.name, kind).await?;
    print_info(config, prefix, &metadata, deps, analytics);
    Ok(())
}

/// Show tap-scoped install analytics for one formula or cask.
///
/// The namespace is inferred the same way as [`info`]: an explicit tap selects
/// it, otherwise the name is resolved as a formula first and as a cask only
/// after an explicit 404. Only `30d`, `90d` and `365d` are accepted.
pub(crate) async fn analytics(config: &Config, package: &str, period: &str) -> Result<()> {
    let period = validate_period(period)?;
    let requested = parse_package(package)?;
    let source = HttpMetadataSource::new(config)?;
    let metadata = lookup(&source, requested.name, requested.kind).await?;
    let response = source.analytics(metadata.kind, period).await?;
    print_analytics(config, &metadata, period, &response)
}

/// Validate a package argument and strip one of the official tap prefixes.
fn parse_package(raw: &str) -> Result<Requested<'_>> {
    let raw = raw.trim();
    if raw.is_empty() {
        bail!("Package name must not be empty");
    }
    if raw.len() > MAX_NAME_LEN + MAX_TAP_LEN + 2 {
        bail!("Package name '{raw}' is longer than the supported maximum");
    }

    let mut qualified = None;
    for (prefix, kind) in [(CORE_TAP, Kind::Formula), (CASK_TAP, Kind::Cask)] {
        let prefix = format!("{prefix}/");
        if let Some(rest) = raw.strip_prefix(&prefix) {
            qualified = Some((rest, kind));
            break;
        }
    }
    let (name, kind) = match qualified {
        Some((rest, kind)) => {
            if rest.contains('/') {
                bail!("Unsupported tap in '{raw}': only {CORE_TAP} and {CASK_TAP} are supported");
            }
            (rest, Some(kind))
        }
        None => {
            if raw.contains('/') {
                bail!("Unsupported tap in '{raw}': only {CORE_TAP} and {CASK_TAP} are supported");
            }
            (raw, None)
        }
    };

    validate_identifier(name, "package name")?;
    Ok(Requested { name, kind })
}

/// Combine an explicit tap with the CLI kind flags, rejecting contradictions.
fn requested_kind(
    raw: &str,
    requested: &Requested<'_>,
    formula: bool,
    cask: bool,
) -> Result<Option<Kind>> {
    if formula && cask {
        bail!("--formula and --cask are mutually exclusive");
    }
    let flag = if formula {
        Some(Kind::Formula)
    } else if cask {
        Some(Kind::Cask)
    } else {
        None
    };
    match (requested.kind, flag) {
        (Some(tap), Some(flag)) if tap != flag => bail!(
            "'{raw}' names a {tap} in {} but --{flag} was requested; pass the matching flag or drop --{flag}",
            tap.tap()
        ),
        (tap, flag) => Ok(tap.or(flag)),
    }
}

/// Accept only lowercase Homebrew identifiers and reject traversal and unsafe
/// characters before they can reach a URL.
fn validate_identifier<'a>(name: &'a str, what: &str) -> Result<&'a str> {
    if name.is_empty() || name.len() > MAX_NAME_LEN {
        bail!("{what} '{name}' must be between 1 and {MAX_NAME_LEN} characters");
    }
    if name.contains("..") {
        bail!("{what} '{name}' must not contain '..'");
    }
    let mut characters = name.chars();
    match characters.next() {
        Some(first) if first.is_ascii_lowercase() || first.is_ascii_digit() => {}
        _ => bail!("{what} '{name}' must start with a lowercase letter or digit"),
    }
    if !name.chars().all(|character| {
        character.is_ascii_lowercase()
            || character.is_ascii_digit()
            || matches!(character, '@' | '+' | '-' | '_' | '.')
    }) {
        bail!("{what} '{name}' may only contain lowercase letters, digits and '@+._-'");
    }
    Ok(name)
}

/// Why a per-package lookup produced no payload.
#[derive(Debug)]
enum LookupError {
    /// The endpoint returned an explicit HTTP 404.
    NotFound,
    /// Anything else: transport failure, 5xx, malformed JSON, ...
    Failed(anyhow::Error),
}

/// A seam over the package endpoints so the lookup rules and formatting can be
/// tested offline.
trait MetadataSource {
    async fn formula(&self, name: &str) -> std::result::Result<FormulaResponse, LookupError>;
    async fn cask(&self, token: &str) -> std::result::Result<CaskResponse, LookupError>;
    async fn analytics(&self, kind: Kind, period: &str) -> Result<AnalyticsResponse>;
}

/// [`MetadataSource`] backed by the official API over [`HttpClient`].
struct HttpMetadataSource {
    http: HttpClient,
    config: Config,
}

impl HttpMetadataSource {
    fn new(config: &Config) -> Result<Self> {
        Ok(Self {
            http: HttpClient::new(config)?,
            config: config.clone(),
        })
    }
}

impl MetadataSource for HttpMetadataSource {
    async fn formula(&self, name: &str) -> std::result::Result<FormulaResponse, LookupError> {
        let url = format!("{API_BASE}/{}/{name}.json", Kind::Formula.endpoint());
        match self.http.fetch_json(&url).await {
            Ok(payload) => Ok(payload),
            Err(error) => {
                if is_not_found(&error) {
                    if let Ok(Some(canonical)) =
                        catalog_cache::resolve_formula_alias(&self.config, name).await
                    {
                        if !canonical.contains('/')
                            && validate_identifier(&canonical, "formula alias").is_ok()
                        {
                            let alias_url =
                                format!("{API_BASE}/{}/{canonical}.json", Kind::Formula.endpoint());
                            return match self.http.fetch_json(&alias_url).await {
                                Ok(payload) => Ok(payload),
                                Err(alias_error) => {
                                    Err(classify_http_error(&alias_url, alias_error))
                                }
                            };
                        }
                    }
                }
                Err(classify_http_error(&url, error))
            }
        }
    }

    async fn cask(&self, token: &str) -> std::result::Result<CaskResponse, LookupError> {
        let url = format!("{API_BASE}/{}/{token}.json", Kind::Cask.endpoint());
        match self.http.fetch_json(&url).await {
            Ok(payload) => Ok(payload),
            Err(error) => Err(classify_http_error(&url, error)),
        }
    }

    async fn analytics(&self, kind: Kind, period: &str) -> Result<AnalyticsResponse> {
        let url = analytics_url(kind, period);
        self.http
            .fetch_json(&url)
            .await
            .with_context(|| format!("Could not fetch Homebrew analytics from {url}"))
    }
}

/// Turn a [`HttpClient`] failure into a typed lookup outcome.
///
/// Only a typed `404 Not Found` status is treated as "not found"; a transport
/// error, a 5xx status, a parse failure or an untyped message keeps its error
/// and its context.
fn classify_http_error(url: &str, error: anyhow::Error) -> LookupError {
    if is_not_found(&error) {
        LookupError::NotFound
    } else {
        LookupError::Failed(error.context(format!("GET {url} failed")))
    }
}

/// Build the tap-scoped analytics endpoint for a period.
fn analytics_url(kind: Kind, period: &str) -> String {
    format!(
        "{API_BASE}/analytics/{}/{}/{period}.json",
        kind.analytics_category(),
        kind.analytics_tap()
    )
}

/// Resolved package metadata.
#[derive(Debug)]
struct Metadata {
    kind: Kind,
    /// Canonical API name: the formula name or cask token.
    name: String,
    formula: Option<FormulaResponse>,
    cask: Option<CaskResponse>,
}

impl Metadata {
    fn from_formula(response: FormulaResponse, requested: &str) -> Result<Self> {
        if response.name.is_empty() {
            bail!("The Homebrew API returned formula metadata without a name for '{requested}'");
        }
        validate_identifier(&response.name, "formula name")?;
        Ok(Self {
            kind: Kind::Formula,
            name: response.name.clone(),
            formula: Some(response),
            cask: None,
        })
    }

    fn from_cask(response: CaskResponse, requested: &str) -> Result<Self> {
        if response.token.is_empty() {
            bail!("The Homebrew API returned cask metadata without a token for '{requested}'");
        }
        validate_identifier(&response.token, "cask token")?;
        Ok(Self {
            kind: Kind::Cask,
            name: response.token.clone(),
            formula: None,
            cask: Some(response),
        })
    }
}

/// Resolve metadata with the deterministic formula-first rule.
async fn lookup<S: MetadataSource>(source: &S, name: &str, kind: Option<Kind>) -> Result<Metadata> {
    match kind {
        Some(Kind::Formula) => formula_metadata(source, name).await,
        Some(Kind::Cask) => cask_metadata(source, name).await,
        None => match source.formula(name).await {
            Ok(formula) => Metadata::from_formula(formula, name),
            // Only a real 404 permits the cask fallback.
            Err(LookupError::NotFound) => match source.cask(name).await {
                Ok(cask) => Metadata::from_cask(cask, name),
                Err(LookupError::NotFound) => {
                    bail!(
                        "No formula or cask named '{name}' was found in the official Homebrew API"
                    )
                }
                Err(LookupError::Failed(error)) => {
                    Err(error.context(format!("Could not fetch cask '{name}' from {CASK_TAP}")))
                }
            },
            Err(LookupError::Failed(error)) => {
                Err(error.context(format!("Could not fetch formula '{name}' from {CORE_TAP}")))
            }
        },
    }
}

async fn formula_metadata<S: MetadataSource>(source: &S, name: &str) -> Result<Metadata> {
    match source.formula(name).await {
        Ok(formula) => Metadata::from_formula(formula, name),
        Err(LookupError::NotFound) => {
            bail!("No formula named '{name}' was found in {CORE_TAP}")
        }
        Err(LookupError::Failed(error)) => {
            Err(error.context(format!("Could not fetch formula '{name}' from {CORE_TAP}")))
        }
    }
}

async fn cask_metadata<S: MetadataSource>(source: &S, token: &str) -> Result<Metadata> {
    match source.cask(token).await {
        Ok(cask) => Metadata::from_cask(cask, token),
        Err(LookupError::NotFound) => {
            bail!("No cask named '{token}' was found in {CASK_TAP}")
        }
        Err(LookupError::Failed(error)) => {
            Err(error.context(format!("Could not fetch cask '{token}' from {CASK_TAP}")))
        }
    }
}

/// Reject analytics periods outside the three the API publishes.
fn validate_period(period: &str) -> Result<&str> {
    let period = period.trim();
    if ANALYTICS_PERIODS.contains(&period) {
        return Ok(period);
    }
    bail!(
        "unsupported analytics period '{period}'; the Homebrew API publishes {} — choose one of: {}",
        ANALYTICS_PERIODS.len(),
        ANALYTICS_PERIODS.join(", ")
    )
}

fn print_info(
    config: &Config,
    prefix: Option<&Path>,
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

fn print_formula(
    formatter: &DnfStyleFormatter,
    prefix: Option<&Path>,
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

    let installed = read_installed(prefix, &metadata.name);
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
    print_caveats(formatter, formula.caveats.as_deref());
}

fn print_cask(
    formatter: &DnfStyleFormatter,
    prefix: Option<&Path>,
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
    let installed = read_installed_cask(prefix, &metadata.name);
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
    print_caveats(formatter, cask.caveats.as_deref());
}

fn print_dependencies(formatter: &DnfStyleFormatter, rows: &[Vec<String>]) {
    if rows.is_empty() {
        println!("{}", formatter.field("Dependencies", "none"));
        return;
    }
    println!();
    println!("{}", formatter.section("Dependencies"));
    println!("{}", formatter.table(&["Kind", "Dependency"], rows));
}

fn print_inline_analytics(
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
        formatter.section("Analytics (Homebrew install events)")
    );
    println!(
        "{}",
        formatter.table(&["Period", "Installs", "Variants"], &rows)
    );
}

fn print_caveats(formatter: &DnfStyleFormatter, caveats: Option<&str>) {
    let Some(text) = caveats.map(str::trim).filter(|text| !text.is_empty()) else {
        return;
    };
    println!();
    println!("{}", formatter.section("Caveats"));
    println!("{}", clean_block(text));
}

fn print_analytics(
    config: &Config,
    metadata: &Metadata,
    period: &str,
    response: &AnalyticsResponse,
) -> Result<()> {
    let formatter = DnfStyleFormatter::new(config);
    println!(
        "{}",
        formatter.header(&format!(
            "Analytics for {} ({})",
            metadata.name,
            metadata.kind.label()
        ))
    );
    println!(
        "{}",
        formatter.field(
            "Category",
            &category_display(&response.category, metadata.kind)
        )
    );
    println!(
        "{}",
        formatter.field("Period", &period_description(period, response))
    );
    if response.total_items > 0 {
        println!(
            "{}",
            formatter.field(
                "Tracked",
                &format!("{} packages", format_count(response.total_items))
            )
        );
    }

    let Some(entries) = response.formulae.get(&metadata.name) else {
        println!();
        println!("{}", no_analytics_message(metadata, period, response));
        return Ok(());
    };
    let breakdown = analytics_breakdown(&metadata.name, entries)?;
    if breakdown.is_empty() {
        println!();
        println!("{}", no_analytics_message(metadata, period, response));
        return Ok(());
    }

    let canonical = breakdown
        .iter()
        .find(|(label, _)| label == &metadata.name)
        .map(|(_, count)| *count)
        .unwrap_or_else(|| breakdown.iter().map(|(_, count)| *count).sum());
    let installs = if response.total_count == 0 {
        format!(
            "{} installs (tap total unavailable)",
            format_count(canonical)
        )
    } else {
        match format_percent(canonical, response.total_count) {
            Some(percent) => format!(
                "{} of {} installs ({percent})",
                format_count(canonical),
                format_count(response.total_count)
            ),
            None => format!("{} installs", format_count(canonical)),
        }
    };
    println!("{}", formatter.field("Installs", &installs));

    if breakdown.len() > 1 {
        println!();
        println!("{}", formatter.section("Breakdown"));
        let rows: Vec<Vec<String>> = breakdown
            .iter()
            .map(|(label, count)| vec![label.clone(), format_count(*count)])
            .collect();
        println!("{}", formatter.table(&["Variant", "Installs"], &rows));
    }
    Ok(())
}

fn no_analytics_message(metadata: &Metadata, period: &str, response: &AnalyticsResponse) -> String {
    let category = category_display(&response.category, metadata.kind);
    let range = period_range(response);
    format!(
        "No {category} analytics recorded for '{}' in {period}{range}.",
        metadata.name
    )
}

/// The effective analytics category, falling back to the namespace's own.
fn category_display(category: &str, kind: Kind) -> String {
    let category = category.trim();
    if category.is_empty() {
        kind.analytics_category().to_string()
    } else {
        category.to_string()
    }
}

fn period_description(period: &str, response: &AnalyticsResponse) -> String {
    let range = period_range(response);
    if range.is_empty() {
        period.to_string()
    } else {
        format!("{period}{range}")
    }
}

fn period_range(response: &AnalyticsResponse) -> String {
    let start = response.start_date.trim();
    let end = response.end_date.trim();
    if start.is_empty() || end.is_empty() {
        String::new()
    } else {
        format!(" ({start} to {end})")
    }
}

fn analytics_breakdown(name: &str, entries: &[AnalyticsEntry]) -> Result<Vec<(String, u64)>> {
    let mut rows = Vec::with_capacity(entries.len());
    for entry in entries {
        let label = entry
            .formula
            .as_deref()
            .or(entry.cask.as_deref())
            .with_context(|| {
                format!("Homebrew analytics entry for '{name}' has no formula or cask label")
            })?;
        let count = parse_count(&entry.count).with_context(|| {
            format!("Homebrew analytics entry '{label}' has an invalid install count")
        })?;
        rows.push((label.to_string(), count));
    }
    Ok(rows)
}

fn formula_versions(formula: &FormulaResponse) -> String {
    let revision = formula.revision.unwrap_or(0);
    let mut parts = Vec::new();
    match formula
        .versions
        .stable
        .as_deref()
        .map(str::trim)
        .filter(|version| !version.is_empty())
    {
        Some(stable) => parts.push(format!("stable {}", with_revision(stable, revision))),
        None => parts.push("stable none".to_string()),
    }
    if let Some(head) = formula
        .versions
        .head
        .as_deref()
        .map(str::trim)
        .filter(|version| !version.is_empty())
    {
        parts.push(format!("head {head}"));
    }
    parts.join(", ")
}

fn cask_version(cask: &CaskResponse) -> String {
    let version = cask.version.trim();
    let version = if version.is_empty() {
        "unknown".to_string()
    } else {
        version.to_string()
    };
    if cask.auto_updates == Some(true) {
        format!("{version} (auto-updating)")
    } else {
        version
    }
}

fn with_revision(version: &str, revision: u32) -> String {
    if revision == 0 {
        version.to_string()
    } else {
        format!("{version}_{revision}")
    }
}

fn keg_only_value(formula: &FormulaResponse) -> String {
    match formula.keg_only_reason.as_ref() {
        Some(reason) if !reason.reason.trim().is_empty() => {
            let reason_text = reason.reason.trim();
            if reason.explanation.trim().is_empty() {
                format!("yes ({reason_text})")
            } else {
                format!("yes ({reason_text}: {})", reason.explanation.trim())
            }
        }
        _ => "yes".to_string(),
    }
}

fn formula_status_messages(formula: &FormulaResponse, name: &str) -> Vec<String> {
    let mut messages = Vec::new();
    if formula.disabled {
        messages.push(status_message(
            name,
            "disabled",
            &formula.disable_date,
            &formula.disable_reason,
            replacement_text(
                &formula.disable_replacement_formula,
                &formula.disable_replacement_cask,
            ),
        ));
    } else if formula.deprecated {
        messages.push(status_message(
            name,
            "deprecated",
            &formula.deprecation_date,
            &formula.deprecation_reason,
            replacement_text(
                &formula.deprecation_replacement_formula,
                &formula.deprecation_replacement_cask,
            ),
        ));
    }
    messages
}

fn cask_status_messages(cask: &CaskResponse, token: &str) -> Vec<String> {
    let mut messages = Vec::new();
    if cask.disabled {
        messages.push(status_message(
            token,
            "disabled",
            &cask.disable_date,
            &cask.disable_reason,
            replacement_text(
                &cask.disable_replacement_formula,
                &cask.disable_replacement_cask,
            ),
        ));
    } else if cask.deprecated {
        messages.push(status_message(
            token,
            "deprecated",
            &cask.deprecation_date,
            &cask.deprecation_reason,
            replacement_text(
                &cask.deprecation_replacement_formula,
                &cask.deprecation_replacement_cask,
            ),
        ));
    }
    messages
}

fn status_message(
    name: &str,
    state: &str,
    date: &Option<String>,
    reason: &Option<String>,
    replacement: Option<String>,
) -> String {
    let mut message = format!("{name} is {state}");
    if let Some(date) = non_empty(date.as_deref()) {
        message.push_str(&format!(" since {date}"));
    }
    if let Some(reason) = non_empty(reason.as_deref()) {
        message.push_str(&format!(": {reason}"));
    }
    if let Some(replacement) = replacement {
        message.push_str(&format!(" (replacement: {replacement})"));
    }
    message
}

fn replacement_text(formula: &Option<String>, cask: &Option<String>) -> Option<String> {
    match (non_empty(formula.as_deref()), non_empty(cask.as_deref())) {
        (Some(formula), Some(cask)) => Some(format!("{formula} (formula) or {cask} (cask)")),
        (Some(formula), None) => Some(formula.to_string()),
        (None, Some(cask)) => Some(cask.to_string()),
        (None, None) => None,
    }
}

fn non_empty(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

fn optional_text<'a>(value: Option<&'a str>, fallback: &'a str) -> &'a str {
    non_empty(value).unwrap_or(fallback)
}

fn tap_or(tap: &str, fallback: &str) -> String {
    non_empty(Some(tap))
        .map(str::to_string)
        .unwrap_or_else(|| fallback.to_string())
}

fn cask_display_name(cask: &CaskResponse) -> Option<String> {
    let names: Vec<&str> = cask
        .name
        .iter()
        .map(String::as_str)
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .collect();
    if names.is_empty() || (names.len() == 1 && names[0] == cask.token) {
        return None;
    }
    Some(names.join(", "))
}

fn formula_dependency_rows(formula: &FormulaResponse) -> Vec<Vec<String>> {
    let mut rows = Vec::new();
    push_dependency_rows(&mut rows, "Runtime", &formula.dependencies);
    push_dependency_rows(&mut rows, "Recommended", &formula.recommended_dependencies);
    push_dependency_rows(&mut rows, "Optional", &formula.optional_dependencies);
    push_dependency_rows(&mut rows, "Build", &formula.build_dependencies);
    for value in &formula.uses_from_macos {
        match value {
            serde_json::Value::String(name) => {
                rows.push(vec!["macOS".to_string(), name.clone()]);
            }
            serde_json::Value::Object(scopes) => {
                for (name, scope) in scopes {
                    let scope = render_value(scope);
                    let text = if scope.is_empty() || scope == "runtime" {
                        name.clone()
                    } else {
                        format!("{name} ({scope})")
                    };
                    rows.push(vec!["macOS".to_string(), text]);
                }
            }
            other => rows.push(vec!["macOS".to_string(), render_value(other)]),
        }
    }
    rows
}

fn push_dependency_rows(rows: &mut Vec<Vec<String>>, kind: &str, names: &[String]) {
    for name in names {
        rows.push(vec![kind.to_string(), name.clone()]);
    }
}

fn cask_dependency_rows(depends_on: &serde_json::Value) -> Vec<Vec<String>> {
    let Some(object) = depends_on.as_object() else {
        return Vec::new();
    };
    let mut rows = Vec::new();

    if let Some(macos) = object.get("macos") {
        match macos {
            // An empty macOS map means "no constraint", not a dependency.
            serde_json::Value::Object(bounds) => {
                for (operator, versions) in bounds {
                    rows.push(vec![
                        "macOS".to_string(),
                        format!("{operator} {}", render_value(versions)),
                    ]);
                }
            }
            other => rows.push(vec!["macOS".to_string(), render_value(other)]),
        }
    }
    if let Some(arch) = object.get("arch") {
        rows.push(vec!["Architecture".to_string(), render_value(arch)]);
    }
    for (key, label) in [("formula", "Formula"), ("cask", "Cask")] {
        if let Some(value) = object.get(key) {
            for name in list_values(value) {
                rows.push(vec![label.to_string(), name]);
            }
        }
    }
    for (key, value) in object {
        if matches!(key.as_str(), "macos" | "arch" | "formula" | "cask") {
            continue;
        }
        rows.push(vec![humanize(key), render_value(value)]);
    }
    rows
}

fn cask_conflicts_value(conflicts: &serde_json::Value) -> Option<String> {
    let object = conflicts.as_object()?;
    let mut parts = Vec::new();
    for (kind, names) in object {
        let names = list_values(names);
        if !names.is_empty() {
            parts.push(format!("{} ({kind})", names.join(", ")));
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("; "))
    }
}

fn list_values(value: &serde_json::Value) -> Vec<String> {
    match value {
        serde_json::Value::Array(items) => items.iter().map(render_value).collect(),
        serde_json::Value::String(_) => vec![render_value(value)],
        serde_json::Value::Null => Vec::new(),
        other => vec![render_value(other)],
    }
}

fn render_value(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(text) => text.clone(),
        serde_json::Value::Array(items) => items
            .iter()
            .map(render_value)
            .collect::<Vec<_>>()
            .join(", "),
        serde_json::Value::Bool(true) => "yes".to_string(),
        serde_json::Value::Bool(false) => "no".to_string(),
        serde_json::Value::Null => "unknown".to_string(),
        other => other.to_string(),
    }
}

fn humanize(key: &str) -> String {
    let mut output = String::with_capacity(key.len());
    for (index, part) in key.split('_').filter(|part| !part.is_empty()).enumerate() {
        if index > 0 {
            output.push(' ');
        }
        let mut characters = part.chars();
        if let Some(first) = characters.next() {
            output.extend(first.to_uppercase());
            output.push_str(characters.as_str());
        }
    }
    if output.is_empty() {
        key.to_string()
    } else {
        output
    }
}

/// A package the native store recorded in the selected prefix.
#[derive(Debug)]
enum Installed {
    Present { version: String, requested: bool },
    Absent,
    Unknown(String),
}

fn read_installed(prefix: Option<&Path>, name: &str) -> Installed {
    let path = match prefix_path(prefix) {
        Ok(path) => path,
        Err(error) => return Installed::Unknown(format!("{error:#}")),
    };
    let store = match Store::open(&path, false) {
        Ok(store) => store,
        Err(error) => return Installed::Unknown(format!("{error:#}")),
    };
    let receipts = match store.installed() {
        Ok(receipts) => receipts,
        Err(error) => return Installed::Unknown(format!("{error:#}")),
    };
    match receipts.into_iter().find(|receipt| receipt.name == name) {
        Some(receipt) => Installed::Present {
            version: receipt.version,
            requested: receipt.requested,
        },
        None => Installed::Absent,
    }
}

/// Describe the installed overlay, including whether the API stable version is
/// newer. Version comparison is conservative; unsupported syntax leaves only
/// the installed version and reason, never a guessed upgrade state.
fn describe_installed(installed: &Installed, available: Option<&str>, revision: u32) -> String {
    match installed {
        Installed::Present { version, requested } => {
            let mut details = vec![if *requested {
                "requested"
            } else {
                "dependency"
            }];
            if let Some(stable) = available.map(str::trim).filter(|value| !value.is_empty()) {
                match super::version::compare(&with_revision(stable, revision), version) {
                    Ok(Ordering::Greater) => details.push("update available"),
                    Ok(Ordering::Equal) => details.push("up to date"),
                    Ok(Ordering::Less) => details.push("newer than the API stable"),
                    Err(_) => {}
                }
            }
            format!("{version} ({})", details.join(", "))
        }
        Installed::Absent => "not installed by brewrs".to_string(),
        Installed::Unknown(_) => "unknown".to_string(),
    }
}

#[derive(Debug)]
enum InstalledCask {
    Present { version: String },
    Absent,
    Unknown(String),
}

fn read_installed_cask(prefix: Option<&Path>, token: &str) -> InstalledCask {
    let path = match prefix_path(prefix) {
        Ok(path) => path,
        Err(error) => return InstalledCask::Unknown(format!("{error:#}")),
    };
    let store = match Store::open(&path, false) {
        Ok(store) => store,
        Err(error) => return InstalledCask::Unknown(format!("{error:#}")),
    };
    let receipts = match super::cask_store::installed(&store) {
        Ok(receipts) => receipts,
        Err(error) => return InstalledCask::Unknown(format!("{error:#}")),
    };
    match receipts.into_iter().find(|receipt| receipt.token == token) {
        Some(receipt) => InstalledCask::Present {
            version: receipt.version,
        },
        None => InstalledCask::Absent,
    }
}

fn describe_installed_cask(installed: &InstalledCask, available: &str) -> String {
    match installed {
        InstalledCask::Present { version } => {
            let available = available.trim();
            if available.is_empty() {
                return version.clone();
            }
            let status = match compare_cask_versions(available, version) {
                CaskVersionOrder::Same => "up to date",
                CaskVersionOrder::Upgrade => "update available",
                CaskVersionOrder::Downgrade => "newer than the API version",
            };
            format!("{version} ({status})")
        }
        InstalledCask::Absent => "not installed by brewrs".to_string(),
        InstalledCask::Unknown(_) => "unknown".to_string(),
    }
}

/// Inline analytics published with a formula or cask payload.
///
/// `install` maps a period (`30d`, `90d`, `365d`) to the counts of the exact
/// identifier and of option/HEAD variants (for example `jq --HEAD`).
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
struct InlineAnalytics {
    install: BTreeMap<String, BTreeMap<String, u64>>,
}

fn inline_analytics_rows(name: &str, analytics: &InlineAnalytics) -> Vec<Vec<String>> {
    let mut rows = Vec::new();
    for period in sorted_periods(&analytics.install) {
        let Some(counts) = analytics.install.get(period) else {
            continue;
        };
        let Some((canonical, variants)) = inline_counts(name, counts) else {
            continue;
        };
        rows.push(vec![
            period.to_string(),
            format_count(canonical),
            if variants == 0 {
                String::new()
            } else {
                format_count(variants)
            },
        ]);
    }
    rows
}

fn inline_counts(name: &str, counts: &BTreeMap<String, u64>) -> Option<(u64, u64)> {
    let mut canonical = None;
    let mut variants = 0u64;
    let mut matched = false;
    for (label, count) in counts {
        if label == name {
            canonical = Some(*count);
            matched = true;
        } else if label
            .strip_prefix(name)
            .is_some_and(|rest| rest.starts_with(' '))
        {
            variants = variants.saturating_add(*count);
            matched = true;
        }
    }
    if matched {
        Some((canonical.unwrap_or(0), variants))
    } else {
        None
    }
}

fn sorted_periods(install: &BTreeMap<String, BTreeMap<String, u64>>) -> Vec<&str> {
    let mut periods: Vec<&str> = install.keys().map(String::as_str).collect();
    periods.sort_by(|left, right| {
        period_rank(left)
            .cmp(&period_rank(right))
            .then_with(|| left.cmp(right))
    });
    periods
}

fn period_rank(period: &str) -> usize {
    ANALYTICS_PERIODS
        .iter()
        .position(|known| *known == period)
        .unwrap_or(ANALYTICS_PERIODS.len())
}

fn format_count(value: u64) -> String {
    let digits = value.to_string();
    let mut output = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, character) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index) % 3 == 0 {
            output.push(',');
        }
        output.push(character);
    }
    output
}

fn parse_count(raw: &str) -> Result<u64> {
    let raw = raw.trim();
    if raw.is_empty() {
        bail!("count is empty");
    }
    let parts: Vec<&str> = raw.split(',').collect();
    let Some((first, rest)) = parts.split_first() else {
        bail!("invalid count '{raw}'");
    };
    if first.is_empty() || first.len() > 3 || !first.bytes().all(|byte| byte.is_ascii_digit()) {
        bail!("invalid count '{raw}'");
    }
    for part in rest {
        if part.len() != 3 || !part.bytes().all(|byte| byte.is_ascii_digit()) {
            bail!("invalid count '{raw}'");
        }
    }
    let digits: String = parts.concat();
    digits
        .parse::<u64>()
        .with_context(|| format!("count '{raw}' is above the supported maximum"))
}

fn format_percent(part: u64, total: u64) -> Option<String> {
    if total == 0 {
        return None;
    }
    let percent = part as f64 * 100.0 / total as f64;
    Some(if percent < 0.01 {
        format!("{percent:.4}%")
    } else {
        format!("{percent:.2}%")
    })
}

fn one_line(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect()
}

fn clean_block(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_control() && character != '\n' {
                ' '
            } else {
                character
            }
        })
        .collect()
}

/// A formula as published by `formula/<name>.json`, reduced to displayed
/// fields.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
struct FormulaResponse {
    name: String,
    full_name: String,
    tap: String,
    desc: Option<String>,
    license: Option<String>,
    homepage: Option<String>,
    versions: FormulaVersions,
    revision: Option<u32>,
    keg_only: bool,
    keg_only_reason: Option<KegOnlyReason>,
    caveats: Option<String>,
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
    dependencies: Vec<String>,
    build_dependencies: Vec<String>,
    recommended_dependencies: Vec<String>,
    optional_dependencies: Vec<String>,
    uses_from_macos: Vec<serde_json::Value>,
    conflicts_with: Vec<String>,
    aliases: Vec<String>,
    oldnames: Vec<String>,
    analytics: Option<InlineAnalytics>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
struct FormulaVersions {
    stable: Option<String>,
    head: Option<String>,
    bottle: bool,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
struct KegOnlyReason {
    reason: String,
    explanation: String,
}

/// A cask as published by `cask/<token>.json`, reduced to displayed fields.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
struct CaskResponse {
    token: String,
    full_token: String,
    old_tokens: Vec<String>,
    tap: String,
    name: Vec<String>,
    desc: Option<String>,
    homepage: Option<String>,
    version: String,
    auto_updates: Option<bool>,
    caveats: Option<String>,
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
    depends_on: serde_json::Value,
    conflicts_with: serde_json::Value,
    analytics: Option<InlineAnalytics>,
}

/// A tap-scoped analytics report as published by
/// `analytics/<category>/<tap>/<period>.json`.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
struct AnalyticsResponse {
    category: String,
    total_items: u64,
    start_date: String,
    end_date: String,
    total_count: u64,
    formulae: BTreeMap<String, Vec<AnalyticsEntry>>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
struct AnalyticsEntry {
    formula: Option<String>,
    cask: Option<String>,
    count: String,
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;

    const FORMULA_JSON: &str = r#"{
        "name": "jq",
        "full_name": "jq",
        "tap": "homebrew/core",
        "desc": "Lightweight and flexible command-line JSON processor",
        "license": "MIT",
        "homepage": "https://jqlang.github.io/jq/",
        "versions": { "stable": "1.8.2", "head": "HEAD", "bottle": true },
        "revision": 0,
        "keg_only": false,
        "dependencies": ["oniguruma"],
        "build_dependencies": ["pkgconf"],
        "recommended_dependencies": [],
        "optional_dependencies": ["extra"],
        "uses_from_macos": ["krb5", { "unzip": "build" }],
        "conflicts_with": ["other"],
        "aliases": ["jq5"],
        "oldnames": ["jq4"],
        "deprecated": false,
        "disabled": false,
        "analytics": {
            "install": {
                "30d": { "jq": 62097, "jq --HEAD": 39 },
                "90d": { "jq": 171818, "jq --HEAD": 312 },
                "365d": { "jq": 822414, "jq --HEAD": 3530 }
            }
        }
    }"#;

    const CASK_JSON: &str = r#"{
        "token": "firefox",
        "full_token": "firefox",
        "old_tokens": [],
        "tap": "homebrew/cask",
        "name": ["Mozilla Firefox"],
        "desc": "Web browser",
        "homepage": "https://www.mozilla.org/firefox/",
        "version": "141.0",
        "auto_updates": true,
        "deprecated": false,
        "disabled": false,
        "depends_on": { "macos": { ">=": ["13"] } },
        "conflicts_with": { "cask": ["firefox@beta"] },
        "analytics": { "install": { "30d": { "firefox": 40282 } } }
    }"#;

    const ANALYTICS_JSON: &str = r#"{
        "category": "formula_install",
        "total_items": 8113,
        "start_date": "2026-10-20",
        "end_date": "2026-11-19",
        "total_count": 24541690,
        "formulae": {
            "jq": [
                { "formula": "jq", "count": "62,097" },
                { "formula": "jq --HEAD", "count": "39" }
            ],
            "other": [{ "formula": "other", "count": "1" }]
        }
    }"#;

    #[derive(Clone, Copy)]
    enum Failure {
        NotFound,
        Unavailable,
    }

    struct FixtureSource {
        formula: Option<&'static str>,
        cask: Option<&'static str>,
        analytics: &'static str,
        formula_failure: Option<Failure>,
        cask_failure: Option<Failure>,
        formula_calls: Cell<usize>,
        cask_calls: Cell<usize>,
        analytics_calls: Cell<usize>,
    }

    impl FixtureSource {
        fn new() -> Self {
            Self {
                formula: None,
                cask: None,
                analytics: "{}",
                formula_failure: None,
                cask_failure: None,
                formula_calls: Cell::new(0),
                cask_calls: Cell::new(0),
                analytics_calls: Cell::new(0),
            }
        }

        fn formula(mut self, document: &'static str) -> Self {
            self.formula = Some(document);
            self
        }

        fn cask(mut self, document: &'static str) -> Self {
            self.cask = Some(document);
            self
        }

        fn analytics_document(mut self, document: &'static str) -> Self {
            self.analytics = document;
            self
        }

        fn formula_failure(mut self, failure: Failure) -> Self {
            self.formula_failure = Some(failure);
            self
        }

        fn cask_failure(mut self, failure: Failure) -> Self {
            self.cask_failure = Some(failure);
            self
        }
    }

    impl MetadataSource for FixtureSource {
        async fn formula(&self, _name: &str) -> std::result::Result<FormulaResponse, LookupError> {
            self.formula_calls.set(self.formula_calls.get() + 1);
            match self.formula_failure {
                Some(Failure::NotFound) => Err(LookupError::NotFound),
                Some(Failure::Unavailable) => Err(LookupError::Failed(anyhow::anyhow!(
                    "API request failed: HTTP 503 Service Unavailable"
                ))),
                None => {
                    let document = self.formula.expect("formula fixture must be set");
                    serde_json::from_str(document)
                        .map_err(|error| LookupError::Failed(error.into()))
                }
            }
        }

        async fn cask(&self, _token: &str) -> std::result::Result<CaskResponse, LookupError> {
            self.cask_calls.set(self.cask_calls.get() + 1);
            match self.cask_failure {
                Some(Failure::NotFound) => Err(LookupError::NotFound),
                Some(Failure::Unavailable) => Err(LookupError::Failed(anyhow::anyhow!(
                    "API request failed: HTTP 503 Service Unavailable"
                ))),
                None => {
                    let document = self.cask.expect("cask fixture must be set");
                    serde_json::from_str(document)
                        .map_err(|error| LookupError::Failed(error.into()))
                }
            }
        }

        async fn analytics(&self, _kind: Kind, _period: &str) -> Result<AnalyticsResponse> {
            self.analytics_calls.set(self.analytics_calls.get() + 1);
            serde_json::from_str(self.analytics).context("invalid analytics fixture")
        }
    }

    #[test]
    fn parses_official_identifiers_and_taps() {
        let plain = parse_package("jq").expect("plain name");
        assert_eq!(plain.name, "jq");
        assert_eq!(plain.kind, None);

        let versioned = parse_package("python@3.13").expect("versioned name");
        assert_eq!(versioned.name, "python@3.13");

        let core = parse_package("homebrew/core/jq").expect("core tap");
        assert_eq!(core.name, "jq");
        assert_eq!(core.kind, Some(Kind::Formula));

        let cask = parse_package("homebrew/cask/firefox").expect("cask tap");
        assert_eq!(cask.name, "firefox");
        assert_eq!(cask.kind, Some(Kind::Cask));
    }

    #[test]
    fn rejects_unsafe_identifiers_and_unsupported_taps() {
        for bad in [
            "",
            "   ",
            "../jq",
            "jq/",
            "homebrew/core/",
            "homebrew/core/a/b",
            "user/repo/pkg",
            "homebrew/other/jq",
            "JQ",
            "-jq",
            ".jq",
            "jq?x",
            "jq x",
            "caf\u{e9}",
        ] {
            assert!(
                parse_package(bad).is_err(),
                "'{bad}' must be rejected before any request"
            );
        }
    }

    #[test]
    fn kind_flags_are_explicit_and_unambiguous() {
        let cask = parse_package("homebrew/cask/firefox").expect("cask");
        assert!(requested_kind("homebrew/cask/firefox", &cask, true, false).is_err());

        let plain = parse_package("firefox").expect("plain");
        assert!(requested_kind("firefox", &plain, true, true).is_err());
        assert_eq!(
            requested_kind("firefox", &plain, false, true).expect("cask flag"),
            Some(Kind::Cask)
        );

        let cask = parse_package("homebrew/cask/firefox").expect("cask");
        assert_eq!(
            requested_kind("homebrew/cask/firefox", &cask, false, false).expect("tap kind"),
            Some(Kind::Cask)
        );
    }

    #[test]
    fn only_a_typed_http_404_counts_as_not_found() {
        // Error text alone never qualifies; the status must be typed. The
        // positive path is exercised end-to-end in the `HttpClient` tests.
        assert!(!is_not_found(&anyhow::anyhow!(
            "API request failed: HTTP 404 Not Found"
        )));
        assert!(!is_not_found(&anyhow::anyhow!(
            "API request failed: HTTP 503 Service Unavailable"
        )));
        assert!(!is_not_found(&anyhow::anyhow!(
            "Failed to send request: connection refused"
        )));
    }

    #[tokio::test]
    async fn bare_name_prefers_formula_when_both_namespaces_exist() {
        let source = FixtureSource::new().formula(FORMULA_JSON).cask(CASK_JSON);
        let metadata = lookup(&source, "jq", None).await.expect("formula");
        assert_eq!(metadata.kind, Kind::Formula);
        assert_eq!(metadata.name, "jq");
        assert_eq!(source.cask_calls.get(), 0);
    }

    #[tokio::test]
    async fn formula_404_falls_back_to_the_cask() {
        let source = FixtureSource::new()
            .formula_failure(Failure::NotFound)
            .cask(CASK_JSON);
        let metadata = lookup(&source, "firefox", None).await.expect("cask");
        assert_eq!(metadata.kind, Kind::Cask);
        assert_eq!(metadata.name, "firefox");
    }

    #[tokio::test]
    async fn a_non_404_formula_failure_is_not_hidden_by_the_fallback() {
        let source = FixtureSource::new()
            .formula_failure(Failure::Unavailable)
            .cask(CASK_JSON);
        let error = lookup(&source, "jq", None)
            .await
            .expect_err("503 must fail");
        assert!(format!("{error:#}").contains("503"));
        assert_eq!(source.cask_calls.get(), 0);
    }

    #[tokio::test]
    async fn two_404s_report_a_missing_package() {
        let source = FixtureSource::new()
            .formula_failure(Failure::NotFound)
            .cask_failure(Failure::NotFound);
        let error = lookup(&source, "missing", None)
            .await
            .expect_err("missing package");
        assert!(error.to_string().contains("No formula or cask named"));
    }

    #[tokio::test]
    async fn an_explicit_cask_never_probes_the_formula_endpoint() {
        let source = FixtureSource::new().cask_failure(Failure::NotFound);
        let error = lookup(&source, "firefox", Some(Kind::Cask))
            .await
            .expect_err("missing cask");
        assert!(error.to_string().contains("No cask named"));
        assert_eq!(source.formula_calls.get(), 0);
    }

    #[test]
    fn analytics_urls_are_tap_scoped() {
        assert_eq!(
            analytics_url(Kind::Formula, "30d"),
            "https://formulae.brew.sh/api/analytics/install/homebrew-core/30d.json"
        );
        assert_eq!(
            analytics_url(Kind::Cask, "365d"),
            "https://formulae.brew.sh/api/analytics/cask-install/homebrew-cask/365d.json"
        );
    }

    #[test]
    fn period_validation_lists_supported_values() {
        assert_eq!(validate_period("30d").expect("30d"), "30d");
        assert_eq!(validate_period(" 90d ").expect("trimmed"), "90d");
        let error = validate_period("7d").expect_err("7d is unsupported");
        assert!(error.to_string().contains("30d, 90d, 365d"));
    }

    #[test]
    fn analytics_breakdown_reads_variants_and_dates() {
        let response: AnalyticsResponse =
            serde_json::from_str(ANALYTICS_JSON).expect("analytics fixture");
        let entries = response.formulae.get("jq").expect("jq entries");
        let rows = analytics_breakdown("jq", entries).expect("breakdown");
        assert_eq!(
            rows,
            vec![("jq".to_string(), 62_097), ("jq --HEAD".to_string(), 39)]
        );
        assert_eq!(
            category_display(&response.category, Kind::Formula),
            "formula_install"
        );
        assert_eq!(
            period_description("30d", &response),
            "30d (2026-10-20 to 2026-11-19)"
        );
        assert_eq!(format_percent(62_097, 24_541_690).as_deref(), Some("0.25%"));
    }

    #[test]
    fn analytics_breakdown_rejects_malformed_counts() {
        let entry = AnalyticsEntry {
            formula: Some("jq".to_string()),
            cask: None,
            count: "62,,097".to_string(),
        };
        assert!(analytics_breakdown("jq", &[entry]).is_err());
    }

    #[test]
    fn missing_package_is_reported_as_no_data() {
        let response: AnalyticsResponse =
            serde_json::from_str(ANALYTICS_JSON).expect("analytics fixture");
        assert!(!response.formulae.contains_key("absent"));
        let metadata = Metadata {
            kind: Kind::Formula,
            name: "absent".to_string(),
            formula: None,
            cask: None,
        };
        let message = no_analytics_message(&metadata, "30d", &response);
        assert!(message.contains("No formula_install analytics recorded for 'absent'"));
        assert!(message.contains("2026-10-20 to 2026-11-19"));
    }

    #[test]
    fn formats_and_parses_install_counts() {
        assert_eq!(format_count(0), "0");
        assert_eq!(format_count(62_097), "62,097");
        assert_eq!(format_count(24_541_690), "24,541,690");
        assert_eq!(parse_count("62,097").expect("grouped"), 62_097);
        assert_eq!(parse_count("0").expect("zero"), 0);
        for bad in ["", " ", "62,,097", "62097,", "0062,097", "abc", "1,2"] {
            assert!(parse_count(bad).is_err(), "'{bad}' must be rejected");
        }
    }

    #[test]
    fn inline_analytics_prefers_exact_counts_and_orders_periods() {
        let analytics: InlineAnalytics = serde_json::from_str(
            r#"{"install":{"365d":{"jq":3,"jq --HEAD":1},"30d":{"jq":62,"jq --HEAD":2},"90d":{"jq":171}}}"#,
        )
        .expect("inline analytics");
        let rows = inline_analytics_rows("jq", &analytics);
        let periods: Vec<&str> = rows.iter().map(|row| row[0].as_str()).collect();
        assert_eq!(periods, ["30d", "90d", "365d"]);
        assert_eq!(rows[0], vec!["30d", "62", "2"]);
        assert_eq!(rows[2], vec!["365d", "3", "1"]);
    }

    #[test]
    fn inline_analytics_ignores_unrelated_names() {
        let analytics: InlineAnalytics =
            serde_json::from_str(r#"{"install":{"30d":{"other":5}}}"#).expect("inline");
        assert!(inline_analytics_rows("jq", &analytics).is_empty());
    }

    #[test]
    fn formula_versions_include_revision_and_head() {
        let mut formula: FormulaResponse =
            serde_json::from_str(FORMULA_JSON).expect("formula fixture");
        assert_eq!(formula_versions(&formula), "stable 1.8.2, head HEAD");
        formula.revision = Some(2);
        assert_eq!(formula_versions(&formula), "stable 1.8.2_2, head HEAD");
        formula.versions.head = None;
        formula.versions.stable = None;
        assert_eq!(formula_versions(&formula), "stable none");
    }

    #[test]
    fn formula_dependencies_cover_optional_and_macos_scopes() {
        let formula: FormulaResponse = serde_json::from_str(FORMULA_JSON).expect("formula fixture");
        let rows = formula_dependency_rows(&formula);
        assert!(rows.contains(&vec!["Runtime".to_string(), "oniguruma".to_string()]));
        assert!(rows.contains(&vec!["Optional".to_string(), "extra".to_string()]));
        assert!(rows.contains(&vec!["Build".to_string(), "pkgconf".to_string()]));
        assert!(rows.contains(&vec!["macOS".to_string(), "krb5".to_string()]));
        assert!(rows.contains(&vec!["macOS".to_string(), "unzip (build)".to_string()]));
    }

    #[test]
    fn cask_dependencies_render_versions_and_names() {
        let depends_on = serde_json::json!({
            "macos": { ">=": ["13"] },
            "arch": ["arm64"],
            "formula": ["lib"],
            "cask": ["helper"],
            "x11": true
        });
        assert_eq!(
            cask_dependency_rows(&depends_on),
            vec![
                vec!["macOS".to_string(), ">= 13".to_string()],
                vec!["Architecture".to_string(), "arm64".to_string()],
                vec!["Formula".to_string(), "lib".to_string()],
                vec!["Cask".to_string(), "helper".to_string()],
                vec!["X11".to_string(), "yes".to_string()],
            ]
        );
    }

    #[test]
    fn cask_conflicts_are_grouped_by_namespace() {
        let conflicts = serde_json::json!({ "cask": ["firefox@beta", "firefox@esr"] });
        assert_eq!(
            cask_conflicts_value(&conflicts).as_deref(),
            Some("firefox@beta, firefox@esr (cask)")
        );
    }

    #[test]
    fn installed_overlay_reports_version_and_update_state() {
        let installed = Installed::Present {
            version: "1.7.0".to_string(),
            requested: true,
        };
        let described = describe_installed(&installed, Some("1.8.2"), 0);
        assert!(described.contains("1.7.0"));
        assert!(described.contains("requested"));
        assert!(described.contains("update available"));

        let current = Installed::Present {
            version: "1.8.2".to_string(),
            requested: false,
        };
        let described = describe_installed(&current, Some("1.8.2"), 0);
        assert!(described.contains("dependency"));
        assert!(described.contains("up to date"));

        let unsupported = Installed::Present {
            version: "1.8.2-rc1".to_string(),
            requested: true,
        };
        let described = describe_installed(&unsupported, Some("1.8.2"), 0);
        assert!(!described.contains("available"));
    }

    #[test]
    fn an_unowned_prefix_has_no_installed_overlay() {
        let prefix = tempfile::tempdir().expect("tempdir");
        match read_installed(Some(prefix.path()), "jq") {
            Installed::Absent => {}
            other => panic!("unowned prefix must be absent, got {other:?}"),
        }
        match read_installed_cask(Some(prefix.path()), "firefox") {
            InstalledCask::Absent => {}
            other => panic!("unowned prefix must be absent for casks, got {other:?}"),
        }
    }

    #[test]
    fn cask_installed_overlay_reports_version_and_update_state() {
        assert_eq!(
            describe_installed_cask(
                &InstalledCask::Present {
                    version: "141.0".into(),
                },
                "142.0",
            ),
            "141.0 (update available)"
        );
        assert_eq!(
            describe_installed_cask(
                &InstalledCask::Present {
                    version: "142.0".into(),
                },
                "142.0",
            ),
            "142.0 (up to date)"
        );
        assert_eq!(
            describe_installed_cask(&InstalledCask::Absent, "142.0"),
            "not installed by brewrs"
        );
    }

    #[test]
    fn status_messages_name_the_replacement() {
        let formula: FormulaResponse = serde_json::from_str(
            r#"{
                "name": "jq",
                "deprecated": true,
                "deprecation_date": "2024-01-01",
                "deprecation_reason": "unmaintained",
                "deprecation_replacement_formula": "jaq"
            }"#,
        )
        .expect("status fixture");
        let messages = formula_status_messages(&formula, "jq");
        assert_eq!(messages.len(), 1);
        assert!(messages[0].contains("deprecated since 2024-01-01"));
        assert!(messages[0].contains("replacement: jaq"));
    }

    #[tokio::test]
    async fn analytics_source_is_used_for_the_requested_period() {
        let source = FixtureSource::new().analytics_document(ANALYTICS_JSON);
        let response = source
            .analytics(Kind::Formula, "30d")
            .await
            .expect("analytics");
        assert_eq!(source.analytics_calls.get(), 1);
        assert_eq!(response.total_count, 24_541_690);
    }
}
