//! Native install planning for `homebrew/core`.
//!
//! [`resolve`] turns a list of requested formula names into a dependency-first
//! plan built exclusively from the official Homebrew formula API:
//! `https://formulae.brew.sh/api/formula/<name>.json`. It never runs `brew`,
//! never evaluates Ruby, never writes a file, and never extracts an archive.
//!
//! The planner deliberately supports a single, well-understood slice of
//! Homebrew:
//!
//! * only `homebrew/core` formulae with a stable version and a bottle for the
//!   requested host tag (or the universal `all` bottle) are accepted;
//! * disabled and source-only formulae are rejected;
//! * the planned version carries Homebrew's `_<revision>` suffix when the
//!   formula revision is non-zero;
//! * runtime dependencies are the host-tag `variations` override when one is
//!   present, otherwise the formula's `dependencies` list, and every one of them
//!   is resolved, planned and recorded on the dependent formula under its
//!   canonical name. Build, test, optional, recommended and `uses_from_macos`
//!   dependencies are intentionally not expanded (see the limitations below).
//!
//! The API payload is untrusted data. Every name, version, tag, URL, digest and
//! cellar value is validated before it reaches a [`PlannedFormula`], and the
//! graph is bounded. The digest is only recorded here; it must be verified
//! against the downloaded bytes by the download stage.
//!
//! Known limitations, all consequences of using the static API without running
//! Homebrew's Ruby:
//!
//! * Alias names are not resolvable: `formula/<alias>.json` returns 404, so
//!   targets must be canonical formula names. Requests and dependencies that
//!   resolve to the same canonical `name` are deduplicated.
//! * `uses_from_macos` is not expanded. The API mixes runtime entries,
//!   build/test-scoped objects, macOS version bounds and alias names that the
//!   per-formula endpoint cannot resolve, so expanding it statically would be
//!   unsound. Linux plans can therefore omit an OS-shim dependency that
//!   Homebrew itself would install, and a formula that declares one is marked
//!   with an install blocker instead of being installed incompletely.
//! * Only direct runtime `dependencies` are planned. `recommended_dependencies`
//!   are refused with an install blocker because Homebrew would install them by
//!   default.
//! * Dependency metadata comes from the API only. Its variation lists are used
//!   verbatim, so no Ruby-side conditional logic is evaluated.
//! * Deprecated formulae are allowed when they are still bottled; disabled
//!   formulae are rejected.
//! * Metadata is fetched sequentially to keep ordering deterministic and the
//!   request volume bounded.

use std::collections::{HashMap, HashSet, VecDeque};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::api::http_client::HttpClient;
use crate::config::Config;

/// Official Homebrew formula API base.
const API_BASE: &str = "https://formulae.brew.sh/api";
/// Tap supported by this planner.
const CORE_TAP: &str = "homebrew/core";
/// Host that serves official Homebrew bottles.
const BOTTLE_HOST: &str = "ghcr.io";
/// Path prefix shared by official `homebrew/core` bottle blobs.
const BOTTLE_URL_PREFIX: &str = "https://ghcr.io/v2/homebrew/core/";

/// Maximum number of targets accepted in one plan.
const MAX_TARGETS: usize = 128;
/// Maximum number of distinct formulae in one plan.
const MAX_FORMULAE: usize = 1024;
/// Maximum number of distinct names (targets plus dependencies) queued.
const MAX_REQUESTS: usize = 4096;
/// Maximum runtime dependencies read from a single formula.
const MAX_DEPS_PER_FORMULA: usize = 256;
/// Maximum length of a formula name.
const MAX_NAME_LEN: usize = 64;
/// Maximum length of a rendered version.
const MAX_VERSION_LEN: usize = 64;
/// Maximum length of a host tag.
const MAX_TAG_LEN: usize = 64;
/// Maximum length of a bottle URL.
const MAX_URL_LEN: usize = 2048;
/// Maximum length of a bottle cellar path.
const MAX_CELLAR_LEN: usize = 1024;
/// Length of a SHA-256 digest in hexadecimal.
const SHA256_HEX_LEN: usize = 64;

/// One formula in a native install plan.
///
/// `name` is the canonical API name, `version` is the stable version with the
/// Homebrew revision suffix when needed, and `requested` distinguishes an
/// explicit target from a dependency pulled in to satisfy one. `dependencies`
/// lists the canonical names of this formula's direct runtime dependencies in
/// API order, deduplicated, so a consumer can rely on every entry appearing
/// earlier in the plan. `url`, `sha256` and `cellar` come from the selected
/// bottle entry for the plan's host tag.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedFormula {
    pub name: String,
    pub version: String,
    pub version_scheme: Option<u32>,
    pub requested: bool,
    pub dependencies: Vec<String>,
    pub url: String,
    pub sha256: String,
    pub cellar: String,
    pub install_blockers: Vec<&'static str>,
}

/// Build a native install plan for `targets` on host tag `tag`.
///
/// Targets are bare canonical formula names, or `homebrew/core/<name>`; any
/// other tap is rejected. `tag` is a Homebrew bottle platform key such as
/// `arm64_sequoia`, `x86_64_linux`, or `arm64_linux`. The returned vector is
/// dependency-first, stable for the same inputs and API payload, and contains
/// each canonical formula once.
///
/// # Errors
///
/// Returns an error for an unsafe target or tag, an unsupported tap, a missing
/// formula, a disabled or source-only formula, a formula without a bottle for
/// `tag`, malformed metadata, a dependency cycle, or an over-large graph.
pub async fn resolve(
    config: &Config,
    targets: &[String],
    tag: &str,
) -> anyhow::Result<Vec<PlannedFormula>> {
    let source = HttpFormulaSource {
        http: HttpClient::new(config)?,
    };
    resolve_with(&source, targets, tag).await
}

/// A source of formula metadata, kept as a seam for offline fixture tests.
trait FormulaSource {
    async fn fetch(&self, name: &str) -> Result<FormulaResponse>;
}

/// [`FormulaSource`] backed by the official API over [`HttpClient`].
struct HttpFormulaSource {
    http: HttpClient,
}

impl FormulaSource for HttpFormulaSource {
    async fn fetch(&self, name: &str) -> Result<FormulaResponse> {
        let url = format!("{API_BASE}/formula/{name}.json");
        self.http
            .fetch_json(&url)
            .await
            .with_context(|| format!("failed to fetch formula '{name}' from {url}"))
    }
}

/// Resolve the plan through an arbitrary [`FormulaSource`].
async fn resolve_with<S: FormulaSource>(
    source: &S,
    targets: &[String],
    tag: &str,
) -> Result<Vec<PlannedFormula>> {
    validate_tag(tag)?;
    if targets.len() > MAX_TARGETS {
        bail!(
            "too many targets: {} requested, at most {MAX_TARGETS} are supported",
            targets.len()
        );
    }

    // Parse and validate every target before doing any I/O.
    let mut requested_names = Vec::with_capacity(targets.len());
    for target in targets {
        requested_names.push(parse_target(target)?.to_string());
    }

    // Graph discovery. `canonical_for` maps every name that was requested
    // (target or dependency) to the canonical name returned by the API, which
    // is what deduplicates shared dependencies and canonical duplicates.
    let mut canonical_for: HashMap<String, String> = HashMap::new();
    let mut nodes: HashMap<String, FormulaNode> = HashMap::new();
    let mut queue: VecDeque<String> = VecDeque::new();
    let mut enqueued: HashSet<String> = HashSet::new();

    for name in &requested_names {
        if enqueued.insert(name.clone()) {
            queue.push_back(name.clone());
        }
    }

    while let Some(requested) = queue.pop_front() {
        if canonical_for.contains_key(&requested) {
            continue;
        }

        let response = source
            .fetch(&requested)
            .await
            .with_context(|| format!("while resolving '{requested}'"))?;
        let (canonical, node) = build_node(response, tag)?;
        if let Some(existing) = canonical_for.get(&canonical) {
            if existing != &canonical {
                bail!(
                    "inconsistent formula metadata: '{canonical}' was already resolved as '{existing}'"
                );
            }
        }
        // Map the canonical name to itself so a request for it does not fetch
        // the same payload twice, then map the requested name.
        canonical_for.insert(canonical.clone(), canonical.clone());
        canonical_for.insert(requested, canonical.clone());

        if !nodes.contains_key(&canonical) {
            if nodes.len() >= MAX_FORMULAE {
                bail!("install plan exceeds the {MAX_FORMULAE}-formula limit");
            }
            for dependency in &node.dependencies {
                if enqueued.insert(dependency.clone()) {
                    if enqueued.len() > MAX_REQUESTS {
                        bail!("install plan exceeds the {MAX_REQUESTS}-request limit");
                    }
                    queue.push_back(dependency.clone());
                }
            }
            nodes.insert(canonical, node);
        }
    }

    // Preserve target order while deduplicating requests that share a
    // canonical name.
    let mut target_names = Vec::new();
    let mut requested_canonical = HashSet::new();
    for name in &requested_names {
        let canonical = canonical_for
            .get(name)
            .with_context(|| format!("internal planning error: unresolved target '{name}'"))?;
        if requested_canonical.insert(canonical.clone()) {
            target_names.push(canonical.clone());
        }
    }

    topological_order(&nodes, &canonical_for, &target_names, &requested_canonical)
}

/// Build and validate one graph node from an API payload.
fn build_node(response: FormulaResponse, tag: &str) -> Result<(String, FormulaNode)> {
    let name = validate_formula_name(&response.name, "formula name")?.to_string();

    if response.tap != CORE_TAP {
        bail!(
            "unsupported tap '{}' for formula '{name}': only {CORE_TAP} is supported",
            response.tap
        );
    }
    if response.disabled {
        bail!("formula '{name}' is disabled and cannot be installed");
    }

    if !response.versions.bottle {
        bail!(
            "formula '{name}' has no bottled stable version (source-only builds are not supported)"
        );
    }
    let stable = response
        .versions
        .stable
        .as_deref()
        .with_context(|| format!("formula '{name}' has no stable version"))?;
    if stable.is_empty() {
        bail!("formula '{name}' has an empty stable version");
    }

    let bottle = response
        .bottle
        .as_ref()
        .and_then(|bottle| bottle.stable.as_ref())
        .with_context(|| format!("formula '{name}' has no stable bottle metadata"))?;
    // `all` is Homebrew's platform-independent bottle.
    let bottle_file = bottle
        .files
        .get(tag)
        .or_else(|| bottle.files.get("all"))
        .with_context(|| {
            format!(
                "formula '{name}' has no bottle for host tag '{tag}' (available: {})",
                available_tags(&bottle.files)
            )
        })?;

    let revision = response.revision.unwrap_or(0);
    let version = if revision == 0 {
        stable.to_string()
    } else {
        format!("{stable}_{revision}")
    };
    validate_version(&version, &name)?;

    let sha256 = validate_sha256(&bottle_file.sha256, &name)?;
    let url = validate_bottle_url(&bottle_file.url, &sha256, &name)?;
    let cellar = validate_cellar(&bottle_file.cellar, &name)?;

    let dependency_list = response
        .variations
        .get(tag)
        .and_then(|variation| variation.dependencies.as_ref())
        .unwrap_or(&response.dependencies);
    if dependency_list.len() > MAX_DEPS_PER_FORMULA {
        bail!(
            "formula '{name}' declares {} dependencies, above the {MAX_DEPS_PER_FORMULA} limit",
            dependency_list.len()
        );
    }

    let mut dependencies = Vec::with_capacity(dependency_list.len());
    let mut seen = HashSet::new();
    for dependency in dependency_list {
        let dependency = validate_formula_name(dependency, "dependency name")?.to_string();
        if seen.insert(dependency.clone()) {
            dependencies.push(dependency);
        }
    }

    // The first native installer handles portable formulae and their direct
    // runtime dependencies. Planning and fetching remain available for all
    // supported bottle types, and anything the installer cannot reproduce
    // faithfully stays behind an install blocker.
    let mut install_blockers = Vec::new();
    if response.version_scheme != Some(0) {
        install_blockers.push("uses an unsupported or unknown Homebrew version scheme");
    }
    if cellar != ":any_skip_relocation" {
        install_blockers.push("requires Homebrew relocation or a fixed prefix");
    }
    if !response.recommended_dependencies.is_empty() || !response.uses_from_macos.is_empty() {
        install_blockers
            .push("declares recommended or uses_from_macos dependencies not supported by this native installer");
    }
    if response.post_install_defined != Some(false) {
        install_blockers.push("has a post-install hook or does not declare hook status");
    }
    if response.keg_only != Some(false) || !response.conflicts_with.is_empty() {
        install_blockers.push("has Homebrew linking or conflict rules");
    }
    if !response.service.is_null()
        || !response.pour_bottle_only_if.is_null()
        || !response.requirements.is_empty()
    {
        install_blockers.push("declares a service or additional install requirements");
    }
    if response
        .variations
        .get(tag)
        .is_some_and(|variation| !variation.extra.is_empty())
    {
        install_blockers
            .push("declares platform-specific metadata not understood by the installer");
    }

    Ok((
        name.clone(),
        FormulaNode {
            name,
            version,
            version_scheme: response.version_scheme,
            dependencies,
            url,
            sha256,
            cellar,
            install_blockers,
        },
    ))
}

/// Produce a dependency-first, stable topological ordering.
fn topological_order(
    nodes: &HashMap<String, FormulaNode>,
    canonical_for: &HashMap<String, String>,
    targets: &[String],
    requested: &HashSet<String>,
) -> Result<Vec<PlannedFormula>> {
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Mark {
        Visiting,
        Done,
    }

    enum Step {
        Enter(String),
        Exit(String),
    }

    let mut marks: HashMap<String, Mark> = HashMap::new();
    let mut order = Vec::with_capacity(nodes.len());
    let mut stack = Vec::new();

    for target in targets {
        if marks.get(target) == Some(&Mark::Done) {
            continue;
        }
        stack.push(Step::Enter(target.clone()));

        while let Some(step) = stack.pop() {
            match step {
                Step::Enter(name) => {
                    match marks.get(&name) {
                        Some(Mark::Done) => continue,
                        Some(Mark::Visiting) => {
                            bail!("dependency cycle detected involving formula '{name}'")
                        }
                        None => {}
                    }

                    let node = nodes.get(&name).with_context(|| {
                        format!("internal planning error: missing node '{name}'")
                    })?;
                    marks.insert(name.clone(), Mark::Visiting);
                    stack.push(Step::Exit(name));

                    // Push in reverse so dependencies are visited in API order.
                    for dependency in node.dependencies.iter().rev() {
                        let canonical = canonical_for.get(dependency).with_context(|| {
                            format!(
                                "internal planning error: unresolved dependency '{dependency}' of '{}'",
                                node.name
                            )
                        })?;
                        match marks.get(canonical) {
                            Some(Mark::Done) => {}
                            Some(Mark::Visiting) => {
                                bail!("dependency cycle detected involving formula '{canonical}'")
                            }
                            None => stack.push(Step::Enter(canonical.clone())),
                        }
                    }
                }
                Step::Exit(name) => {
                    marks.insert(name.clone(), Mark::Done);
                    let node = nodes.get(&name).with_context(|| {
                        format!("internal planning error: missing node '{name}'")
                    })?;
                    let dependencies = canonical_dependencies(node, canonical_for)?;
                    order.push(PlannedFormula {
                        name: node.name.clone(),
                        version: node.version.clone(),
                        version_scheme: node.version_scheme,
                        requested: requested.contains(&name),
                        dependencies,
                        url: node.url.clone(),
                        sha256: node.sha256.clone(),
                        cellar: node.cellar.clone(),
                        install_blockers: node.install_blockers.clone(),
                    });
                }
            }
        }
    }

    Ok(order)
}

/// Resolve a node's requested dependency names to canonical plan names.
///
/// The result keeps the formula's API order and drops repeated canonical names
/// (possible when two requests resolve to the same formula), so the edge list is
/// deterministic. Every dependency has already been discovered and ordered
/// before this runs, and cycles are rejected during traversal.
fn canonical_dependencies(
    node: &FormulaNode,
    canonical_for: &HashMap<String, String>,
) -> Result<Vec<String>> {
    let mut dependencies = Vec::with_capacity(node.dependencies.len());
    let mut seen = HashSet::new();
    for dependency in &node.dependencies {
        let canonical = canonical_for.get(dependency).with_context(|| {
            format!(
                "internal planning error: unresolved dependency '{dependency}' of '{}'",
                node.name
            )
        })?;
        if seen.insert(canonical.clone()) {
            dependencies.push(canonical.clone());
        }
    }
    Ok(dependencies)
}

/// A validated formula after the fields needed by the plan have been checked.
struct FormulaNode {
    name: String,
    version: String,
    version_scheme: Option<u32>,
    dependencies: Vec<String>,
    url: String,
    sha256: String,
    cellar: String,
    install_blockers: Vec<&'static str>,
}

/// Parse a target into a bare formula name, rejecting unsupported taps.
pub(super) fn parse_target(raw: &str) -> Result<&str> {
    if raw.is_empty() {
        bail!("target formula names must not be empty");
    }
    if raw.len() > MAX_NAME_LEN + CORE_TAP.len() + 1 {
        bail!("target '{raw}' is longer than the supported maximum");
    }

    let name = match raw.rsplit_once('/') {
        Some((tap, name)) => {
            if tap != CORE_TAP {
                bail!(
                    "unsupported tap '{tap}' in target '{raw}': only {CORE_TAP} formulae are supported"
                );
            }
            name
        }
        None => raw,
    };

    validate_formula_name(name, "target formula name")
}

/// Validate a host tag: lowercase ASCII, digits and underscores only.
fn validate_tag(tag: &str) -> Result<()> {
    if tag.is_empty() || tag.len() > MAX_TAG_LEN {
        bail!("host tag must be between 1 and {MAX_TAG_LEN} characters");
    }
    let mut characters = tag.chars();
    match characters.next() {
        Some(first) if first.is_ascii_lowercase() || first.is_ascii_digit() => {}
        _ => bail!("host tag '{tag}' must start with a lowercase letter or digit"),
    }
    if !tag.chars().all(|character| {
        character.is_ascii_lowercase() || character.is_ascii_digit() || character == '_'
    }) {
        bail!("host tag '{tag}' may only contain lowercase letters, digits and underscores");
    }
    Ok(())
}

/// Validate a Homebrew formula identifier and return it unchanged.
fn validate_formula_name<'a>(value: &'a str, what: &str) -> Result<&'a str> {
    if value.is_empty() || value.len() > MAX_NAME_LEN {
        bail!("{what} '{value}' must be between 1 and {MAX_NAME_LEN} characters");
    }

    let mut characters = value.chars();
    match characters.next() {
        Some(first) if first.is_ascii_lowercase() || first.is_ascii_digit() => {}
        _ => bail!("{what} '{value}' must start with a lowercase letter or digit"),
    }
    if value.contains("..") {
        bail!("{what} '{value}' must not contain '..'");
    }
    if !value.chars().all(|character| {
        character.is_ascii_lowercase()
            || character.is_ascii_digit()
            || matches!(character, '@' | '+' | '-' | '_' | '.')
    }) {
        bail!("{what} '{value}' contains characters outside [a-z0-9@+._-]");
    }
    Ok(value)
}

/// Validate a rendered version string.
fn validate_version(version: &str, formula: &str) -> Result<()> {
    if version.is_empty() || version.len() > MAX_VERSION_LEN {
        bail!("formula '{formula}' has an invalid version length");
    }
    if !version.chars().all(|character| {
        character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '+' | '-')
    }) {
        bail!("formula '{formula}' has an invalid version '{version}'");
    }
    Ok(())
}

/// Validate and normalize a bottle digest to lowercase hexadecimal.
fn validate_sha256(value: &str, formula: &str) -> Result<String> {
    if value.len() != SHA256_HEX_LEN
        || !value.chars().all(|character| character.is_ascii_hexdigit())
    {
        bail!("formula '{formula}' has an invalid bottle sha256");
    }
    Ok(value.to_ascii_lowercase())
}

/// Validate the bottle URL and require it to agree with `sha256`.
fn validate_bottle_url(value: &str, sha256: &str, formula: &str) -> Result<String> {
    if value.is_empty() || value.len() > MAX_URL_LEN {
        bail!("formula '{formula}' has an invalid bottle URL length");
    }
    if !value.is_ascii()
        || value
            .chars()
            .any(|character| character.is_control() || character.is_whitespace())
    {
        bail!(
            "formula '{formula}' has a bottle URL with whitespace, control or non-ASCII characters"
        );
    }
    let Some(rest) = value.strip_prefix("https://") else {
        bail!("formula '{formula}' bottle URL must use https");
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    if authority != BOTTLE_HOST {
        bail!("formula '{formula}' bottle URL must point at {BOTTLE_HOST}");
    }
    let Some(rest) = value.strip_prefix(BOTTLE_URL_PREFIX) else {
        bail!("formula '{formula}' bottle URL must be an official homebrew/core blob");
    };
    let Some((repository, embedded)) = rest.rsplit_once("/blobs/sha256:") else {
        bail!("formula '{formula}' bottle URL must end in /blobs/sha256:<digest>");
    };
    if repository.is_empty()
        || repository.split('/').any(|segment| {
            segment.is_empty()
                || segment == "."
                || segment == ".."
                || !segment.chars().all(|character| {
                    character.is_ascii_lowercase()
                        || character.is_ascii_digit()
                        || matches!(character, '@' | '+' | '-' | '_' | '.')
                })
        })
    {
        bail!("formula '{formula}' bottle URL has an invalid repository path");
    }
    if embedded != sha256 {
        bail!("formula '{formula}' bottle URL digest does not match its sha256");
    }
    Ok(value.to_string())
}

/// Validate the bottle cellar: the two Homebrew sentinels or an absolute,
/// normalized path.
fn validate_cellar(value: &str, formula: &str) -> Result<String> {
    if value == ":any" || value == ":any_skip_relocation" {
        return Ok(value.to_string());
    }
    if value.is_empty() || value.len() > MAX_CELLAR_LEN {
        bail!("formula '{formula}' has an invalid bottle cellar");
    }
    if !value.is_ascii()
        || value
            .chars()
            .any(|character| character.is_control() || character.is_whitespace())
    {
        bail!("formula '{formula}' bottle cellar contains whitespace or control characters");
    }
    if !value.starts_with('/') || value.contains('\\') || value.contains("//") {
        bail!("formula '{formula}' bottle cellar must be an absolute path");
    }
    let components: Vec<&str> = value[1..].split('/').collect();
    if components.is_empty()
        || components
            .iter()
            .any(|component| component.is_empty() || *component == "." || *component == "..")
    {
        bail!("formula '{formula}' bottle cellar is not a normalized path");
    }
    Ok(value.to_string())
}

/// Render the available bottle tags for an error message, deterministically.
fn available_tags(files: &HashMap<String, BottleFile>) -> String {
    if files.is_empty() {
        return "none".to_string();
    }
    let mut tags: Vec<&str> = files.keys().map(String::as_str).collect();
    tags.sort_unstable();
    tags.join(", ")
}

/// The subset of the official formula payload used by the planner.
#[derive(Debug, Deserialize)]
struct FormulaResponse {
    #[serde(default)]
    name: String,
    #[serde(default)]
    tap: String,
    #[serde(default)]
    versions: FormulaVersions,
    #[serde(default)]
    revision: Option<u32>,
    version_scheme: Option<u32>,
    #[serde(default)]
    dependencies: Vec<String>,
    #[serde(default)]
    variations: HashMap<String, Variation>,
    #[serde(default)]
    bottle: Option<Bottle>,
    #[serde(default)]
    disabled: bool,
    post_install_defined: Option<bool>,
    keg_only: Option<bool>,
    #[serde(default)]
    recommended_dependencies: Vec<String>,
    #[serde(default)]
    uses_from_macos: Vec<serde_json::Value>,
    #[serde(default)]
    conflicts_with: Vec<String>,
    #[serde(default)]
    service: serde_json::Value,
    #[serde(default)]
    pour_bottle_only_if: serde_json::Value,
    #[serde(default)]
    requirements: Vec<serde_json::Value>,
}

#[derive(Debug, Deserialize, Default)]
struct FormulaVersions {
    #[serde(default)]
    stable: Option<String>,
    #[serde(default)]
    bottle: bool,
}

#[derive(Debug, Deserialize)]
struct Variation {
    /// `None` means the variation does not override dependencies; `Some(vec![])`
    /// means it explicitly removes them.
    #[serde(default)]
    dependencies: Option<Vec<String>>,
    #[serde(flatten)]
    extra: HashMap<String, serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct Bottle {
    #[serde(default)]
    stable: Option<BottleStable>,
}

#[derive(Debug, Deserialize)]
struct BottleStable {
    #[serde(default)]
    files: HashMap<String, BottleFile>,
}

#[derive(Debug, Deserialize)]
struct BottleFile {
    #[serde(default)]
    cellar: String,
    #[serde(default)]
    url: String,
    #[serde(default)]
    sha256: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    const TAG: &str = "arm64_sequoia";
    const SHA: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    struct FixtureSource {
        docs: HashMap<String, String>,
    }

    impl FixtureSource {
        fn new(docs: Vec<(&str, String)>) -> Self {
            Self {
                docs: docs
                    .into_iter()
                    .map(|(name, doc)| (name.to_string(), doc))
                    .collect(),
            }
        }
    }

    impl FormulaSource for FixtureSource {
        async fn fetch(&self, name: &str) -> Result<FormulaResponse> {
            let doc = self
                .docs
                .get(name)
                .with_context(|| format!("no fixture for '{name}'"))?;
            serde_json::from_str(doc).with_context(|| format!("invalid fixture for '{name}'"))
        }
    }

    fn formula_json(name: &str, version: &str, revision: u32, dependencies: &[&str]) -> String {
        formula_json_for_tag(
            name,
            version,
            revision,
            dependencies,
            TAG,
            "/opt/homebrew/Cellar",
        )
    }

    fn formula_json_for_tag(
        name: &str,
        version: &str,
        revision: u32,
        dependencies: &[&str],
        tag: &str,
        cellar: &str,
    ) -> String {
        let file = serde_json::json!({
            "cellar": cellar,
            "url": format!("https://ghcr.io/v2/homebrew/core/{name}/blobs/sha256:{SHA}"),
            "sha256": SHA,
        });
        let mut files = serde_json::Map::new();
        files.insert(tag.to_string(), file);
        serde_json::json!({
            "name": name,
            "tap": CORE_TAP,
            "versions": { "stable": version, "bottle": true },
            "revision": revision,
            "version_scheme": 0,
            "keg_only": false,
            "post_install_defined": false,
            "dependencies": dependencies,
            "bottle": { "stable": { "files": files } }
        })
        .to_string()
    }

    /// A fixture whose bottle is relocatable-free, so it can be installed by the
    /// portable native engine unless something else blocks it.
    fn portable_formula_json(name: &str, version: &str, dependencies: &[&str]) -> String {
        let mut doc: serde_json::Value =
            serde_json::from_str(&formula_json(name, version, 0, dependencies))
                .expect("fixture json");
        doc["bottle"]["stable"]["files"][TAG]["cellar"] = serde_json::json!(":any_skip_relocation");
        doc.to_string()
    }

    #[tokio::test]
    async fn orders_dependencies_first_and_deduplicates_shared_ones() {
        let source = FixtureSource::new(vec![
            ("app", formula_json("app", "1.2.3", 2, &["lib", "util"])),
            ("lib", formula_json("lib", "2.0.0", 0, &["util"])),
            ("util", formula_json("util", "3.0.0", 0, &[])),
        ]);

        let plan = resolve_with(&source, &["app".into(), "lib".into()], TAG)
            .await
            .expect("plan");

        let names: Vec<&str> = plan.iter().map(|formula| formula.name.as_str()).collect();
        assert_eq!(names, ["util", "lib", "app"]);
        assert!(
            plan.iter()
                .find(|formula| formula.name == "app")
                .unwrap()
                .requested
        );
        assert!(
            plan.iter()
                .find(|formula| formula.name == "lib")
                .unwrap()
                .requested
        );
        assert!(
            !plan
                .iter()
                .find(|formula| formula.name == "util")
                .unwrap()
                .requested
        );
        assert_eq!(
            plan.iter()
                .find(|formula| formula.name == "app")
                .unwrap()
                .version,
            "1.2.3_2"
        );
        assert_eq!(
            plan.iter()
                .find(|formula| formula.name == "lib")
                .unwrap()
                .version,
            "2.0.0"
        );
        assert_eq!(plan[0].cellar, "/opt/homebrew/Cellar");
        assert_eq!(plan[0].sha256, SHA);
        assert!(
            plan[0]
                .url
                .starts_with("https://ghcr.io/v2/homebrew/core/util/")
        );
        let app = plan.iter().find(|formula| formula.name == "app").unwrap();
        let lib = plan.iter().find(|formula| formula.name == "lib").unwrap();
        let util = plan.iter().find(|formula| formula.name == "util").unwrap();
        assert_eq!(app.dependencies, ["lib", "util"]);
        assert_eq!(lib.dependencies, ["util"]);
        assert!(util.dependencies.is_empty());
    }

    #[tokio::test]
    async fn records_canonical_dependency_edges_in_api_order() {
        let source = FixtureSource::new(vec![
            (
                "app",
                formula_json("app", "1.0.0", 0, &["z-lib", "a-alias", "a-lib"]),
            ),
            ("a-alias", formula_json("a-lib", "1.0.0", 0, &[])),
            ("a-lib", formula_json("a-lib", "1.0.0", 0, &[])),
            ("z-lib", formula_json("z-lib", "1.0.0", 0, &[])),
        ]);

        let plan = resolve_with(&source, &["app".into()], TAG)
            .await
            .expect("plan");

        let names: Vec<&str> = plan.iter().map(|formula| formula.name.as_str()).collect();
        assert_eq!(names, ["z-lib", "a-lib", "app"]);

        let app = plan.iter().find(|formula| formula.name == "app").unwrap();
        // `a-alias` and `a-lib` collapse to one canonical name; the surviving
        // order follows the formula's API order, not discovery order.
        assert_eq!(app.dependencies, ["z-lib", "a-lib"]);
        assert_eq!(plan[0].name, "z-lib");
        assert_eq!(plan[1].name, "a-lib");

        // Every recorded edge points at a formula installed earlier.
        let positions: HashMap<&str, usize> = plan
            .iter()
            .enumerate()
            .map(|(index, formula)| (formula.name.as_str(), index))
            .collect();
        for (index, formula) in plan.iter().enumerate() {
            for dependency in &formula.dependencies {
                let position = positions
                    .get(dependency.as_str())
                    .unwrap_or_else(|| panic!("missing node for dependency '{dependency}'"));
                assert!(
                    *position < index,
                    "{dependency} must precede {}",
                    formula.name
                );
            }
        }
    }

    #[tokio::test]
    async fn portable_runtime_dependencies_plan_without_blocker() {
        let source = FixtureSource::new(vec![
            ("app", portable_formula_json("app", "1.0.0", &["lib"])),
            ("lib", portable_formula_json("lib", "2.0.0", &[])),
        ]);

        let plan = resolve_with(&source, &["app".into()], TAG)
            .await
            .expect("plan");

        let names: Vec<&str> = plan.iter().map(|formula| formula.name.as_str()).collect();
        assert_eq!(names, ["lib", "app"]);
        assert!(
            plan.iter()
                .all(|formula| formula.install_blockers.is_empty()),
            "portable runtime dependencies must not block installation: {:?}",
            plan.iter()
                .map(|formula| (&formula.name, &formula.install_blockers))
                .collect::<Vec<_>>()
        );
        assert_eq!(plan[0].dependencies, Vec::<String>::new());
        assert_eq!(plan[1].dependencies, ["lib"]);
        assert!(plan[1].requested);
        assert!(!plan[0].requested);
    }

    #[tokio::test]
    async fn deduplicates_requests_that_share_a_canonical_name() {
        let source = FixtureSource::new(vec![
            ("alias", formula_json("canonical", "1.0.0", 0, &[])),
            ("canonical", formula_json("canonical", "1.0.0", 0, &[])),
        ]);

        let plan = resolve_with(&source, &["alias".into(), "canonical".into()], TAG)
            .await
            .expect("plan");

        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].name, "canonical");
        assert!(plan[0].requested);
    }

    #[tokio::test]
    async fn uses_host_tag_variation_runtime_dependencies() {
        let tool = serde_json::json!({
            "name": "tool",
            "tap": CORE_TAP,
            "versions": { "stable": "1.0.0", "bottle": true },
            "revision": 0,
            "dependencies": ["mac-only"],
            "variations": {
                "arm64_linux": { "dependencies": ["linux-only"] }
            },
            "bottle": {
                "stable": {
                    "files": {
                        "arm64_linux": {
                            "cellar": "/home/linuxbrew/.linuxbrew/Cellar",
                            "url": format!("https://ghcr.io/v2/homebrew/core/tool/blobs/sha256:{SHA}"),
                            "sha256": SHA,
                        }
                    }
                }
            }
        })
        .to_string();
        let source = FixtureSource::new(vec![
            ("tool", tool),
            (
                "linux-only",
                formula_json_for_tag(
                    "linux-only",
                    "1.0.0",
                    0,
                    &[],
                    "arm64_linux",
                    "/home/linuxbrew/.linuxbrew/Cellar",
                ),
            ),
            ("mac-only", formula_json("mac-only", "1.0.0", 0, &[])),
        ]);

        let plan = resolve_with(&source, &["tool".into()], "arm64_linux")
            .await
            .expect("plan");
        let names: Vec<&str> = plan.iter().map(|formula| formula.name.as_str()).collect();
        assert_eq!(names, ["linux-only", "tool"]);
        assert_eq!(plan[1].cellar, "/home/linuxbrew/.linuxbrew/Cellar");
        // The selected variation replaces the runtime dependency edges too.
        assert_eq!(plan[1].dependencies, ["linux-only"]);
    }

    #[tokio::test]
    async fn blockers_remain_for_recommended_and_uses_from_macos() {
        for field in ["recommended_dependencies", "uses_from_macos"] {
            let mut doc: serde_json::Value =
                serde_json::from_str(&portable_formula_json("tool", "1.0.0", &[]))
                    .expect("fixture json");
            doc[field] = serde_json::json!(["optional-lib"]);
            let source = FixtureSource::new(vec![("tool", doc.to_string())]);

            let plan = resolve_with(&source, &["tool".into()], TAG)
                .await
                .expect("plan");

            assert_eq!(plan.len(), 1, "{field} must not be expanded into the plan");
            assert!(
                plan[0]
                    .install_blockers
                    .iter()
                    .any(|blocker| blocker.contains("recommended or uses_from_macos")),
                "{field} must keep a blocker: {:?}",
                plan[0].install_blockers
            );
            assert!(plan[0].dependencies.is_empty());
        }
    }

    #[tokio::test]
    async fn blocked_runtime_dependencies_still_record_edges() {
        let source = FixtureSource::new(vec![
            ("app", formula_json("app", "1.0.0", 0, &["lib"])),
            ("lib", formula_json("lib", "1.0.0", 0, &[])),
        ]);

        let plan = resolve_with(&source, &["app".into()], TAG)
            .await
            .expect("plan");

        let names: Vec<&str> = plan.iter().map(|formula| formula.name.as_str()).collect();
        assert_eq!(names, ["lib", "app"]);
        let app = plan.iter().find(|formula| formula.name == "app").unwrap();
        assert_eq!(app.dependencies, ["lib"]);
        assert!(
            app.install_blockers
                .iter()
                .any(|blocker| blocker.contains("relocation")),
            "a fixed-prefix bottle must stay blocked: {:?}",
            app.install_blockers
        );
    }

    #[tokio::test]
    async fn variation_extra_metadata_keeps_blocker() {
        let tool = serde_json::json!({
            "name": "tool",
            "tap": CORE_TAP,
            "versions": { "stable": "1.0.0", "bottle": true },
            "revision": 0,
            "dependencies": ["base"],
            "variations": {
                TAG: {
                    "dependencies": [],
                    "requirements": [{ "name": "xcode" }]
                }
            },
            "bottle": {
                "stable": {
                    "files": {
                        TAG: {
                            "cellar": ":any_skip_relocation",
                            "url": format!("https://ghcr.io/v2/homebrew/core/tool/blobs/sha256:{SHA}"),
                            "sha256": SHA,
                        }
                    }
                }
            }
        })
        .to_string();
        let source = FixtureSource::new(vec![
            ("tool", tool),
            ("base", portable_formula_json("base", "1.0.0", &[])),
        ]);

        let plan = resolve_with(&source, &["tool".into()], TAG)
            .await
            .expect("plan");

        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].name, "tool");
        assert!(plan[0].dependencies.is_empty());
        assert!(
            plan[0]
                .install_blockers
                .iter()
                .any(|blocker| blocker.contains("platform-specific metadata")),
            "unrecognized variation fields must stay blocked: {:?}",
            plan[0].install_blockers
        );
    }

    #[tokio::test]
    async fn explicit_empty_variation_removes_dependencies() {
        let tool = serde_json::json!({
            "name": "tool",
            "tap": CORE_TAP,
            "versions": { "stable": "1.0.0", "bottle": true },
            "revision": 0,
            "dependencies": ["base"],
            "variations": {
                TAG: { "dependencies": [] }
            },
            "bottle": {
                "stable": {
                    "files": {
                        TAG: {
                            "cellar": ":any",
                            "url": format!("https://ghcr.io/v2/homebrew/core/tool/blobs/sha256:{SHA}"),
                            "sha256": SHA,
                        }
                    }
                }
            }
        })
        .to_string();
        let source = FixtureSource::new(vec![
            ("tool", tool),
            ("base", formula_json("base", "1.0.0", 0, &[])),
        ]);

        let plan = resolve_with(&source, &["tool".into()], TAG)
            .await
            .expect("plan");
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].name, "tool");
    }

    #[tokio::test]
    async fn accepts_universal_all_bottle() {
        let doc = serde_json::json!({
            "name": "portable",
            "tap": CORE_TAP,
            "versions": { "stable": "1.0.0", "bottle": true },
            "revision": 0,
            "dependencies": [],
            "bottle": {
                "stable": {
                    "files": {
                        "all": {
                            "cellar": ":any_skip_relocation",
                            "url": format!("https://ghcr.io/v2/homebrew/core/portable/blobs/sha256:{SHA}"),
                            "sha256": SHA,
                        }
                    }
                }
            }
        })
        .to_string();
        let source = FixtureSource::new(vec![("portable", doc)]);

        let plan = resolve_with(&source, &["portable".into()], TAG)
            .await
            .expect("plan");
        assert_eq!(plan[0].cellar, ":any_skip_relocation");
    }

    #[tokio::test]
    async fn rejects_dependency_cycles() {
        let source = FixtureSource::new(vec![
            ("a", formula_json("a", "1.0.0", 0, &["b"])),
            ("b", formula_json("b", "1.0.0", 0, &["a"])),
        ]);

        let error = resolve_with(&source, &["a".into()], TAG)
            .await
            .expect_err("cycle must fail");
        assert!(format!("{error:#}").contains("cycle"), "{error:#}");
    }

    #[tokio::test]
    async fn rejects_self_dependency() {
        let source = FixtureSource::new(vec![("a", formula_json("a", "1.0.0", 0, &["a"]))]);

        let error = resolve_with(&source, &["a".into()], TAG)
            .await
            .expect_err("self cycle must fail");
        assert!(format!("{error:#}").contains("cycle"), "{error:#}");
    }

    #[tokio::test]
    async fn rejects_invalid_targets_before_fetching() {
        let source = FixtureSource::new(vec![]);
        for target in [
            "",
            "../ripgrep",
            "Ripgrep",
            "rip grep",
            "homebrew/cask/firefox",
            "other/repo/foo",
            "homebrew/core/../x",
            "homebrew/core/",
        ] {
            assert!(
                resolve_with(&source, &[target.to_string()], TAG)
                    .await
                    .is_err(),
                "target '{target}' must be rejected"
            );
        }
    }

    #[tokio::test]
    async fn rejects_invalid_tags() {
        let source = FixtureSource::new(vec![]);
        for tag in ["", "Arm64_Sequoia", "arm64-sequoia", "arm64/sequoia"] {
            assert!(
                resolve_with(&source, &[], tag).await.is_err(),
                "tag '{tag}' must be rejected"
            );
        }
    }

    #[tokio::test]
    async fn rejects_unsupported_taps() {
        let mut doc: serde_json::Value =
            serde_json::from_str(&formula_json("tool", "1.0.0", 0, &[])).unwrap();
        doc["tap"] = serde_json::json!("someone/else");
        let source = FixtureSource::new(vec![("tool", doc.to_string())]);

        let error = resolve_with(&source, &["tool".into()], TAG)
            .await
            .expect_err("tap must be rejected");
        assert!(
            format!("{error:#}").contains("unsupported tap"),
            "{error:#}"
        );
    }

    #[tokio::test]
    async fn rejects_source_only_disabled_and_missing_bottle() {
        let mut source_only: serde_json::Value =
            serde_json::from_str(&formula_json("tool", "1.0.0", 0, &[])).unwrap();
        source_only["versions"]["bottle"] = serde_json::json!(false);
        source_only.as_object_mut().unwrap().remove("bottle");
        let source = FixtureSource::new(vec![("tool", source_only.to_string())]);
        let error = resolve_with(&source, &["tool".into()], TAG)
            .await
            .expect_err("source-only must fail");
        assert!(
            format!("{error:#}").contains("no bottled stable"),
            "{error:#}"
        );

        let mut disabled: serde_json::Value =
            serde_json::from_str(&formula_json("tool", "1.0.0", 0, &[])).unwrap();
        disabled["disabled"] = serde_json::json!(true);
        let source = FixtureSource::new(vec![("tool", disabled.to_string())]);
        let error = resolve_with(&source, &["tool".into()], TAG)
            .await
            .expect_err("disabled must fail");
        assert!(format!("{error:#}").contains("disabled"), "{error:#}");

        let source = FixtureSource::new(vec![("tool", formula_json("tool", "1.0.0", 0, &[]))]);
        let error = resolve_with(&source, &["tool".into()], "x86_64_linux")
            .await
            .expect_err("missing tag must fail");
        let message = format!("{error:#}");
        assert!(message.contains("no bottle for host tag"), "{message}");
        assert!(message.contains("available: arm64_sequoia"), "{message}");
    }

    #[tokio::test]
    async fn rejects_malformed_metadata() {
        type Mutation = Box<dyn Fn(&mut serde_json::Value)>;

        let cases: Vec<(&str, Mutation)> = vec![
            (
                "bad sha",
                Box::new(|doc| {
                    doc["bottle"]["stable"]["files"][TAG]["sha256"] = serde_json::json!("abc");
                }),
            ),
            (
                "http url",
                Box::new(|doc| {
                    doc["bottle"]["stable"]["files"][TAG]["url"] =
                        serde_json::json!("http://ghcr.io/x");
                }),
            ),
            (
                "wrong host",
                Box::new(|doc| {
                    doc["bottle"]["stable"]["files"][TAG]["url"] = serde_json::json!(
                        "https://evil.example/v2/homebrew/core/tool/blobs/sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                    );
                }),
            ),
            (
                "digest mismatch",
                Box::new(|doc| {
                    doc["bottle"]["stable"]["files"][TAG]["url"] = serde_json::json!(
                        "https://ghcr.io/v2/homebrew/core/tool/blobs/sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
                    );
                }),
            ),
            (
                "non-core bottle path",
                Box::new(|doc| {
                    doc["bottle"]["stable"]["files"][TAG]["url"] = serde_json::json!(
                        "https://ghcr.io/v2/homebrew/other/tool/blobs/sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                    );
                }),
            ),
            (
                "bottle path traversal",
                Box::new(|doc| {
                    doc["bottle"]["stable"]["files"][TAG]["url"] = serde_json::json!(
                        "https://ghcr.io/v2/homebrew/core/../tool/blobs/sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                    );
                }),
            ),
            (
                "unsafe cellar",
                Box::new(|doc| {
                    doc["bottle"]["stable"]["files"][TAG]["cellar"] =
                        serde_json::json!("/opt/homebrew/../../../etc");
                }),
            ),
            (
                "formula name traversal",
                Box::new(|doc| {
                    doc["name"] = serde_json::json!("../evil");
                }),
            ),
            (
                "dependency traversal",
                Box::new(|doc| {
                    doc["dependencies"] = serde_json::json!(["../evil"]);
                }),
            ),
        ];

        for (label, mutate) in cases {
            let mut doc: serde_json::Value =
                serde_json::from_str(&formula_json("tool", "1.0.0", 0, &[])).unwrap();
            mutate(&mut doc);
            let source = FixtureSource::new(vec![("tool", doc.to_string())]);
            assert!(
                resolve_with(&source, &["tool".into()], TAG).await.is_err(),
                "case '{label}' must be rejected"
            );
        }
    }

    #[tokio::test]
    async fn no_targets_produces_an_empty_plan() {
        let source = FixtureSource::new(vec![]);
        let plan = resolve_with(&source, &[], TAG).await.expect("plan");
        assert!(plan.is_empty());
    }
}
