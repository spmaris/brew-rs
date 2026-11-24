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
//!   canonical name. Build, test, optional and recommended dependencies are
//!   intentionally not expanded (see the limitations below).
//!
//! The API payload is untrusted data. Every name, version, tag, URL, digest and
//! cellar value is validated before it reaches a [`PlannedFormula`], and the
//! graph is bounded. The digest is only recorded here; it must be verified
//! against the downloaded bytes by the download stage.
//!
//! Known limitations, all consequences of using the static API without running
//! Homebrew's Ruby:
//!
//! * Alias and old formula names fall back to the cached formula catalog
//!   when `formula/<name>.json` returns HTTP 404 (except during read-only
//!   `migrate --check` audits, which never populate or mutate a catalog
//!   cache). Requests and dependencies that resolve to the same canonical
//!   `name` are deduplicated.
//! * `uses_from_macos` is understood in three shapes on supported macOS tags:
//!   1. a plain (untagged) runtime library name from [`SYSTEM_SUPPLIED_LIBRARIES`]
//!      whose bounds entry is `{}` or `{ "since": "<known_codename>" }` with
//!      host macOS release `>=` the `since` release;
//!   2. a `no_linkage` entry (`{ "<formula>": "no_linkage" }` or `["no_linkage"]`)
//!      whose formula is in [`SYSTEM_SUPPLIED_LIBRARIES`] with satisfied bounds;
//!   3. a build- and/or test-only entry (`{ "<formula>": "build" }`, `"test"`,
//!      or `["build", "test"]`) with empty or recognized macOS `since`/`before`
//!      bounds, because precompiled bottles do not execute build or test phases.
//!
//!   Every other runtime name, non-build/test tag, `before` bound or unsatisfied
//!   `since` bound, mismatched bounds array, unknown or non-macOS platform, and
//!   unrecognized JSON shape keeps an install blocker.
//! * `requirements` supports `macos` entries (numeric dotted minimum evaluated
//!   against the host bottle tag or `version: null` for any supported macOS
//!   release), `arch` entries (`"arm64"` or `"x86_64"` matched against the host
//!   tag), and build- and/or test-only requirements (`contexts` containing only
//!   `"build"`/`"test"`). Other requirement names, non-build/test contexts,
//!   cask/download requests, additional fields, unusual spec lists, unparseable
//!   versions, unknown host releases and oversize arrays keep an install blocker.
//! * A `service` declaration is metadata only: installing a formula never runs
//!   `brew services`, so this crate never starts a service. Recognized
//!   optional `service` blocks on supported macOS tags do not block
//!   installation (`has_service` is recorded so `install` can note it). Invalid
//!   or unmodelled service blocks, any service on a non-macOS host, and any
//!   `pour_bottle_only_if` value stay blocked.
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
use serde_json::Value;

use crate::api::catalog_cache;
use crate::api::http_client::{HttpClient, is_not_found};
use crate::config::Config;

use super::not_found;
use super::store::PackageSource;

/// Official Homebrew formula API base.
pub(super) const API_BASE: &str = "https://formulae.brew.sh/api";
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
/// Maximum install requirements read from a single formula.
const MAX_REQUIREMENTS_PER_FORMULA: usize = 8;
/// Maximum contexts an accepted macOS requirement may list (only none today).
const MAX_REQUIREMENT_CONTEXTS: usize = 8;
/// Maximum specs an accepted macOS requirement may list.
const MAX_REQUIREMENT_SPECS: usize = 2;
/// Maximum length of a macOS minimum version string.
const MAX_REQUIREMENT_VERSION_LEN: usize = 16;
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

/// A parsed macOS release, compared as a dotted triple.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct MacosVersion {
    major: u32,
    minor: u32,
    patch: u32,
}

/// Known Homebrew macOS bottle codenames and the release they name, oldest
/// first.
///
/// A tag whose codename is missing here counts as an unsupported platform, so a
/// new macOS release is refused until its system libraries and version have
/// been checked. The version feeds only minimum-requirement comparisons.
const MACOS_RELEASES: [(&str, u32, u32); 12] = [
    ("el_capitan", 10, 11),
    ("sierra", 10, 12),
    ("high_sierra", 10, 13),
    ("mojave", 10, 14),
    ("catalina", 10, 15),
    ("big_sur", 11, 0),
    ("monterey", 12, 0),
    ("ventura", 13, 0),
    ("sonoma", 14, 0),
    ("sequoia", 15, 0),
    ("tahoe", 26, 0),
    ("golden_gate", 27, 0),
];

/// `uses_from_macos` libraries verified against macOS system libraries linked by
/// official Homebrew bottles or macOS-supplied system tools/runtimes.
const SYSTEM_SUPPLIED_LIBRARIES: [&str; 14] = [
    "bzip2", "cups", "curl", "expat", "krb5", "libedit", "libffi", "libiconv", "libxml2",
    "libxslt", "ncurses", "openldap", "python", "zlib",
];

/// Blocker for a `uses_from_macos` declaration whose JSON shape is not modelled.
const BLOCKER_USES_FROM_MACOS_SHAPE: &str =
    "declares a uses_from_macos metadata shape not supported by this native installer";

/// Blocker for a `uses_from_macos` library that is not a verified macOS one.
const BLOCKER_USES_FROM_MACOS_DEPENDENCY: &str =
    "declares a uses_from_macos dependency that is not a verified macOS system library";

/// Blocker for tagged or version-bounded `uses_from_macos` declarations.
const BLOCKER_USES_FROM_MACOS_SCOPING: &str = "declares conditional or build-scoped uses_from_macos dependencies not supported by this native installer";

/// Blocker for `uses_from_macos` on a platform where macOS cannot supply it.
const BLOCKER_USES_FROM_MACOS_PLATFORM: &str =
    "declares uses_from_macos dependencies on a platform where macOS does not supply them";

/// Blocker for default-installed recommended dependencies.
const BLOCKER_RECOMMENDED_DEPENDENCY: &str =
    "declares recommended dependencies this native installer does not install";

/// Blocker for a service declaration that is not the recognized optional one.
const BLOCKER_SERVICE: &str =
    "declares a service declaration this native installer does not recognize as optional";

/// Blocker for a pour-bottle-only condition.
const BLOCKER_POUR_BOTTLE_ONLY: &str =
    "declares pour-bottle-only conditions not supported by this native installer";

/// Blocker for install requirements outside the modelled macOS minimum shape.
const BLOCKER_REQUIREMENTS_SHAPE: &str =
    "declares additional install requirements not supported by this native installer";

/// Blocker for macOS requirements declared on a non-macOS host.
const BLOCKER_REQUIREMENTS_PLATFORM: &str =
    "declares macOS install requirements on a host platform macOS does not serve";

/// Blocker for a macOS minimum newer than the host bottle tag's release.
const BLOCKER_REQUIREMENTS_VERSION: &str =
    "requires a newer macOS release than the host bottle tag";

/// Blocker for an architecture requirement that disagrees with the host bottle tag.
const BLOCKER_REQUIREMENTS_ARCH: &str =
    "requires a different CPU architecture than the host bottle tag";

/// The `--source` choice of `install`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum SourceFlag {
    /// Resolve through Homebrew (the default).
    Homebrew,
    /// Install the reviewed official upstream build from the bundled catalog.
    Upstream,
}

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
    pub keg_only: bool,
    pub has_service: bool,
    pub caveats: Option<String>,
    pub conflicts_with: Vec<String>,
    pub requested: bool,
    pub dependencies: Vec<String>,
    pub url: String,
    pub sha256: String,
    pub cellar: String,
    pub install_blockers: Vec<&'static str>,
    /// Where the artifact comes from; routing never inspects `cellar` for this.
    pub source: PackageSource,
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
        alias_config: Some(config.clone()),
    };
    match resolve_with(&source, targets, tag).await {
        Ok(plan) => Ok(plan),
        Err(error) => Err(not_found::add_hints_online(error, config).await),
    }
}

/// A read-only audit reuses metadata across independently resolved formulae.
/// The cache is scoped to one audit; readiness never writes a metadata cache.
pub(super) struct AuditPlanner {
    source: Memoized<HttpFormulaSource>,
}

impl AuditPlanner {
    pub(super) fn new(config: &Config) -> Result<Self> {
        Ok(Self {
            source: Memoized::new(HttpFormulaSource {
                http: HttpClient::new(config)?,
                alias_config: None,
            }),
        })
    }

    pub(super) async fn resolve(&self, name: &str, tag: &str) -> Result<Vec<PlannedFormula>> {
        resolve_with(&self.source, &[name.into()], tag).await
    }
}

const MAX_AUDIT_REQUESTS: usize = 512;

struct Memoized<S> {
    source: S,
    cached: std::cell::RefCell<HashMap<String, std::result::Result<FormulaResponse, String>>>,
}

impl<S> Memoized<S> {
    fn new(source: S) -> Self {
        Self {
            source,
            cached: Default::default(),
        }
    }
}

impl<S: FormulaSource> FormulaSource for Memoized<S> {
    async fn fetch(&self, name: &str) -> Result<FormulaResponse> {
        if let Some(cached) = self.cached.borrow().get(name).cloned() {
            return cached.map_err(anyhow::Error::msg);
        }
        if self.cached.borrow().len() >= MAX_AUDIT_REQUESTS {
            bail!("Migration audit exceeded the {MAX_AUDIT_REQUESTS}-request metadata budget");
        }
        let answer = self.source.fetch(name).await.map_err(|error| {
            if is_not_found(&error) {
                format!(
                    "No current official formula record for '{name}' (HTTP 404). It may have been renamed or removed; this audit does not resolve aliases."
                )
            } else {
                format!("{error:#}")
            }
        });
        self.cached.borrow_mut().insert(name.into(), answer.clone());
        answer.map_err(anyhow::Error::msg)
    }
}

/// A source of formula metadata, kept as a seam for offline fixture tests.
trait FormulaSource {
    async fn fetch(&self, name: &str) -> Result<FormulaResponse>;
}

/// [`FormulaSource`] backed by the official API over [`HttpClient`].
struct HttpFormulaSource {
    http: HttpClient,
    alias_config: Option<Config>,
}

impl FormulaSource for HttpFormulaSource {
    async fn fetch(&self, name: &str) -> Result<FormulaResponse> {
        let url = format!("{API_BASE}/formula/{name}.json");
        match self.http.fetch_json(&url).await {
            Ok(payload) => Ok(payload),
            Err(error) => {
                if is_not_found(&error)
                    && let Some(config) = &self.alias_config
                    && let Ok(Some(canonical)) =
                        catalog_cache::resolve_formula_alias(config, name).await
                    && validate_formula_name(&canonical, "alias target").is_ok()
                {
                    let alias_url = format!("{API_BASE}/formula/{canonical}.json");
                    return self.http.fetch_json(&alias_url).await.with_context(|| {
                        format!(
                            "failed to fetch formula '{canonical}' (resolved from alias '{name}')"
                        )
                    });
                }
                log::debug!("GET {url} failed: {error:#}");
                Err(error).with_context(|| format!("failed to fetch formula '{name}'"))
            }
        }
    }
}

/// Service metadata for an installed formula, pinned to its receipt.
///
/// [`fetch_installed_service`] only returns after the official payload has been
/// proven to describe exactly the installed artifact, so a preview can never
/// render a declaration that belongs to another version or bottle.
pub(super) struct InstalledServiceMetadata {
    pub name: String,
    pub version: String,
    pub service: serde_json::Value,
}

/// Fetch official metadata for `name` and require it to describe exactly the
/// installed receipt.
///
/// This is a read-only metadata query. It deliberately bypasses alias
/// resolution and the catalog cache, so rendering a preview never populates or
/// mutates a cache. The tap, stable version (including the `_<revision>`
/// suffix), version scheme, and selected bottle digest must all equal the
/// installed values. Bottle selection follows the planner: the host tag first,
/// then Homebrew's platform-independent `all` bottle. A service declaration is
/// returned only when the existing optional-macOS-service review accepts it.
pub(super) async fn fetch_installed_service(
    config: &Config,
    name: &str,
    tag: &str,
    installed_version: &str,
    installed_version_scheme: u32,
    installed_sha256: &str,
) -> Result<InstalledServiceMetadata> {
    validate_tag(tag)?;
    let name = validate_formula_name(name, "service formula name")?;
    let http = HttpClient::new(config)?;
    let url = format!("{API_BASE}/formula/{name}.json");
    let response: FormulaResponse = http
        .fetch_json(&url)
        .await
        .with_context(|| format!("failed to fetch formula '{name}' from {url}"))?;

    let canonical = validate_formula_name(&response.name, "formula name")?;
    if canonical != name {
        bail!(
            "official metadata for '{name}' returned formula '{canonical}'; refusing to preview a different formula"
        );
    }
    if response.tap != CORE_TAP {
        bail!(
            "formula '{name}' is served by tap '{}'; only {CORE_TAP} metadata is supported",
            response.tap
        );
    }
    if response.disabled {
        bail!("formula '{name}' is disabled upstream");
    }
    if !response.versions.bottle {
        bail!("formula '{name}' has no bottled stable version");
    }
    let stable = response
        .versions
        .stable
        .as_deref()
        .with_context(|| format!("formula '{name}' has no stable version"))?;
    if stable.is_empty() {
        bail!("formula '{name}' has an empty stable version");
    }
    let revision = response.revision.unwrap_or(0);
    let version = if revision == 0 {
        stable.to_string()
    } else {
        format!("{stable}_{revision}")
    };
    validate_version(&version, name)?;
    if version != installed_version {
        bail!(
            "installed receipt pins {name} {installed_version}, but official metadata reports {version}; refusing to preview a different version"
        );
    }
    if response.version_scheme != Some(installed_version_scheme) {
        bail!(
            "installed receipt pins {name} version scheme {installed_version_scheme}, but official metadata reports {:?}; refusing to preview a different version",
            response.version_scheme
        );
    }
    let bottle = response
        .bottle
        .as_ref()
        .and_then(|bottle| bottle.stable.as_ref())
        .with_context(|| format!("formula '{name}' has no stable bottle metadata"))?;
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
    let sha256 = validate_sha256(&bottle_file.sha256, name)?;
    if !sha256.eq_ignore_ascii_case(installed_sha256) {
        bail!(
            "installed receipt pins {name} bottle {installed_sha256}, but official metadata for tag '{tag}' reports {sha256}; refusing to preview a different artifact"
        );
    }
    if response.service.is_null() {
        bail!("formula '{name}' does not declare a service");
    }
    if review_service(&response.service, tag).is_some() {
        bail!(
            "formula '{name}' declares a service declaration this native installer does not recognize as optional"
        );
    }
    Ok(InstalledServiceMetadata {
        name: name.to_string(),
        version,
        service: response.service,
    })
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
    // Tap qualifier of each tap-qualified target, and the formula that listed
    // each dependency, for error messages.
    let mut target_taps: HashMap<String, String> = HashMap::new();
    let mut dependency_parents: HashMap<String, String> = HashMap::new();
    for target in targets {
        let name = parse_target(target)?.to_string();
        if let Some((tap, _)) = target.rsplit_once('/') {
            target_taps.insert(name.clone(), tap.to_string());
        }
        requested_names.push(name);
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

        let response = match source.fetch(&requested).await {
            Ok(response) => response,
            Err(error) => {
                return Err(not_found::from_fetch_error(
                    error,
                    &requested,
                    target_taps.get(&requested).map(String::as_str),
                    dependency_parents.get(&requested).map(String::as_str),
                ));
            }
        };
        let (canonical, node) = build_node(response, tag)?;
        if let Some(existing) = canonical_for.get(&canonical)
            && existing != &canonical
        {
            bail!(
                "inconsistent formula metadata: '{canonical}' was already resolved as '{existing}'"
            );
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
                    dependency_parents.insert(dependency.clone(), canonical.clone());
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

    // Portable bottles and the validated macOS relocation lane can be staged.
    // Keep other fixed-prefix and relocation formats available to plan/fetch,
    // but ineligible for installation.
    let mut install_blockers = Vec::new();
    if response.version_scheme.is_none() {
        install_blockers.push("does not declare a Homebrew version scheme");
    }
    match cellar.as_str() {
        ":any_skip_relocation" => {}
        ":any" if is_supported_macos_tag(tag) => {}
        ":any" => install_blockers
            .push("requires bottle relocation not yet supported for this formula or platform"),
        _ => install_blockers.push("requires a fixed Homebrew prefix"),
    }
    if !response.recommended_dependencies.is_empty() {
        install_blockers.push(BLOCKER_RECOMMENDED_DEPENDENCY);
    }
    if let Some(blocker) = review_uses_from_macos(&response, tag) {
        install_blockers.push(blocker);
    }
    if response.post_install_defined != Some(false) {
        install_blockers.push("has a post-install hook or does not declare hook status");
    }
    let keg_only = match response.keg_only {
        Some(flag) => flag,
        None => {
            install_blockers.push("does not declare keg-only status");
            false
        }
    };
    let conflicts_with = validate_conflicts(&name, &response.conflicts_with, &mut install_blockers);
    let has_service = !response.service.is_null();
    if let Some(blocker) = review_service(&response.service, tag) {
        install_blockers.push(blocker);
    }
    if !response.pour_bottle_only_if.is_null() {
        install_blockers.push(BLOCKER_POUR_BOTTLE_ONLY);
    }
    if let Some(blocker) = review_requirements(&response.requirements, tag) {
        install_blockers.push(blocker);
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
            keg_only,
            has_service,
            caveats: response.caveats.clone(),
            conflicts_with,
            dependencies,
            url,
            sha256,
            cellar,
            install_blockers,
        },
    ))
}

fn validate_conflicts(
    formula_name: &str,
    raw: &[String],
    install_blockers: &mut Vec<&'static str>,
) -> Vec<String> {
    if raw.len() > MAX_DEPS_PER_FORMULA {
        install_blockers.push("declares an unsupported conflict rule");
        return Vec::new();
    }
    let mut validated = Vec::with_capacity(raw.len());
    let mut seen = HashSet::new();
    for conflict in raw {
        let Ok(name) = validate_formula_name(conflict, "conflict formula name") else {
            install_blockers.push("declares an unsupported conflict rule");
            return Vec::new();
        };
        if name == formula_name {
            install_blockers.push("declares an unsupported conflict rule");
            return Vec::new();
        }
        if seen.insert(name) {
            validated.push(name.to_string());
        }
    }
    validated.sort_unstable();
    validated
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
                        keg_only: node.keg_only,
                        has_service: node.has_service,
                        caveats: node.caveats.clone(),
                        conflicts_with: node.conflicts_with.clone(),
                        requested: requested.contains(&name),
                        dependencies,
                        url: node.url.clone(),
                        sha256: node.sha256.clone(),
                        cellar: node.cellar.clone(),
                        install_blockers: node.install_blockers.clone(),
                        source: PackageSource::Homebrew,
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
    keg_only: bool,
    has_service: bool,
    caveats: Option<String>,
    conflicts_with: Vec<String>,
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

/// Known keys in an optional Homebrew `service` declaration API payload.
const KNOWN_SERVICE_KEYS: [&str; 18] = [
    "run",
    "run_type",
    "interval",
    "cron",
    "keep_alive",
    "launch_only_once",
    "require_root",
    "environment_variables",
    "working_dir",
    "root_dir",
    "input_path",
    "log_path",
    "error_log_path",
    "macos_legacy_timers",
    "process_type",
    "sockets",
    "name",
    "restart_delay",
];

/// Exact keys of a Homebrew formula `requirements` entry.
const MACOS_REQUIREMENT_KEYS: [&str; 6] =
    ["name", "cask", "download", "version", "contexts", "specs"];

/// Review a formula's optional `service` declaration against host tag `tag`.
///
/// A service block is metadata, not an install step: Homebrew starts a service
/// only when the user explicitly asks (`brew services start`), never because the
/// package was installed. This planner carries no service state and starts
/// nothing. Valid Homebrew service blocks on supported macOS tags do not block
/// installation; Linux targets and unrecognized shapes keep [`BLOCKER_SERVICE`].
fn review_service(service: &Value, tag: &str) -> Option<&'static str> {
    if service.is_null() {
        return None;
    }
    if is_valid_optional_macos_service(service, tag) {
        None
    } else {
        Some(BLOCKER_SERVICE)
    }
}

fn is_valid_optional_macos_service(service: &Value, tag: &str) -> bool {
    if !is_supported_macos_tag(tag) {
        return false;
    }
    let Some(service) = service.as_object() else {
        return false;
    };
    if service.is_empty()
        || service.len() > KNOWN_SERVICE_KEYS.len()
        || !service
            .keys()
            .all(|key| KNOWN_SERVICE_KEYS.contains(&key.as_str()))
    {
        return false;
    }
    if !service.contains_key("run") && !service.contains_key("name") {
        return false;
    }
    if let Some(run) = service.get("run")
        && !is_valid_service_run(run)
    {
        return false;
    }
    true
}

fn is_valid_service_run(run: &Value) -> bool {
    match run {
        Value::String(command) => !command.is_empty(),
        Value::Array(args) => is_valid_service_run_array(args),
        Value::Object(os_map) => {
            !os_map.is_empty()
                && os_map.len() <= 4
                && os_map.iter().all(|(os, value)| {
                    matches!(os.as_str(), "macos" | "linux")
                        && match value {
                            Value::String(command) => !command.is_empty(),
                            Value::Array(args) => is_valid_service_run_array(args),
                            _ => false,
                        }
                })
        }
        _ => false,
    }
}

fn is_valid_service_run_array(args: &[Value]) -> bool {
    !args.is_empty()
        && args.len() <= 64
        && args
            .iter()
            .all(|value| value.as_str().is_some_and(|arg| !arg.is_empty()))
}

/// CPU architecture named by a Homebrew bottle tag.
pub(super) fn arch_for_tag(tag: &str) -> Option<&'static str> {
    if tag.starts_with("arm64_") {
        Some("arm64")
    } else if tag.starts_with("x86_64_") || macos_version_for_tag(tag).is_some() {
        Some("x86_64")
    } else {
        None
    }
}

/// Review a formula's `requirements` array against host tag `tag`.
fn review_requirements(requirements: &[Value], tag: &str) -> Option<&'static str> {
    if requirements.is_empty() {
        return None;
    }
    if requirements.len() > MAX_REQUIREMENTS_PER_FORMULA {
        return Some(BLOCKER_REQUIREMENTS_SHAPE);
    }
    for requirement in requirements {
        if let Some(blocker) = review_requirement(requirement, tag) {
            return Some(blocker);
        }
    }
    None
}

/// Review one requirement entry against host tag `tag`.
fn review_requirement(requirement: &Value, tag: &str) -> Option<&'static str> {
    let Some(requirement) = requirement.as_object() else {
        return Some(BLOCKER_REQUIREMENTS_SHAPE);
    };
    if requirement.len() != MACOS_REQUIREMENT_KEYS.len()
        || !MACOS_REQUIREMENT_KEYS
            .iter()
            .all(|key| requirement.contains_key(*key))
    {
        return Some(BLOCKER_REQUIREMENTS_SHAPE);
    }
    let Some(name) = requirement.get("name").and_then(Value::as_str) else {
        return Some(BLOCKER_REQUIREMENTS_SHAPE);
    };
    if !requirement.get("cask").is_some_and(Value::is_null)
        || !requirement.get("download").is_some_and(Value::is_null)
    {
        return Some(BLOCKER_REQUIREMENTS_SHAPE);
    }

    let Some(specs) = requirement.get("specs").and_then(Value::as_array) else {
        return Some(BLOCKER_REQUIREMENTS_SHAPE);
    };
    if specs.len() > MAX_REQUIREMENT_SPECS {
        return Some(BLOCKER_REQUIREMENTS_SHAPE);
    }
    let specs_are_stable = match specs.as_slice() {
        [stable] => stable.as_str() == Some("stable"),
        [stable, head] => stable.as_str() == Some("stable") && head.as_str() == Some("head"),
        _ => false,
    };
    if !specs_are_stable {
        return Some(BLOCKER_REQUIREMENTS_SHAPE);
    }

    let Some(contexts) = requirement.get("contexts").and_then(Value::as_array) else {
        return Some(BLOCKER_REQUIREMENTS_SHAPE);
    };
    if contexts.len() > MAX_REQUIREMENT_CONTEXTS {
        return Some(BLOCKER_REQUIREMENTS_SHAPE);
    }
    if !contexts.is_empty() {
        let build_or_test_only = contexts.len() <= 2
            && contexts
                .iter()
                .all(|context| matches!(context.as_str(), Some("build" | "test")));
        if build_or_test_only && validate_formula_name(name, "requirement name").is_ok() {
            return None;
        }
        return Some(BLOCKER_REQUIREMENTS_SHAPE);
    }

    match name {
        "macos" => {
            let required = match requirement.get("version") {
                Some(Value::Null) => None,
                Some(Value::String(version)) => {
                    let Some(parsed) = parse_macos_version(version) else {
                        return Some(BLOCKER_REQUIREMENTS_SHAPE);
                    };
                    Some(parsed)
                }
                _ => return Some(BLOCKER_REQUIREMENTS_SHAPE),
            };
            let Some(host) = macos_version_for_tag(tag) else {
                return Some(BLOCKER_REQUIREMENTS_PLATFORM);
            };
            if let Some(required) = required
                && host < required
            {
                return Some(BLOCKER_REQUIREMENTS_VERSION);
            }
            None
        }
        "arch" => {
            let Some(required_arch) = requirement.get("version").and_then(Value::as_str) else {
                return Some(BLOCKER_REQUIREMENTS_SHAPE);
            };
            if !matches!(required_arch, "arm64" | "x86_64") {
                return Some(BLOCKER_REQUIREMENTS_SHAPE);
            }
            let Some(host_arch) = arch_for_tag(tag) else {
                return Some(BLOCKER_REQUIREMENTS_PLATFORM);
            };
            if host_arch != required_arch {
                return Some(BLOCKER_REQUIREMENTS_ARCH);
            }
            None
        }
        _ => Some(BLOCKER_REQUIREMENTS_SHAPE),
    }
}

/// Parse a numeric macOS version (`12`, `10.15`, `14.2.1`) into a dotted triple.
///
/// Only one to three non-empty numeric components of at most three digits are
/// accepted, leading zeros are refused, and a major version below 10 is treated
/// as unknown. A false return keeps the requirement blocked.
pub(super) fn parse_macos_version(value: &str) -> Option<MacosVersion> {
    if value.is_empty() || value.len() > MAX_REQUIREMENT_VERSION_LEN {
        return None;
    }
    let mut components = [0_u32; 3];
    let mut count = 0;
    for component in value.split('.') {
        if count == components.len()
            || component.is_empty()
            || component.len() > 3
            || !component.bytes().all(|byte| byte.is_ascii_digit())
            || (component.len() > 1 && component.starts_with('0'))
        {
            return None;
        }
        components[count] = component.parse().ok()?;
        count += 1;
    }
    if count == 0 || components[0] < 10 {
        return None;
    }
    Some(MacosVersion {
        major: components[0],
        minor: components[1],
        patch: components[2],
    })
}

/// Review a formula's `uses_from_macos` metadata for host tag `tag`.
///
/// Accepts plain untagged entries from [`SYSTEM_SUPPLIED_LIBRARIES`] when their
/// bounds entry is `{}` or `{ "since": "<known_codename>" }` with host release
/// `>=` that release, plus build/test-only entries validated by
/// [`is_build_or_test_scoped_uses_from_macos`]. On non-macOS hosts any non-empty
/// declaration keeps [`BLOCKER_USES_FROM_MACOS_PLATFORM`].
fn review_uses_from_macos(response: &FormulaResponse, tag: &str) -> Option<&'static str> {
    let declarations = &response.uses_from_macos;
    let bounds = &response.uses_from_macos_bounds;

    // The official API serialises exactly one bounds entry per declaration. A
    // payload whose arrays disagree, or that records bounds without
    // declarations, cannot be interpreted safely.
    if declarations.len() != bounds.len() {
        return Some(BLOCKER_USES_FROM_MACOS_SHAPE);
    }
    if declarations.is_empty() {
        return None;
    }

    let Some(host) = macos_version_for_tag(tag) else {
        return Some(BLOCKER_USES_FROM_MACOS_PLATFORM);
    };

    for (declaration, bounds) in declarations.iter().zip(bounds) {
        match declaration {
            serde_json::Value::String(name) => {
                if !SYSTEM_SUPPLIED_LIBRARIES.contains(&name.as_str()) {
                    return Some(BLOCKER_USES_FROM_MACOS_DEPENDENCY);
                }
                if let Some(blocker) = review_system_library_bounds(bounds, host) {
                    return Some(blocker);
                }
            }
            serde_json::Value::Object(map) => {
                if is_build_or_test_scoped_uses_from_macos(map, bounds) {
                    continue;
                }
                if is_no_linkage_uses_from_macos(map) {
                    let name = map.keys().next().expect("map.len() == 1");
                    if !SYSTEM_SUPPLIED_LIBRARIES.contains(&name.as_str()) {
                        return Some(BLOCKER_USES_FROM_MACOS_DEPENDENCY);
                    }
                    if let Some(blocker) = review_system_library_bounds(bounds, host) {
                        return Some(blocker);
                    }
                    continue;
                }
                let is_recognized_tagged_object = map.len() == 1
                    && map.iter().all(|(key, value)| {
                        validate_formula_name(key, "uses_from_macos dependency").is_ok()
                            && (value.is_string() || value.is_array())
                    });
                return Some(if is_recognized_tagged_object {
                    BLOCKER_USES_FROM_MACOS_SCOPING
                } else {
                    BLOCKER_USES_FROM_MACOS_SHAPE
                });
            }
            _ => return Some(BLOCKER_USES_FROM_MACOS_SHAPE),
        }
    }

    None
}

fn review_system_library_bounds(bounds: &Value, host: MacosVersion) -> Option<&'static str> {
    let Some(bounds) = bounds.as_object() else {
        return Some(BLOCKER_USES_FROM_MACOS_SHAPE);
    };
    if bounds.is_empty() {
        return None;
    }
    let satisfied = bounds.len() == 1
        && bounds
            .get("since")
            .and_then(Value::as_str)
            .and_then(macos_version_for_tag)
            .is_some_and(|since| host >= since);
    if satisfied {
        None
    } else {
        Some(BLOCKER_USES_FROM_MACOS_SCOPING)
    }
}

fn is_no_linkage_uses_from_macos(map: &serde_json::Map<String, Value>) -> bool {
    if map.len() != 1 {
        return false;
    }
    let Some((name, scope)) = map.iter().next() else {
        return false;
    };
    if validate_formula_name(name, "uses_from_macos dependency").is_err() {
        return false;
    }
    match scope {
        Value::String(tag) => tag == "no_linkage",
        Value::Array(tags) => tags.len() == 1 && tags[0].as_str() == Some("no_linkage"),
        _ => false,
    }
}

fn is_build_or_test_scoped_uses_from_macos(
    map: &serde_json::Map<String, Value>,
    bounds: &Value,
) -> bool {
    if map.len() != 1 {
        return false;
    }
    let Some((name, scope)) = map.iter().next() else {
        return false;
    };
    if validate_formula_name(name, "uses_from_macos dependency").is_err() {
        return false;
    }
    let is_build_or_test_tag = |tag: &Value| matches!(tag.as_str(), Some("build" | "test"));
    let scope_ok = match scope {
        Value::String(_) => is_build_or_test_tag(scope),
        Value::Array(tags) => {
            !tags.is_empty() && tags.len() <= 2 && tags.iter().all(is_build_or_test_tag)
        }
        _ => false,
    };
    if !scope_ok {
        return false;
    }
    let Some(bounds) = bounds.as_object() else {
        return false;
    };
    bounds.iter().all(|(key, value)| {
        matches!(key.as_str(), "since" | "before")
            && value.as_str().is_some_and(|codename| {
                MACOS_RELEASES
                    .iter()
                    .any(|(known, _, _)| *known == codename)
            })
    })
}

/// Parse either a numeric macOS version (`13`, `14.2`) or a known codename (`sequoia`).
pub(super) fn parse_macos_version_or_codename(value: &str) -> Option<MacosVersion> {
    parse_macos_version(value).or_else(|| {
        MACOS_RELEASES
            .iter()
            .find(|(name, _, _)| *name == value)
            .map(|(_, major, minor)| MacosVersion {
                major: *major,
                minor: *minor,
                patch: 0,
            })
    })
}

/// Homebrew uses `arm64_<codename>` on Apple Silicon and bare codenames on Intel.
/// Neither Linux tags nor unknown codenames provide this dependency guarantee.
pub(super) fn is_supported_macos_tag(tag: &str) -> bool {
    macos_version_for_tag(tag).is_some()
}

/// Map a macOS bottle tag to the release it names, if the release is known.
pub(super) fn macos_version_for_tag(tag: &str) -> Option<MacosVersion> {
    let codename = tag.strip_prefix("arm64_").unwrap_or(tag);
    MACOS_RELEASES
        .iter()
        .find(|(name, _, _)| *name == codename)
        .map(|(_, major, minor)| MacosVersion {
            major: *major,
            minor: *minor,
            patch: 0,
        })
}

#[cfg(test)]
#[test]
fn recognizes_actual_macos_bottle_tags() {
    assert!(is_supported_macos_tag("sequoia"));
    assert!(is_supported_macos_tag("arm64_sequoia"));
    assert!(!is_supported_macos_tag("x86_64_linux"));
    assert!(!is_supported_macos_tag("arm64_linux"));
}

/// The subset of the official formula payload used by the planner.
#[derive(Clone, Debug, Deserialize)]
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
    caveats: Option<String>,
    #[serde(default)]
    recommended_dependencies: Vec<String>,
    #[serde(default)]
    uses_from_macos: Vec<serde_json::Value>,
    #[serde(default)]
    uses_from_macos_bounds: Vec<serde_json::Value>,
    #[serde(default)]
    conflicts_with: Vec<String>,
    #[serde(default)]
    service: serde_json::Value,
    #[serde(default)]
    pour_bottle_only_if: serde_json::Value,
    #[serde(default)]
    requirements: Vec<serde_json::Value>,
}

#[derive(Clone, Debug, Deserialize, Default)]
struct FormulaVersions {
    #[serde(default)]
    stable: Option<String>,
    #[serde(default)]
    bottle: bool,
}

#[derive(Clone, Debug, Deserialize)]
struct Variation {
    /// `None` means the variation does not override dependencies; `Some(vec![])`
    /// means it explicitly removes them.
    #[serde(default)]
    dependencies: Option<Vec<String>>,
    #[serde(flatten)]
    extra: HashMap<String, serde_json::Value>,
}

#[derive(Clone, Debug, Deserialize)]
struct Bottle {
    #[serde(default)]
    stable: Option<BottleStable>,
}

#[derive(Clone, Debug, Deserialize)]
struct BottleStable {
    #[serde(default)]
    files: HashMap<String, BottleFile>,
}

#[derive(Clone, Debug, Deserialize)]
struct BottleFile {
    #[serde(default)]
    cellar: String,
    #[serde(default)]
    url: String,
    #[serde(default)]
    sha256: String,
}

#[cfg(test)]
mod tests;
