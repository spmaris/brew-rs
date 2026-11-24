//! Native install planning for `homebrew/cask`.
//!
//! [`resolve`] fetches official cask metadata from
//! `https://formulae.brew.sh/api/cask/<token>.json` (falling back on HTTP 404 to
//! the cached cask catalog for unique `old_tokens` renames, except during
//! read-only `migrate --check` audits) without running `brew` or evaluating Ruby.
//!
//! Only a clean, verified subset of macOS casks is eligible for native
//! installation:
//!
//! * exact 64-character lowercase SHA-256 checksums (`sha256: no_check` is blocked);
//! * public HTTPS download URLs with no `url_specs`, `rename`, or nested containers;
//! * `app`, `binary`, `command_wrapper` (`executable` + optional `args`), and static
//!   `manpage` / shell completion artifacts whose destinations stay inside the
//!   configured appdir or `$HOMEBREW_PREFIX`;
//! * recognized `depends_on` (`macos` `>=`/`==` and 64-bit `arch`) and
//!   non-destructive `uninstall` metadata (`quit`/`signal`/`login_item`/`launchctl`).
//!
//! All other casks remain plannable for diagnostics and `migrate --check`, with
//! explicit blockers explaining why native installation refuses them.

use std::cmp::Ordering;
use std::collections::HashSet;

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::{Map, Value};

use crate::api::catalog_cache;
use crate::api::http_client::{HttpClient, is_not_found};
use crate::config::Config;

use super::cask_download::validate_cask_url;
use super::download::is_lower_hex_digest;
use super::plan::{
    arch_for_tag, is_supported_macos_tag, macos_version_for_tag, parse_macos_version_or_codename,
};

/// Official Homebrew cask API base.
const API_BASE: &str = "https://formulae.brew.sh/api";
/// Official cask tap supported by the native backend.
const CASK_TAP: &str = "homebrew/cask";
/// Maximum cask targets accepted in one invocation.
const MAX_TARGETS: usize = 64;
/// Maximum length of a cask token.
const MAX_TOKEN_LEN: usize = 128;
/// Maximum length of a cask version.
const MAX_VERSION_LEN: usize = 128;
/// Maximum number of artifacts declared by a single cask.
const MAX_ARTIFACTS_PER_CASK: usize = 128;

/// Container extraction hint declared by a cask payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ContainerHint {
    Auto,
    Naked,
    Zip,
    Dmg,
}

/// Location of a cask source file inside either the unpacked archive or a
/// declared `.app` bundle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum CaskSourcePath {
    Payload(String),
    AppBundle {
        bundle_name: String,
        inner_path: String,
    },
}

/// One validated cask artifact supported by native cask installation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum PlannedCaskArtifact {
    App {
        source: String,
        bundle_name: String,
    },
    Binary {
        source: CaskSourcePath,
        link_name: String,
    },
    CommandWrapper {
        executable: CaskSourcePath,
        link_name: String,
        args: Vec<String>,
    },
    PrefixLink {
        source: CaskSourcePath,
        relative_dest: String,
    },
}

/// Planned cask installation metadata and any blockers preventing install.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct PlannedCask {
    pub token: String,
    pub name: Vec<String>,
    pub version: String,
    pub auto_updates: bool,
    pub url: String,
    pub sha256: String,
    pub container: ContainerHint,
    pub artifacts: Vec<PlannedCaskArtifact>,
    pub conflicts_with_casks: Vec<String>,
    pub conflicts_with_formulae: Vec<String>,
    pub install_blockers: Vec<String>,
}

/// Ordering result when comparing an available cask version with an installed one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CaskVersionOrder {
    Same,
    Upgrade,
    Downgrade,
}

/// Compare candidate and installed cask version strings conservatively.
pub(super) fn compare_cask_versions(candidate: &str, installed: &str) -> CaskVersionOrder {
    if candidate == installed {
        return CaskVersionOrder::Same;
    }
    let (cand_base, cand_build) = split_cask_version(candidate);
    let (inst_base, inst_build) = split_cask_version(installed);
    match super::version::compare(cand_base, inst_base) {
        Ok(Ordering::Greater) => CaskVersionOrder::Upgrade,
        Ok(Ordering::Less) => CaskVersionOrder::Downgrade,
        Ok(Ordering::Equal) => match (cand_build, inst_build) {
            (Some(cb), Some(ib)) => match super::version::compare(cb, ib) {
                Ok(Ordering::Greater) => CaskVersionOrder::Upgrade,
                Ok(Ordering::Less) => CaskVersionOrder::Downgrade,
                Ok(Ordering::Equal) => CaskVersionOrder::Same,
                Err(_) => {
                    if cb == ib {
                        CaskVersionOrder::Same
                    } else {
                        CaskVersionOrder::Upgrade
                    }
                }
            },
            (Some(_), None) => CaskVersionOrder::Upgrade,
            (None, Some(_)) => CaskVersionOrder::Downgrade,
            (None, None) => CaskVersionOrder::Same,
        },
        Err(_) => CaskVersionOrder::Upgrade,
    }
}

fn split_cask_version(version: &str) -> (&str, Option<&str>) {
    match version.split_once(',') {
        Some((base, build)) => (base, Some(build)),
        None => (version, None),
    }
}

/// Validate a cask CLI target (`<token>` or `homebrew/cask/<token>`).
pub(super) fn parse_cask_target(raw: &str) -> Result<&str> {
    if raw.len() > CASK_TAP.len() + 1 + MAX_TOKEN_LEN {
        bail!("cask target '{raw}' is longer than the supported maximum");
    }
    let token = match raw.rsplit_once('/') {
        Some((tap, token)) => {
            if tap != CASK_TAP {
                bail!(
                    "unsupported tap '{tap}' in target '{raw}': only {CASK_TAP} casks are supported"
                );
            }
            token
        }
        None => raw,
    };
    validate_cask_identifier(token, "target cask token")
}

/// Validate one lowercase Homebrew identifier (cask token or formula name).
pub(super) fn validate_cask_identifier<'a>(value: &'a str, what: &str) -> Result<&'a str> {
    if value.is_empty() || value.len() > MAX_TOKEN_LEN {
        bail!("{what} '{value}' must be between 1 and {MAX_TOKEN_LEN} characters");
    }
    let mut chars = value.chars();
    match chars.next() {
        Some(first) if first.is_ascii_lowercase() || first.is_ascii_digit() => {}
        _ => bail!("{what} '{value}' must start with a lowercase letter or digit"),
    }
    if value.contains("..") {
        bail!("{what} '{value}' must not contain '..'");
    }
    if !value.chars().all(|ch| {
        ch.is_ascii_lowercase() || ch.is_ascii_digit() || matches!(ch, '@' | '+' | '-' | '_' | '.')
    }) {
        bail!("{what} '{value}' contains characters outside [a-z0-9@+._-]");
    }
    Ok(value)
}

/// Resolve cask install plans for `targets` on host tag `tag`.
pub(super) async fn resolve(
    config: &Config,
    targets: &[String],
    tag: &str,
) -> Result<Vec<PlannedCask>> {
    let source = HttpCaskSource {
        http: HttpClient::new(config)?,
        alias_config: Some(config.clone()),
    };
    resolve_with(&source, targets, tag).await
}

/// Read-only cask planner reused across `migrate --check`.
pub(super) struct AuditCaskPlanner {
    source: HttpCaskSource,
}

impl AuditCaskPlanner {
    pub(super) fn new(config: &Config) -> Result<Self> {
        Ok(Self {
            source: HttpCaskSource {
                http: HttpClient::new(config)?,
                alias_config: None,
            },
        })
    }

    pub(super) async fn resolve_one(&self, token: &str, tag: &str) -> Result<PlannedCask> {
        let mut plan = resolve_with(&self.source, &[token.to_string()], tag).await?;
        plan.pop()
            .context("cask planner returned an empty plan for one target")
    }
}

trait CaskSource {
    async fn fetch_cask(&self, token: &str) -> Result<Value>;
}

struct HttpCaskSource {
    http: HttpClient,
    alias_config: Option<Config>,
}

impl CaskSource for HttpCaskSource {
    async fn fetch_cask(&self, token: &str) -> Result<Value> {
        let url = format!("{API_BASE}/cask/{token}.json");
        match self.http.fetch_json(&url).await {
            Ok(payload) => Ok(payload),
            Err(error) => {
                if is_not_found(&error)
                    && let Some(config) = &self.alias_config
                    && let Ok(Some(canonical)) =
                        catalog_cache::resolve_cask_old_token(config, token).await
                    && !canonical.contains('/')
                    && validate_cask_identifier(&canonical, "cask old_token").is_ok()
                {
                    let alias_url = format!("{API_BASE}/cask/{canonical}.json");
                    return self.http.fetch_json(&alias_url).await.with_context(|| {
                                    format!(
                                        "failed to fetch cask '{canonical}' (resolved from '{token}') from {alias_url}"
                                    )
                                });
                }
                Err(error).with_context(|| format!("failed to fetch cask '{token}' from {url}"))
            }
        }
    }
}

async fn resolve_with<S: CaskSource>(
    source: &S,
    targets: &[String],
    tag: &str,
) -> Result<Vec<PlannedCask>> {
    if targets.is_empty() {
        bail!("at least one cask target is required");
    }
    if targets.len() > MAX_TARGETS {
        bail!(
            "cannot resolve more than {MAX_TARGETS} cask targets at once (got {})",
            targets.len()
        );
    }

    let mut seen_tokens = HashSet::new();
    let mut planned = Vec::new();
    for raw_target in targets {
        let token = parse_cask_target(raw_target)?;
        let raw_payload = source.fetch_cask(token).await?;
        let cask = plan_from_value(raw_payload, token, tag)?;
        if seen_tokens.insert(cask.token.clone()) {
            planned.push(cask);
        }
    }
    Ok(planned)
}

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
struct RawCask {
    token: String,
    tap: String,
    name: Vec<String>,
    version: Value,
    url: String,
    url_specs: Value,
    sha256: Value,
    auto_updates: Option<bool>,
    disabled: bool,
    disable_reason: Option<String>,
    container: Value,
    depends_on: Value,
    conflicts_with: Value,
    artifacts: Vec<Value>,
    rename: Vec<Value>,
    supported_platforms: Vec<String>,
    variations: Value,
}

fn merge_variation(base: Value, tag: &str) -> Result<RawCask> {
    let mut base_obj = match base {
        Value::Object(map) => map,
        _ => bail!("cask API response is not a JSON object"),
    };
    let overlay = base_obj
        .get("variations")
        .and_then(Value::as_object)
        .and_then(|vars| vars.get(tag))
        .and_then(Value::as_object)
        .cloned();
    if let Some(overlay) = overlay {
        for (key, value) in overlay {
            base_obj.insert(key, value);
        }
    }
    serde_json::from_value(Value::Object(base_obj))
        .context("failed to parse Homebrew cask metadata")
}

fn plan_from_value(payload: Value, requested_token: &str, tag: &str) -> Result<PlannedCask> {
    let raw = merge_variation(payload, tag)?;
    if raw.token.is_empty() {
        bail!("Homebrew API returned cask '{requested_token}' without a token");
    }
    let token = validate_cask_identifier(&raw.token, "cask token")?.to_string();
    let tap = if raw.tap.trim().is_empty() {
        CASK_TAP
    } else {
        raw.tap.trim()
    };
    if tap != CASK_TAP {
        bail!("cask '{token}' comes from unsupported tap '{tap}'; only {CASK_TAP} is supported");
    }
    if raw.disabled {
        let reason = raw
            .disable_reason
            .as_deref()
            .filter(|text| !text.trim().is_empty())
            .unwrap_or("disabled upstream");
        bail!("cask '{token}' is disabled ({reason})");
    }

    let version = parse_cask_version(&raw.version, &token)?;
    let mut install_blockers = Vec::new();

    if !is_supported_macos_tag(tag) {
        install_blockers.push("native cask installation requires a supported macOS tag".into());
    }
    if !raw.supported_platforms.is_empty() && !raw.supported_platforms.iter().any(|p| p == tag) {
        install_blockers.push(format!("does not support host platform '{tag}'"));
    }

    let sha256 = validate_cask_sha256(&raw.sha256, &mut install_blockers);
    let url = validate_plan_url(&raw.url, &mut install_blockers);

    if matches!(&raw.url_specs, Value::Object(map) if !map.is_empty())
        || (!raw.url_specs.is_null() && !raw.url_specs.is_object())
    {
        install_blockers
            .push("declares custom url_specs not supported by this native installer".into());
    }
    if !raw.rename.is_empty() {
        install_blockers
            .push("declares rename steps not supported by this native installer".into());
    }

    let container = review_container(&raw.container, &mut install_blockers);
    review_depends_on(&raw.depends_on, tag, &mut install_blockers);
    let (conflicts_with_casks, conflicts_with_formulae) =
        review_conflicts_with(&token, &raw.conflicts_with, &mut install_blockers);
    let artifacts = review_artifacts(&raw.artifacts, &mut install_blockers);

    Ok(PlannedCask {
        token,
        name: raw.name,
        version,
        auto_updates: raw.auto_updates == Some(true),
        url,
        sha256,
        container,
        artifacts,
        conflicts_with_casks,
        conflicts_with_formulae,
        install_blockers,
    })
}

fn parse_cask_version(value: &Value, token: &str) -> Result<String> {
    let raw = match value {
        Value::String(s) => s.trim(),
        _ => bail!("cask '{token}' does not declare a string version"),
    };
    if raw.is_empty() || raw.len() > MAX_VERSION_LEN {
        bail!("cask '{token}' has an invalid version length");
    }
    if raw == "."
        || raw == ".."
        || raw.contains("..")
        || raw.contains('/')
        || raw.contains('\\')
        || raw.chars().any(|ch| ch.is_control() || ch.is_whitespace())
    {
        bail!("cask '{token}' has an unsafe version string '{raw}'");
    }
    Ok(raw.to_string())
}

fn validate_cask_sha256(value: &Value, blockers: &mut Vec<String>) -> String {
    match value {
        Value::String(s) if s == "no_check" => {
            blockers.push("does not publish a SHA-256 checksum (sha256: no_check)".into());
            String::new()
        }
        Value::String(s) => {
            let lower = s.to_ascii_lowercase();
            if is_lower_hex_digest(&lower) {
                lower
            } else {
                blockers.push("does not declare a valid 64-character SHA-256 checksum".into());
                String::new()
            }
        }
        _ => {
            blockers.push("does not declare a valid 64-character SHA-256 checksum".into());
            String::new()
        }
    }
}

fn validate_plan_url(raw: &str, blockers: &mut Vec<String>) -> String {
    match validate_cask_url(raw) {
        Ok(url) => url.to_string(),
        Err(error) => {
            blockers.push(format!("declares an unsupported download URL: {error}"));
            String::new()
        }
    }
}

fn review_container(value: &Value, blockers: &mut Vec<String>) -> ContainerHint {
    match value {
        Value::Null => ContainerHint::Auto,
        Value::Object(map) if map.is_empty() => ContainerHint::Auto,
        Value::Object(map) => {
            if map.len() == 1
                && let Some(kind) = map.get("type").and_then(Value::as_str)
            {
                return match kind {
                    "naked" => ContainerHint::Naked,
                    "zip" => ContainerHint::Zip,
                    "dmg" => ContainerHint::Dmg,
                    other => {
                        blockers.push(format!("declares an unsupported container type '{other}'"));
                        ContainerHint::Auto
                    }
                };
            }
            blockers.push("declares an unsupported container configuration".into());
            ContainerHint::Auto
        }
        _ => {
            blockers.push("declares an unsupported container configuration".into());
            ContainerHint::Auto
        }
    }
}

fn review_depends_on(value: &Value, tag: &str, blockers: &mut Vec<String>) {
    let map = match value {
        Value::Null => return,
        Value::Object(map) => map,
        _ => {
            blockers.push("declares malformed depends_on metadata".into());
            return;
        }
    };
    for (key, constraint) in map {
        match key.as_str() {
            "macos" => review_cask_macos_dep(constraint, tag, blockers),
            "arch" => review_cask_arch_dep(constraint, tag, blockers),
            other => blockers.push(format!(
                "declares unsupported '{other}' dependency in depends_on"
            )),
        }
    }
}

fn review_cask_macos_dep(value: &Value, tag: &str, blockers: &mut Vec<String>) {
    let Some(host) = macos_version_for_tag(tag) else {
        // Already blocked by non-macOS tag check.
        return;
    };
    let map = match value {
        Value::Null => return,
        Value::Object(map) if map.is_empty() => return,
        Value::Object(map) => map,
        _ => {
            blockers.push("declares an unsupported macOS dependency constraint".into());
            return;
        }
    };
    if map.len() != 1 {
        blockers.push("declares an unsupported macOS dependency constraint".into());
        return;
    }
    let (op, versions_val) = map.iter().next().expect("len == 1");
    let Some(versions_arr) = versions_val.as_array() else {
        blockers.push("declares an unsupported macOS dependency constraint".into());
        return;
    };
    if versions_arr.is_empty() {
        blockers.push("declares an empty macOS dependency constraint".into());
        return;
    }
    let mut parsed_versions = Vec::with_capacity(versions_arr.len());
    for entry in versions_arr {
        let Some(ver_str) = entry.as_str().and_then(parse_macos_version_or_codename) else {
            blockers.push("declares an unrecognized macOS version in depends_on".into());
            return;
        };
        parsed_versions.push(ver_str);
    }

    match op.as_str() {
        ">=" if parsed_versions.len() == 1 => {
            if host < parsed_versions[0] {
                blockers.push(format!(
                    "requires macOS >= {} (host tag '{tag}' is lower)",
                    versions_arr[0].as_str().unwrap_or_default()
                ));
            }
        }
        "==" => {
            if !parsed_versions.contains(&host) {
                blockers.push(format!("does not match host macOS release for '{tag}'"));
            }
        }
        _ => blockers.push(format!(
            "declares unsupported macOS constraint operator '{op}'"
        )),
    }
}

fn review_cask_arch_dep(value: &Value, tag: &str, blockers: &mut Vec<String>) {
    let Some(host_arch) = arch_for_tag(tag) else {
        return;
    };
    let Some(entries) = value.as_array() else {
        blockers.push("declares an unsupported arch constraint in depends_on".into());
        return;
    };
    if entries.is_empty() {
        blockers.push("declares an empty arch constraint in depends_on".into());
        return;
    }
    let mut matches_host = false;
    for entry in entries {
        let Some(obj) = entry.as_object() else {
            blockers.push("declares an unsupported arch constraint in depends_on".into());
            return;
        };
        let Some(arch_type) = obj.get("type").and_then(Value::as_str) else {
            blockers.push("declares an unsupported arch constraint in depends_on".into());
            return;
        };
        let Some(bits) = obj.get("bits").and_then(Value::as_u64) else {
            blockers.push("declares an unsupported arch constraint in depends_on".into());
            return;
        };
        if bits != 64 {
            continue;
        }
        match (arch_type, host_arch) {
            ("arm", "arm64") | ("intel", "x86_64") => matches_host = true,
            ("arm" | "intel", _) => {}
            _ => {
                blockers.push(format!(
                    "declares unrecognized arch type '{arch_type}' in depends_on"
                ));
                return;
            }
        }
    }
    if !matches_host {
        blockers.push(format!(
            "requires a different CPU architecture than '{tag}'"
        ));
    }
}

fn review_conflicts_with(
    own_token: &str,
    value: &Value,
    blockers: &mut Vec<String>,
) -> (Vec<String>, Vec<String>) {
    let map = match value {
        Value::Null => return (Vec::new(), Vec::new()),
        Value::Object(map) => map,
        _ => {
            blockers.push("declares an unsupported conflict rule".into());
            return (Vec::new(), Vec::new());
        }
    };
    let mut casks = Vec::new();
    let mut formulae = Vec::new();
    for (kind, entries) in map {
        let target = match kind.as_str() {
            "cask" => &mut casks,
            "formula" => &mut formulae,
            _ => {
                blockers.push("declares an unsupported conflict rule".into());
                return (Vec::new(), Vec::new());
            }
        };
        let items = match entries {
            Value::Null => continue,
            Value::String(s) => vec![Value::String(s.clone())],
            Value::Array(arr) => arr.clone(),
            _ => {
                blockers.push("declares an unsupported conflict rule".into());
                return (Vec::new(), Vec::new());
            }
        };
        let mut seen = HashSet::new();
        for item in items {
            let Some(name) = item.as_str() else {
                blockers.push("declares an unsupported conflict rule".into());
                return (Vec::new(), Vec::new());
            };
            let Ok(valid) = validate_cask_identifier(name, "conflict identifier") else {
                blockers.push("declares an unsupported conflict rule".into());
                return (Vec::new(), Vec::new());
            };
            if kind == "cask" && valid == own_token {
                blockers.push("declares a self-conflict rule".into());
                return (Vec::new(), Vec::new());
            }
            if seen.insert(valid.to_string()) {
                target.push(valid.to_string());
            }
        }
        target.sort_unstable();
    }
    (casks, formulae)
}

fn review_artifacts(
    raw_artifacts: &[Value],
    blockers: &mut Vec<String>,
) -> Vec<PlannedCaskArtifact> {
    if raw_artifacts.len() > MAX_ARTIFACTS_PER_CASK {
        blockers.push(format!(
            "declares more than {MAX_ARTIFACTS_PER_CASK} artifacts"
        ));
        return Vec::new();
    }

    // Pass 1: validate and collect `app` bundles so pass 2 can verify any
    // `$APPDIR/<Bundle>.app/...` source paths belong to this cask.
    let mut apps = Vec::new();
    let mut declared_bundles = HashSet::new();
    for entry in raw_artifacts {
        let Some(obj) = entry.as_object() else {
            blockers.push("declares malformed artifact metadata".into());
            return Vec::new();
        };
        if let Some(app_val) = obj.get("app")
            && let Some(artifact) = parse_app_artifact(app_val, obj, blockers)
        {
            if let PlannedCaskArtifact::App {
                ref bundle_name, ..
            } = artifact
                && !declared_bundles.insert(bundle_name.clone())
            {
                blockers.push(format!("declares duplicate app bundle '{bundle_name}'"));
                return Vec::new();
            }
            apps.push(artifact);
        }
    }

    // Pass 2: validate all remaining artifact declarations.
    let mut artifacts = apps;
    let mut seen_prefix_dests = HashSet::new();

    for entry in raw_artifacts {
        let Some(obj) = entry.as_object() else {
            return Vec::new();
        };
        let keys: Vec<&str> = obj
            .keys()
            .map(String::as_str)
            .filter(|k| *k != "target")
            .collect();
        if keys.len() != 1 {
            blockers.push("declares an unsupported multi-key artifact entry".into());
            return Vec::new();
        }
        let kind = keys[0];
        let value = &obj[kind];
        match kind {
            "app" | "zap" => {}
            "uninstall" => review_uninstall_artifact(value, blockers),
            "binary" => {
                if let Some(artifact) =
                    parse_binary_artifact(value, obj, &declared_bundles, blockers)
                {
                    if let PlannedCaskArtifact::Binary { ref link_name, .. } = artifact {
                        let dest = format!("bin/{link_name}");
                        if !seen_prefix_dests.insert(dest.clone()) {
                            blockers.push(format!("declares duplicate destination '{dest}'"));
                            return Vec::new();
                        }
                    }
                    artifacts.push(artifact);
                }
            }
            "command_wrapper" => {
                if let Some(artifact) =
                    parse_command_wrapper_artifact(value, obj, &declared_bundles, blockers)
                {
                    if let PlannedCaskArtifact::CommandWrapper { ref link_name, .. } = artifact {
                        let dest = format!("bin/{link_name}");
                        if !seen_prefix_dests.insert(dest.clone()) {
                            blockers.push(format!("declares duplicate destination '{dest}'"));
                            return Vec::new();
                        }
                    }
                    artifacts.push(artifact);
                }
            }
            "manpage" | "bash_completion" | "zsh_completion" | "fish_completion" => {
                if let Some(artifact) =
                    parse_prefix_link_artifact(kind, value, obj, &declared_bundles, blockers)
                {
                    if let PlannedCaskArtifact::PrefixLink {
                        ref relative_dest, ..
                    } = artifact
                        && !seen_prefix_dests.insert(relative_dest.clone())
                    {
                        blockers.push(format!("declares duplicate destination '{relative_dest}'"));
                        return Vec::new();
                    }
                    artifacts.push(artifact);
                }
            }
            other => {
                blockers.push(format!(
                    "declares unsupported '{other}' artifact not supported by this native installer"
                ));
            }
        }
    }

    let has_primary = artifacts.iter().any(|artifact| {
        matches!(
            artifact,
            PlannedCaskArtifact::App { .. }
                | PlannedCaskArtifact::Binary { .. }
                | PlannedCaskArtifact::CommandWrapper { .. }
        )
    });
    if !has_primary && blockers.is_empty() {
        blockers
            .push("does not declare any supported app, binary, or command_wrapper artifact".into());
    }
    artifacts
}

fn review_uninstall_artifact(value: &Value, blockers: &mut Vec<String>) {
    let Some(items) = value.as_array() else {
        blockers.push("declares uninstall steps not supported by this native installer".into());
        return;
    };
    for item in items {
        let Some(map) = item.as_object() else {
            blockers.push("declares uninstall steps not supported by this native installer".into());
            return;
        };
        let supported = map
            .keys()
            .all(|k| matches!(k.as_str(), "quit" | "signal" | "login_item" | "launchctl"));
        if !supported {
            blockers.push("declares uninstall steps not supported by this native installer".into());
            return;
        }
    }
}

fn parse_app_artifact(
    app_val: &Value,
    obj: &Map<String, Value>,
    blockers: &mut Vec<String>,
) -> Option<PlannedCaskArtifact> {
    let (source, inner_target) = parse_two_element_artifact(app_val, "app", blockers)?;
    if !is_safe_relative_payload_path(&source) || !source.ends_with(".app") {
        blockers.push("declares an invalid app source path".into());
        return None;
    }
    let default_name = source.rsplit('/').next().unwrap_or(&source);
    let inner_bundle = match inner_target {
        Some(t) => {
            if !is_safe_single_component(&t) || !t.ends_with(".app") {
                blockers.push("declares an unsupported app target name".into());
                return None;
            }
            Some(t)
        }
        None => None,
    };

    let bundle_name = match obj.get("target") {
        Some(Value::String(target)) => {
            let Some(name) = target.strip_prefix("/Applications/") else {
                blockers.push(format!(
                    "declares app target '{target}' outside /Applications"
                ));
                return None;
            };
            if !is_safe_single_component(name) || !name.ends_with(".app") {
                blockers.push("declares an unsupported app target path".into());
                return None;
            }
            if let Some(ref inner) = inner_bundle
                && inner != name
            {
                blockers.push("declares conflicting app target names".into());
                return None;
            }
            name.to_string()
        }
        Some(_) => {
            blockers.push("declares malformed app target metadata".into());
            return None;
        }
        None => inner_bundle.unwrap_or_else(|| default_name.to_string()),
    };

    Some(PlannedCaskArtifact::App {
        source,
        bundle_name,
    })
}

fn parse_binary_artifact(
    val: &Value,
    obj: &Map<String, Value>,
    declared_bundles: &HashSet<String>,
    blockers: &mut Vec<String>,
) -> Option<PlannedCaskArtifact> {
    let (source_raw, _inner_target) = parse_two_element_artifact(val, "binary", blockers)?;
    let Some(target) = obj.get("target").and_then(Value::as_str) else {
        blockers.push("declares a binary artifact without a target path".into());
        return None;
    };
    let Some(link_name) = target.strip_prefix("$HOMEBREW_PREFIX/bin/") else {
        blockers.push(format!(
            "declares binary target '{target}' outside $HOMEBREW_PREFIX/bin"
        ));
        return None;
    };
    if !is_safe_single_component(link_name) || link_name.contains('$') {
        blockers.push(format!("declares an unsupported binary target '{target}'"));
        return None;
    }
    let source = parse_cask_source_path(&source_raw, declared_bundles, "binary", blockers)?;
    Some(PlannedCaskArtifact::Binary {
        source,
        link_name: link_name.to_string(),
    })
}

fn parse_command_wrapper_artifact(
    val: &Value,
    obj: &Map<String, Value>,
    declared_bundles: &HashSet<String>,
    blockers: &mut Vec<String>,
) -> Option<PlannedCaskArtifact> {
    let Some(arr) = val.as_array() else {
        blockers.push("declares malformed command_wrapper metadata".into());
        return None;
    };
    if arr.len() != 2 {
        blockers.push("declares an unsupported command_wrapper specification".into());
        return None;
    }
    let Some(wrapper_name) = arr[0].as_str() else {
        blockers.push("declares malformed command_wrapper name".into());
        return None;
    };
    let Some(opts) = arr[1].as_object() else {
        blockers.push("declares an unsupported command_wrapper specification".into());
        return None;
    };
    for key in opts.keys() {
        if !matches!(key.as_str(), "executable" | "args") {
            blockers.push(format!(
                "declares command_wrapper option '{key}' not supported by this native installer"
            ));
            return None;
        }
    }
    let Some(exec_raw) = opts.get("executable").and_then(Value::as_str) else {
        blockers.push("declares command_wrapper without an executable".into());
        return None;
    };
    let executable =
        parse_cask_source_path(exec_raw, declared_bundles, "command_wrapper", blockers)?;

    let mut args = Vec::new();
    if let Some(raw_args) = opts.get("args") {
        let Some(items) = raw_args.as_array() else {
            blockers.push("declares malformed command_wrapper args".into());
            return None;
        };
        if items.len() > 32 {
            blockers.push("declares too many command_wrapper arguments".into());
            return None;
        }
        for item in items {
            let Some(arg) = item.as_str() else {
                blockers.push("declares non-string command_wrapper argument".into());
                return None;
            };
            if arg.contains('$') || arg.chars().any(|ch| ch.is_control()) {
                blockers.push("declares unsupported command_wrapper argument".into());
                return None;
            }
            args.push(arg.to_string());
        }
    }

    let Some(target) = obj.get("target").and_then(Value::as_str) else {
        blockers.push("declares command_wrapper without a target path".into());
        return None;
    };
    let Some(link_name) = target.strip_prefix("$HOMEBREW_PREFIX/bin/") else {
        blockers.push(format!(
            "declares command_wrapper target '{target}' outside $HOMEBREW_PREFIX/bin"
        ));
        return None;
    };
    if !is_safe_single_component(link_name)
        || link_name.contains('$')
        || !is_safe_single_component(wrapper_name)
    {
        blockers.push(format!(
            "declares an unsupported command_wrapper target '{target}'"
        ));
        return None;
    }

    Some(PlannedCaskArtifact::CommandWrapper {
        executable,
        link_name: link_name.to_string(),
        args,
    })
}

fn parse_prefix_link_artifact(
    kind: &str,
    val: &Value,
    obj: &Map<String, Value>,
    declared_bundles: &HashSet<String>,
    blockers: &mut Vec<String>,
) -> Option<PlannedCaskArtifact> {
    let (source_raw, _inner_target) = parse_two_element_artifact(val, kind, blockers)?;
    let Some(target) = obj.get("target").and_then(Value::as_str) else {
        blockers.push(format!("declares {kind} artifact without a target path"));
        return None;
    };
    let Some(rel) = target.strip_prefix("$HOMEBREW_PREFIX/") else {
        blockers.push(format!(
            "declares {kind} target '{target}' outside $HOMEBREW_PREFIX"
        ));
        return None;
    };
    let valid_dest = match kind {
        "manpage" => rel.strip_prefix("share/man/").is_some_and(|rest| {
            let Some((section, file)) = rest.split_once('/') else {
                return false;
            };
            matches!(
                section,
                "man1" | "man2" | "man3" | "man4" | "man5" | "man6" | "man7" | "man8"
            ) && is_safe_single_component(file)
                && !file.contains('$')
        }),
        "bash_completion" => rel
            .strip_prefix("etc/bash_completion.d/")
            .is_some_and(|file| is_safe_single_component(file) && !file.contains('$')),
        "zsh_completion" => rel
            .strip_prefix("share/zsh/site-functions/")
            .is_some_and(|file| is_safe_single_component(file) && !file.contains('$')),
        "fish_completion" => rel
            .strip_prefix("share/fish/vendor_completions.d/")
            .is_some_and(|file| is_safe_single_component(file) && !file.contains('$')),
        _ => false,
    };
    if !valid_dest {
        blockers.push(format!("declares an unsupported {kind} target '{target}'"));
        return None;
    }

    let source = parse_cask_source_path(&source_raw, declared_bundles, kind, blockers)?;
    Some(PlannedCaskArtifact::PrefixLink {
        source,
        relative_dest: rel.to_string(),
    })
}

fn parse_two_element_artifact(
    val: &Value,
    kind: &str,
    blockers: &mut Vec<String>,
) -> Option<(String, Option<String>)> {
    let Some(arr) = val.as_array() else {
        blockers.push(format!("declares malformed {kind} artifact metadata"));
        return None;
    };
    match arr.as_slice() {
        [Value::String(source)] => Some((source.clone(), None)),
        [Value::String(source), Value::Object(opts)] => {
            if opts.len() != 1 || !opts.contains_key("target") {
                blockers.push(format!(
                    "declares unsupported {kind} options not supported by this native installer"
                ));
                return None;
            }
            let Some(target) = opts.get("target").and_then(Value::as_str) else {
                blockers.push(format!("declares malformed {kind} target option"));
                return None;
            };
            Some((source.clone(), Some(target.to_string())))
        }
        _ => {
            blockers.push(format!("declares unsupported {kind} artifact array shape"));
            None
        }
    }
}

fn parse_cask_source_path(
    source: &str,
    declared_bundles: &HashSet<String>,
    kind: &str,
    blockers: &mut Vec<String>,
) -> Option<CaskSourcePath> {
    if let Some(rest) = source.strip_prefix("$APPDIR/") {
        let Some((bundle, inner)) = rest.split_once('/') else {
            blockers.push(format!(
                "declares invalid $APPDIR source path in {kind} artifact"
            ));
            return None;
        };
        if !is_safe_single_component(bundle)
            || !bundle.ends_with(".app")
            || !declared_bundles.contains(bundle)
            || !is_safe_relative_payload_path(inner)
            || inner.contains('$')
        {
            blockers.push(format!(
                "declares unsupported $APPDIR source '{source}' in {kind} artifact"
            ));
            return None;
        }
        return Some(CaskSourcePath::AppBundle {
            bundle_name: bundle.to_string(),
            inner_path: inner.to_string(),
        });
    }
    if source.starts_with('/') || source.contains('$') || !is_safe_relative_payload_path(source) {
        blockers.push(format!(
            "declares unsupported source path '{source}' in {kind} artifact"
        ));
        return None;
    }
    Some(CaskSourcePath::Payload(source.to_string()))
}

fn is_safe_single_component(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name.contains('/')
        && !name.contains('\\')
        && !name.contains('\0')
        && !name.chars().any(|ch| ch.is_control())
}

fn is_safe_relative_payload_path(path: &str) -> bool {
    if path.is_empty()
        || path.starts_with('/')
        || path.ends_with('/')
        || path.contains('\\')
        || path.chars().any(|ch| ch.is_control())
    {
        return false;
    }
    path.split('/')
        .all(|segment| !segment.is_empty() && segment != "." && segment != "..")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::HashMap;

    const SHA_A: &str = "f7c5093b9dd6560d08af28bc27b123fb55091deae3180ac39df199522da86815";
    const SHA_B: &str = "bbed99bb8a39bb3f94389c5c99d3d02cb75f3f983b5d1bc40b1b0d1efca84713";

    struct MockCaskSource(HashMap<String, Value>);

    impl CaskSource for MockCaskSource {
        async fn fetch_cask(&self, token: &str) -> Result<Value> {
            self.0
                .get(token)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("missing mock cask {token}"))
        }
    }

    fn plan_json(value: Value, tag: &str) -> PlannedCask {
        plan_from_value(value, "mock", tag).expect("cask metadata should parse")
    }

    #[test]
    fn parses_claude_code_binary_cask_and_applies_variations() {
        let payload = json!({
            "token": "claude-code@latest",
            "tap": "homebrew/cask",
            "name": ["Claude Code"],
            "version": "2.1.330",
            "url": "https://downloads.claude.ai/claude-code-releases/2.1.330/darwin-arm64/claude",
            "sha256": SHA_A,
            "depends_on": {},
            "conflicts_with": {"cask": ["claude-code"]},
            "artifacts": [
                {"binary": ["claude"], "target": "$HOMEBREW_PREFIX/bin/claude"},
                {"zap": [{"trash": ["~/.cache/claude"]}]}
            ],
            "supported_platforms": ["sequoia", "arm64_sequoia"],
            "variations": {
                "sequoia": {
                    "url": "https://downloads.claude.ai/claude-code-releases/2.1.330/darwin-x64/claude",
                    "sha256": SHA_B
                }
            }
        });

        let arm = plan_json(payload.clone(), "arm64_sequoia");
        assert!(
            arm.install_blockers.is_empty(),
            "{:?}",
            arm.install_blockers
        );
        assert_eq!(arm.sha256, SHA_A);
        assert_eq!(arm.conflicts_with_casks, vec!["claude-code"]);
        assert_eq!(
            arm.artifacts,
            vec![PlannedCaskArtifact::Binary {
                source: CaskSourcePath::Payload("claude".into()),
                link_name: "claude".into(),
            }]
        );

        let intel = plan_json(payload, "sequoia");
        assert!(
            intel.install_blockers.is_empty(),
            "{:?}",
            intel.install_blockers
        );
        assert_eq!(intel.sha256, SHA_B);
        assert!(intel.url.contains("darwin-x64"));
    }

    #[test]
    fn parses_app_binary_command_wrapper_and_completions() {
        let iterm = plan_json(
            json!({
                "token": "iterm2",
                "version": "3.7.9",
                "url": "https://iterm2.com/downloads/stable/iTerm2-3_7_9.zip",
                "sha256": SHA_A,
                "auto_updates": true,
                "depends_on": {"macos": {">=": ["13"]}},
                "artifacts": [
                    {"uninstall": [{"quit": "com.googlecode.iterm2"}]},
                    {"app": ["iTerm.app"], "target": "/Applications/iTerm.app"},
                    {"zap": []}
                ]
            }),
            "arm64_sequoia",
        );
        assert!(
            iterm.install_blockers.is_empty(),
            "{:?}",
            iterm.install_blockers
        );
        assert!(iterm.auto_updates);
        assert_eq!(
            iterm.artifacts,
            vec![PlannedCaskArtifact::App {
                source: "iTerm.app".into(),
                bundle_name: "iTerm.app".into(),
            }]
        );

        let vscode = plan_json(
            json!({
                "token": "visual-studio-code",
                "version": "1.147.0",
                "url": "https://update.code.visualstudio.com/1.147.0/darwin-arm64/stable",
                "sha256": SHA_A,
                "auto_updates": true,
                "depends_on": {"macos": {}},
                "artifacts": [
                    {"uninstall": [{"launchctl": "com.microsoft.VSCode.ShipIt", "quit": "com.microsoft.VSCode"}]},
                    {"app": ["Visual Studio Code.app"], "target": "/Applications/Visual Studio Code.app"},
                    {"binary": ["$APPDIR/Visual Studio Code.app/Contents/Resources/app/bin/code"], "target": "$HOMEBREW_PREFIX/bin/code"}
                ]
            }),
            "arm64_sequoia",
        );
        assert!(
            vscode.install_blockers.is_empty(),
            "{:?}",
            vscode.install_blockers
        );
        assert_eq!(vscode.artifacts.len(), 2);

        let firefox = plan_json(
            json!({
                "token": "firefox",
                "version": "160.0.1",
                "url": "https://download-installer.cdn.mozilla.net/pub/firefox/releases/160.0.1/mac/en-US/Firefox%20160.0.1.dmg",
                "sha256": SHA_A,
                "artifacts": [
                    {"uninstall": [{"quit": "org.mozilla.firefox"}]},
                    {"app": ["Firefox.app"], "target": "/Applications/Firefox.app"},
                    {"command_wrapper": ["firefox", {"executable": "$APPDIR/Firefox.app/Contents/MacOS/firefox", "args": ["--no-remote"]}], "target": "$HOMEBREW_PREFIX/bin/firefox"}
                ]
            }),
            "arm64_sequoia",
        );
        assert!(
            firefox.install_blockers.is_empty(),
            "{:?}",
            firefox.install_blockers
        );
        assert_eq!(
            firefox.artifacts[1],
            PlannedCaskArtifact::CommandWrapper {
                executable: CaskSourcePath::AppBundle {
                    bundle_name: "Firefox.app".into(),
                    inner_path: "Contents/MacOS/firefox".into(),
                },
                link_name: "firefox".into(),
                args: vec!["--no-remote".into()],
            }
        );

        let ghostty = plan_json(
            json!({
                "token": "ghostty",
                "version": "1.4.0",
                "url": "https://release.files.ghostty.org/1.4.0/Ghostty.dmg",
                "sha256": SHA_A,
                "depends_on": {"macos": {">=": ["13"]}},
                "artifacts": [
                    {"app": ["Ghostty.app"], "target": "/Applications/Ghostty.app"},
                    {"manpage": ["$APPDIR/Ghostty.app/Contents/Resources/man/man1/ghostty.1"], "target": "$HOMEBREW_PREFIX/share/man/man1/ghostty.1"},
                    {"bash_completion": ["$APPDIR/Ghostty.app/Contents/Resources/bash-completion/completions/ghostty.bash"], "target": "$HOMEBREW_PREFIX/etc/bash_completion.d/ghostty"},
                    {"zsh_completion": ["$APPDIR/Ghostty.app/Contents/Resources/zsh/site-functions/_ghostty"], "target": "$HOMEBREW_PREFIX/share/zsh/site-functions/_ghostty"},
                    {"fish_completion": ["$APPDIR/Ghostty.app/Contents/Resources/fish/vendor_completions.d/ghostty.fish"], "target": "$HOMEBREW_PREFIX/share/fish/vendor_completions.d/ghostty.fish"}
                ]
            }),
            "arm64_sequoia",
        );
        assert!(
            ghostty.install_blockers.is_empty(),
            "{:?}",
            ghostty.install_blockers
        );
        assert_eq!(ghostty.artifacts.len(), 5);
    }

    #[test]
    fn blocks_no_check_pkg_font_home_target_and_generated_completions() {
        let no_check = plan_json(
            json!({
                "token": "google-chrome",
                "version": "158.0",
                "url": "https://dl.google.com/chrome/mac/universal/stable/GGRO/googlechrome.dmg",
                "sha256": "no_check",
                "artifacts": [{"app": ["Google Chrome.app"], "target": "/Applications/Google Chrome.app"}]
            }),
            "arm64_sequoia",
        );
        assert!(
            no_check
                .install_blockers
                .iter()
                .any(|b| b.contains("no_check")),
            "{:?}",
            no_check.install_blockers
        );

        let pkg = plan_json(
            json!({
                "token": "pkg-app",
                "version": "1.0",
                "url": "https://example.com/app.pkg",
                "sha256": SHA_A,
                "artifacts": [{"pkg": ["App.pkg"]}]
            }),
            "arm64_sequoia",
        );
        assert!(pkg.install_blockers.iter().any(|b| b.contains("'pkg'")));

        let font = plan_json(
            json!({
                "token": "font-fira-code",
                "version": "6.2",
                "url": "https://github.com/tonsky/FiraCode/releases/download/6.2/Fira_Code_v6.2.zip",
                "sha256": SHA_A,
                "artifacts": [{"font": ["ttf/FiraCode-Regular.ttf"], "target": "/$HOME/Library/Fonts/FiraCode-Regular.ttf"}]
            }),
            "arm64_sequoia",
        );
        assert!(font.install_blockers.iter().any(|b| b.contains("'font'")));

        let alacritty = plan_json(
            json!({
                "token": "alacritty",
                "version": "0.17.0",
                "url": "https://github.com/alacritty/alacritty/releases/download/v0.17.0/Alacritty-v0.17.0.dmg",
                "sha256": SHA_A,
                "artifacts": [
                    {"app": ["Alacritty.app"], "target": "/Applications/Alacritty.app"},
                    {"binary": ["$APPDIR/Alacritty.app/Contents/Resources/61/alacritty", {"target": "~/.terminfo/61/alacritty"}], "target": "/$HOME/.terminfo/61/alacritty"}
                ]
            }),
            "arm64_sequoia",
        );
        assert!(
            alacritty
                .install_blockers
                .iter()
                .any(|b| b.contains("outside $HOMEBREW_PREFIX/bin"))
        );

        let op_cli = plan_json(
            json!({
                "token": "1password-cli",
                "version": "2.42.0",
                "url": "https://cache.agilebits.com/dist/1P/op2/pkg/v2.42.0/op_darwin_arm64_v2.42.0.zip",
                "sha256": SHA_A,
                "artifacts": [
                    {"binary": ["op"], "target": "$HOMEBREW_PREFIX/bin/op"},
                    {"generate_completions_from_executable": ["op", "completion"]}
                ]
            }),
            "arm64_sequoia",
        );
        assert!(
            op_cli
                .install_blockers
                .iter()
                .any(|b| b.contains("generate_completions_from_executable"))
        );
    }

    #[test]
    fn enforces_macos_and_arch_depends_on_and_deduplicates_targets() {
        let arch_arm = plan_json(
            json!({
                "token": "arm-only",
                "version": "1.0",
                "url": "https://example.com/arm.zip",
                "sha256": SHA_A,
                "depends_on": {"arch": [{"type": "arm", "bits": 64}], "macos": {"==": ["14", "15"]}},
                "artifacts": [{"binary": ["arm-only"], "target": "$HOMEBREW_PREFIX/bin/arm-only"}]
            }),
            "arm64_sequoia",
        );
        assert!(
            arch_arm.install_blockers.is_empty(),
            "{:?}",
            arch_arm.install_blockers
        );

        let arch_fail = plan_json(
            json!({
                "token": "arm-only",
                "version": "1.0",
                "url": "https://example.com/arm.zip",
                "sha256": SHA_A,
                "depends_on": {"arch": [{"type": "intel", "bits": 64}], "macos": {">=": ["26"]}},
                "artifacts": [{"binary": ["arm-only"], "target": "$HOMEBREW_PREFIX/bin/arm-only"}]
            }),
            "arm64_sequoia",
        );
        assert_eq!(arch_fail.install_blockers.len(), 2);

        let mut map = HashMap::new();
        map.insert(
            "demo".into(),
            json!({
                "token": "demo",
                "version": "1.0",
                "url": "https://example.com/demo.zip",
                "sha256": SHA_A,
                "artifacts": [{"binary": ["demo"], "target": "$HOMEBREW_PREFIX/bin/demo"}]
            }),
        );
        let source = MockCaskSource(map);
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let resolved = rt
            .block_on(resolve_with(
                &source,
                &["demo".into(), "homebrew/cask/demo".into()],
                "arm64_sequoia",
            ))
            .unwrap();
        assert_eq!(resolved.len(), 1);
    }

    #[test]
    fn compares_cask_versions_with_optional_build_numbers() {
        assert_eq!(
            compare_cask_versions("2.1.330", "2.1.330"),
            CaskVersionOrder::Same
        );
        assert_eq!(
            compare_cask_versions("2.1.331", "2.1.330"),
            CaskVersionOrder::Upgrade
        );
        assert_eq!(
            compare_cask_versions("2.1.329", "2.1.330"),
            CaskVersionOrder::Downgrade
        );
        assert_eq!(
            compare_cask_versions("1.5,200", "1.5,199"),
            CaskVersionOrder::Upgrade
        );
        assert_eq!(
            compare_cask_versions("1.5,198", "1.5,199"),
            CaskVersionOrder::Downgrade
        );
        assert_eq!(
            compare_cask_versions("1.5,199", "1.5"),
            CaskVersionOrder::Upgrade
        );
        assert_eq!(
            compare_cask_versions("1.5", "1.5,199"),
            CaskVersionOrder::Downgrade
        );
        assert_eq!(
            compare_cask_versions("1.2.3-beta2", "1.2.3-beta1"),
            CaskVersionOrder::Upgrade
        );
    }
}
