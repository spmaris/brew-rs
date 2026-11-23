//! Native dependency queries for `homebrew/core`.
//!
//! Forward queries read the official formula API
//! (`https://formulae.brew.sh/api/formula/<name>.json`) directly. They never run
//! `brew`, evaluate Ruby, write a file, or require a bottle for the host tag:
//! dependency metadata is available for formulae that are disabled, source
//! only, or not bottled for this machine.
//!
//! Runtime dependencies are the host-tag `variations[tag].dependencies` list
//! when that variation declares one, otherwise the formula's `dependencies`
//! list, exactly as the native planner selects them. Build, test, optional,
//! recommended and `uses_from_macos` entries are outside the scope of this
//! query; they are counted and, for the queried formula, named so their absence
//! from the graph is explicit rather than silent.
//!
//! Reverse queries are local-only. They read the selected brewrs prefix's
//! receipts, treat every recorded `(name, version, sha256)` pin as an edge from
//! a dependent to its dependency, and never touch the network or write
//! anything. A dependent is listed only when its pin names and matches the
//! installed dependency's exact version and digest.
//!
//! Both directions bound their work. The forward graph is fetched breadth
//! first, each formula at most once, stops descending at [`MAX_DEPTH`], and is
//! capped by [`MAX_FORMULAE`]. A formula reached a second time is marked
//! instead of being expanded again, and a branch that revisits an ancestor is
//! rendered as a cycle. Reverse traversal expands each receipt at most once and
//! marks repeated receipts and cycles, so even a hand-edited receipt graph
//! cannot loop.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::Value;

use crate::api::catalog_cache;
use crate::api::http_client::{HttpClient, is_not_found};
use crate::config::Config;
use crate::ui::DnfStyleFormatter;

use super::install::prefix_path;
use super::plan;
use super::store::{Dependency, Receipt, Store};

/// Official Homebrew formula API base.
const API_BASE: &str = "https://formulae.brew.sh/api";
/// Tap supported by this query, as in the native planner.
const CORE_TAP: &str = "homebrew/core";
/// Maximum dependency levels rendered below the queried formula.
const MAX_DEPTH: usize = 16;
/// Maximum distinct formulae read for one recursive forward query.
const MAX_FORMULAE: usize = 256;
/// Maximum runtime dependencies read from a single formula.
const MAX_DEPS_PER_FORMULA: usize = 256;
/// Maximum distinct names queued for a recursive forward query.
const MAX_REQUESTS: usize = 1024;
/// Excluded dependency names shown per category for the queried formula.
const MAX_EXCLUDED_NAMES_PER_KIND: usize = 8;
/// Digits of a digest rendered in a table.
const SHORT_DIGEST_LEN: usize = 12;
/// Maximum rendered version length.
const MAX_DISPLAY_VERSION_LEN: usize = 64;

/// Scope line for forward queries.
const SCOPE_RUNTIME: &str = "runtime dependencies only: the host-tag `variations` override when declared, otherwise `dependencies`\nbuild, test, optional, recommended and `uses_from_macos` entries are excluded; `*` marks a variation override";

/// Scope line for reverse queries.
const SCOPE_REVERSE: &str = "installed brewrs receipts in this prefix only, matched by exact (name, version, sha256) pins\nno network access and no writes";

/// Answer a native dependency query.
///
/// With `reverse` unset, `package` is resolved through the official formula API
/// and its runtime dependencies are printed; `tree` expands the graph
/// recursively instead of listing only direct dependencies. With `reverse` set,
/// `package` must name an installed brewrs package and the query lists the
/// receipts that pin it, directly or transitively; that direction is offline and
/// read-only. `prefix` selects the native store for reverse queries and is
/// ignored by forward queries.
///
/// # Errors
///
/// Returns an error for an invalid package identifier, an unsupported tap, a
/// formula the API cannot serve, malformed or unsafe metadata, an over-large
/// graph, a prefix that cannot be read, or a corrupt receipt.
pub(crate) async fn deps(
    config: &Config,
    prefix: Option<&Path>,
    package: &str,
    tree: bool,
    reverse: bool,
) -> Result<()> {
    if reverse {
        reverse_deps(config, prefix, package, tree)
    } else {
        forward_deps(config, package, tree).await
    }
}

/// Print the forward (formula API) dependency listing.
async fn forward_deps(config: &Config, package: &str, tree: bool) -> Result<()> {
    // Validate the caller's identifier before any network or platform probing.
    let target = plan::parse_target(package)?.to_string();
    let tag = super::default_target()?;
    let source = HttpFormulaSource {
        http: HttpClient::new(config)?,
        config: config.clone(),
    };
    let graph = build_forward_graph(&source, &target, &tag, tree).await?;
    let root = graph
        .nodes
        .get(&graph.root)
        .with_context(|| format!("internal dependency error: missing '{}'", graph.root))?;
    let formatter = DnfStyleFormatter::new(config);

    println!(
        "{}  {}",
        formatter.section("Native dependencies"),
        formatter.info(&graph.tag)
    );
    println!(
        "{}",
        formatter.field(
            "Formula",
            &name_and_version(&graph.root, root.version.as_deref())
        )
    );
    println!("{}", formatter.field("Scope", SCOPE_RUNTIME));
    println!(
        "{}",
        formatter.field(
            "Excluded",
            &if root.excluded.total() == 0 {
                "none declared by this formula".to_string()
            } else {
                format_excluded(root.excluded, &root.excluded_entries)
            }
        )
    );

    if !tree {
        if root.dependencies.is_empty() {
            println!("\nNo runtime dependencies.");
            return Ok(());
        }
        let rows: Vec<Vec<String>> = root
            .dependencies
            .iter()
            .map(|dependency| vec![dependency.clone()])
            .collect();
        println!("\n{}", formatter.table(&["Dependency"], &rows));
        return Ok(());
    }

    let mut summary = format!(
        "{}, {}",
        count_label(graph.nodes.len(), "formula", "formulae"),
        count_label(graph.edge_count(), "runtime edge", "runtime edges")
    );
    if graph.depth_cut_edges > 0 {
        summary.push_str(&format!(
            "; stopped at {MAX_DEPTH} levels ({} not shown)",
            count_label(graph.depth_cut_edges, "edge", "edges")
        ));
    }
    let graph_excluded: usize = graph.nodes.values().map(|node| node.excluded.total()).sum();
    if graph_excluded > root.excluded.total() {
        summary.push_str(&format!(
            "; {graph_excluded} excluded entries across the graph are not expanded"
        ));
    }
    println!("{}", formatter.field("Graph", &summary));

    if graph.edge_count() == 0 {
        println!("\nNo runtime dependencies.");
        return Ok(());
    }
    let rendered = render_forward_tree(&forward_tree(&graph));
    println!("\n{rendered}");
    Ok(())
}

/// Print the reverse (local receipts) dependent listing.
fn reverse_deps(config: &Config, prefix: Option<&Path>, package: &str, tree: bool) -> Result<()> {
    // Validate the caller's identifier before binding to the store.
    let name = plan::parse_target(package)?.to_string();
    let path = prefix_path(prefix)?;
    let store = Store::open(&path, false)?;
    let receipts = store.installed()?;
    let formatter = DnfStyleFormatter::new(config);
    let prefix_display = store.prefix().display().to_string();

    println!(
        "{}  {}",
        formatter.section("Native dependents"),
        formatter.info(&prefix_display)
    );

    let Some(target) = receipts.iter().find(|receipt| receipt.name == name) else {
        println!("\n'{name}' is not installed in this brewrs prefix, so no local receipt pins it.");
        println!("Use `brewrs native list` to see installed packages.");
        return Ok(());
    };

    println!(
        "{}",
        formatter.field(
            "Formula",
            &name_and_version(&target.name, Some(&target.version))
        )
    );
    println!("{}", formatter.field("Scope", SCOPE_REVERSE));

    if !tree {
        let dependents = direct_dependents(&receipts, target);
        if dependents.is_empty() {
            println!("\nNo installed packages depend on '{}'.", target.name);
            return Ok(());
        }
        let rows: Vec<Vec<String>> = dependents
            .iter()
            .map(|dependent| {
                vec![
                    dependent.name.clone(),
                    dependent.version.clone(),
                    dependent
                        .dependencies
                        .iter()
                        .find(|dependency| pin_matches(dependency, target))
                        .map_or_else(String::new, |pin| short_digest(&pin.sha256)),
                    if dependent.requested {
                        "Requested".to_string()
                    } else {
                        "Dependency".to_string()
                    },
                ]
            })
            .collect();
        println!(
            "\n{}",
            formatter.field("Dependents", &dependents.len().to_string())
        );
        println!(
            "{}",
            formatter.table(&["Dependent", "Version", "Pinned digest", "Reason"], &rows)
        );
        return Ok(());
    }

    let tree_root = build_dependent_tree(&receipts, target);
    let count = tree_root.count_descendants();
    if count == 0 {
        println!(
            "\nNo installed packages depend on '{}' recursively.",
            target.name
        );
        return Ok(());
    }
    println!("{}", formatter.field("Dependents", &count.to_string()));
    let rendered = render_forward_tree(&dependent_tree_to_tree(&tree_root));
    println!("\n{rendered}");
    Ok(())
}

/// [`plan`]'s online formula source, using the shared HTTP client.
struct HttpFormulaSource {
    http: HttpClient,
    config: Config,
}

impl FormulaSource for HttpFormulaSource {
    async fn fetch(&self, name: &str) -> Result<FormulaPayload> {
        // `name` comes from `plan::parse_target`, so it cannot alter the path.
        let url = format!("{API_BASE}/formula/{name}.json");
        match self.http.fetch_json(&url).await {
            Ok(payload) => Ok(payload),
            Err(error) => {
                if is_not_found(&error) {
                    if let Ok(Some(canonical)) =
                        catalog_cache::resolve_formula_alias(&self.config, name).await
                    {
                        if !canonical.contains('/') && plan::parse_target(&canonical).is_ok() {
                            let alias_url = format!("{API_BASE}/formula/{canonical}.json");
                            return self.http.fetch_json(&alias_url).await.with_context(|| {
                                format!("failed to fetch formula metadata from {alias_url}")
                            });
                        }
                    }
                }
                Err(error).with_context(|| format!("failed to fetch formula metadata from {url}"))
            }
        }
    }
}

/// A source of formula metadata, kept as a seam for offline fixture tests.
trait FormulaSource {
    async fn fetch(&self, name: &str) -> Result<FormulaPayload>;
}

/// The subset of the official formula payload used by this query.
#[derive(Debug, Clone, Default, Deserialize)]
struct FormulaPayload {
    #[serde(default)]
    name: String,
    #[serde(default)]
    tap: String,
    #[serde(default)]
    versions: FormulaVersions,
    #[serde(default)]
    revision: Option<u32>,
    #[serde(default)]
    dependencies: Vec<String>,
    #[serde(default)]
    build_dependencies: Vec<Value>,
    #[serde(default)]
    test_dependencies: Vec<Value>,
    #[serde(default)]
    recommended_dependencies: Vec<Value>,
    #[serde(default)]
    optional_dependencies: Vec<Value>,
    #[serde(default)]
    uses_from_macos: Vec<Value>,
    #[serde(default)]
    variations: HashMap<String, Variation>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct FormulaVersions {
    #[serde(default)]
    stable: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct Variation {
    /// `None` means the variation does not override dependencies; `Some(vec![])`
    /// means it explicitly removes them.
    #[serde(default)]
    dependencies: Option<Vec<String>>,
}

/// A validated formula in the forward graph.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ForwardNode {
    version: Option<String>,
    dependencies: Vec<String>,
    variation_used: bool,
    excluded: ExcludedCounts,
    excluded_entries: Vec<ExcludedEntry>,
}

/// A complete forward query result.
#[derive(Debug, Default)]
struct ForwardGraph {
    /// Canonical name of the queried formula.
    root: String,
    /// Host tag whose variation was consulted.
    tag: String,
    /// Every requested name that has been resolved to a canonical name.
    canonical_for: HashMap<String, String>,
    /// Validated nodes keyed by canonical name.
    nodes: BTreeMap<String, ForwardNode>,
    /// Dependency edges cut off by [`MAX_DEPTH`].
    depth_cut_edges: usize,
}

impl ForwardGraph {
    /// Resolve a requested name through the alias map, if it is known.
    fn canonical_name(&self, requested: &str) -> String {
        self.canonical_for
            .get(requested)
            .cloned()
            .unwrap_or_else(|| requested.to_string())
    }

    /// Total number of recorded runtime edges.
    fn edge_count(&self) -> usize {
        self.nodes
            .values()
            .map(|node| node.dependencies.len())
            .sum()
    }
}

/// Discover the forward graph breadth first, fetching each name at most once.
async fn build_forward_graph<S: FormulaSource>(
    source: &S,
    target: &str,
    tag: &str,
    recursive: bool,
) -> Result<ForwardGraph> {
    let payload = source.fetch(target).await?;
    let (canonical, node) = build_node(payload, tag)?;
    let mut graph = ForwardGraph {
        root: canonical.clone(),
        tag: tag.to_string(),
        ..ForwardGraph::default()
    };
    graph
        .canonical_for
        .insert(target.to_string(), canonical.clone());
    graph
        .canonical_for
        .insert(canonical.clone(), canonical.clone());
    let root_dependencies = node.dependencies.clone();
    graph.nodes.insert(canonical.clone(), node);
    if !recursive {
        return Ok(graph);
    }

    let mut queue: VecDeque<(String, usize)> = VecDeque::new();
    let mut enqueued: HashSet<String> = HashSet::new();
    enqueued.insert(canonical.clone());
    for dependency in root_dependencies {
        if enqueued.insert(dependency.clone()) {
            queue.push_back((dependency, 1));
        }
    }
    if enqueued.len() > MAX_REQUESTS {
        bail!("dependency graph exceeds the {MAX_REQUESTS}-request limit");
    }

    while let Some((requested, depth)) = queue.pop_front() {
        // A request is skipped when its name or canonical target is already
        // resolved, which also breaks dependency cycles.
        if graph.canonical_for.contains_key(&requested) || graph.nodes.contains_key(&requested) {
            continue;
        }

        let payload = source
            .fetch(&requested)
            .await
            .with_context(|| format!("while resolving dependency '{requested}'"))?;
        let (canonical, node) = build_node(payload, tag)?;
        graph
            .canonical_for
            .insert(requested.clone(), canonical.clone());
        graph
            .canonical_for
            .entry(canonical.clone())
            .or_insert_with(|| canonical.clone());
        if graph.nodes.contains_key(&canonical) {
            continue;
        }
        if graph.nodes.len() >= MAX_FORMULAE {
            bail!("dependency graph exceeds the {MAX_FORMULAE}-formula limit");
        }

        for dependency in &node.dependencies {
            if depth < MAX_DEPTH {
                if enqueued.insert(dependency.clone()) {
                    if enqueued.len() > MAX_REQUESTS {
                        bail!("dependency graph exceeds the {MAX_REQUESTS}-request limit");
                    }
                    queue.push_back((dependency.clone(), depth + 1));
                }
            } else if !enqueued.contains(dependency) && !graph.nodes.contains_key(dependency) {
                graph.depth_cut_edges += 1;
            }
        }
        graph.nodes.insert(canonical, node);
    }

    Ok(graph)
}

/// Build and validate one graph node without requiring a bottle.
fn build_node(payload: FormulaPayload, tag: &str) -> Result<(String, ForwardNode)> {
    if payload.tap != CORE_TAP {
        bail!(
            "unsupported tap '{}' in formula metadata: only {CORE_TAP} is supported",
            safe_text(&payload.tap, MAX_DISPLAY_VERSION_LEN)
        );
    }
    let name = plan::parse_target(&payload.name)
        .with_context(|| {
            format!(
                "formula API returned an invalid name '{}'",
                safe_text(&payload.name, MAX_DISPLAY_VERSION_LEN)
            )
        })?
        .to_string();

    let version = render_version(payload.versions.stable.as_deref(), payload.revision);
    let (dependency_list, variation_used) = runtime_dependency_names(&payload, tag);
    if dependency_list.len() > MAX_DEPS_PER_FORMULA {
        bail!(
            "formula '{name}' declares {} dependencies, above the {MAX_DEPS_PER_FORMULA} limit",
            dependency_list.len()
        );
    }

    let mut dependencies = Vec::with_capacity(dependency_list.len());
    let mut seen = HashSet::new();
    for dependency in dependency_list {
        let dependency = plan::parse_target(dependency)
            .with_context(|| {
                format!(
                    "formula '{name}' declares an invalid dependency '{}'",
                    safe_text(dependency, MAX_DISPLAY_VERSION_LEN)
                )
            })?
            .to_string();
        if seen.insert(dependency.clone()) {
            dependencies.push(dependency);
        }
    }

    Ok((
        name,
        ForwardNode {
            version,
            dependencies,
            variation_used,
            excluded: ExcludedCounts::from_payload(&payload),
            excluded_entries: excluded_entries(&payload),
        },
    ))
}

/// Select the runtime dependency list for `tag`.
///
/// Returns the list plus whether a variation override was used. A variation
/// without a `dependencies` key inherits the formula's base list; an explicit
/// empty list removes it.
fn runtime_dependency_names<'a>(payload: &'a FormulaPayload, tag: &str) -> (&'a [String], bool) {
    match payload
        .variations
        .get(tag)
        .and_then(|variation| variation.dependencies.as_deref())
    {
        Some(dependencies) => (dependencies, true),
        None => (&payload.dependencies, false),
    }
}

/// Render a stable version with Homebrew's `_<revision>` suffix.
fn render_version(stable: Option<&str>, revision: Option<u32>) -> Option<String> {
    let stable = stable?;
    if stable.is_empty() {
        return None;
    }
    let revision = revision.unwrap_or(0);
    let rendered = if revision == 0 {
        stable.to_string()
    } else {
        format!("{stable}_{revision}")
    };
    Some(safe_text(&rendered, MAX_DISPLAY_VERSION_LEN))
}

/// Categories of dependencies this query does not expand.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExcludedKind {
    Build,
    Test,
    Optional,
    Recommended,
    Macos,
}

impl ExcludedKind {
    const ALL: [Self; 5] = [
        Self::Build,
        Self::Test,
        Self::Optional,
        Self::Recommended,
        Self::Macos,
    ];

    /// Stable label used in output.
    fn label(self) -> &'static str {
        match self {
            Self::Build => "build",
            Self::Test => "test",
            Self::Optional => "optional",
            Self::Recommended => "recommended",
            Self::Macos => "uses_from_macos",
        }
    }

    /// Position in [`ExcludedCounts`].
    fn index(self) -> usize {
        match self {
            Self::Build => 0,
            Self::Test => 1,
            Self::Optional => 2,
            Self::Recommended => 3,
            Self::Macos => 4,
        }
    }

    /// The raw payload list for this category.
    fn values(self, payload: &FormulaPayload) -> &[Value] {
        match self {
            Self::Build => &payload.build_dependencies,
            Self::Test => &payload.test_dependencies,
            Self::Optional => &payload.optional_dependencies,
            Self::Recommended => &payload.recommended_dependencies,
            Self::Macos => &payload.uses_from_macos,
        }
    }
}

/// Per-category counts of dependencies outside the runtime graph.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct ExcludedCounts([usize; 5]);

impl ExcludedCounts {
    fn from_payload(payload: &FormulaPayload) -> Self {
        let mut counts = [0usize; 5];
        for kind in ExcludedKind::ALL {
            counts[kind.index()] = kind.values(payload).len();
        }
        Self(counts)
    }

    fn get(self, kind: ExcludedKind) -> usize {
        self.0[kind.index()]
    }

    fn total(self) -> usize {
        self.0.iter().sum()
    }
}

/// One named dependency outside the runtime graph.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ExcludedEntry {
    kind: ExcludedKind,
    name: String,
}

/// Collect validated excluded dependency names, bounded per category.
fn excluded_entries(payload: &FormulaPayload) -> Vec<ExcludedEntry> {
    let mut entries = Vec::new();
    for kind in ExcludedKind::ALL {
        for name in value_names(kind.values(payload))
            .into_iter()
            .take(MAX_EXCLUDED_NAMES_PER_KIND)
        {
            entries.push(ExcludedEntry { kind, name });
        }
    }
    entries
}

/// Extract validated dependency identifiers from loosely typed API entries.
///
/// Entries are either `"name"` strings or objects with a `"name"` field. Every
/// identifier is validated by the planner's target parser, and anything it
/// rejects is silently left out of the display list; the category count still
/// reflects the raw length.
fn value_names(values: &[Value]) -> Vec<String> {
    values
        .iter()
        .filter_map(|value| match value {
            Value::String(name) => Some(name.as_str()),
            Value::Object(object) => object.get("name").and_then(Value::as_str),
            _ => None,
        })
        .filter_map(|name| plan::parse_target(name).ok().map(str::to_owned))
        .collect()
}

/// Render the excluded scope as one line per non-empty category.
fn format_excluded(counts: ExcludedCounts, entries: &[ExcludedEntry]) -> String {
    let mut lines = Vec::new();
    for kind in ExcludedKind::ALL {
        let total = counts.get(kind);
        if total == 0 {
            continue;
        }
        let names: Vec<&str> = entries
            .iter()
            .filter(|entry| entry.kind == kind)
            .map(|entry| entry.name.as_str())
            .collect();
        if names.is_empty() {
            lines.push(format!("{}: {total}", kind.label()));
            continue;
        }
        let hidden = total.saturating_sub(names.len());
        if hidden == 0 {
            lines.push(format!("{}: {}", kind.label(), names.join(", ")));
        } else {
            lines.push(format!(
                "{}: {} (+{hidden} more)",
                kind.label(),
                names.join(", ")
            ));
        }
    }
    if lines.is_empty() {
        "none".to_string()
    } else {
        lines.join("\n")
    }
}

/// Why a tree node's children are (or are not) rendered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Branch {
    Expanded,
    Cycle,
    Repeated,
    DepthLimit,
}

/// One rendered node of a dependency or dependent tree.
#[derive(Debug, Clone, PartialEq, Eq)]
struct TreeNode {
    name: String,
    version: Option<String>,
    variation_used: bool,
    branch: Branch,
    children: Vec<TreeNode>,
}

/// Build the bounded, cycle-safe render tree for a forward graph.
fn forward_tree(graph: &ForwardGraph) -> TreeNode {
    let mut expanded = HashSet::new();
    let mut path = Vec::new();
    expand_forward(graph, &graph.root, 0, &mut path, &mut expanded)
}

fn expand_forward(
    graph: &ForwardGraph,
    requested: &str,
    depth: usize,
    path: &mut Vec<String>,
    expanded: &mut HashSet<String>,
) -> TreeNode {
    let canonical = graph.canonical_name(requested);
    let Some(node) = graph.nodes.get(&canonical) else {
        // Defensive: every displayed level is fetched, so this is only a node
        // the API resolved differently from what the parent declared.
        return TreeNode {
            name: canonical,
            version: None,
            variation_used: false,
            branch: Branch::Expanded,
            children: Vec::new(),
        };
    };

    if path.iter().any(|ancestor| ancestor == &canonical) {
        return TreeNode {
            name: canonical,
            version: node.version.clone(),
            variation_used: node.variation_used,
            branch: Branch::Cycle,
            children: Vec::new(),
        };
    }
    if expanded.contains(&canonical) {
        return TreeNode {
            name: canonical,
            version: node.version.clone(),
            variation_used: node.variation_used,
            branch: Branch::Repeated,
            children: Vec::new(),
        };
    }
    if depth >= MAX_DEPTH && !node.dependencies.is_empty() {
        return TreeNode {
            name: canonical,
            version: node.version.clone(),
            variation_used: node.variation_used,
            branch: Branch::DepthLimit,
            children: Vec::new(),
        };
    }

    path.push(canonical.clone());
    let children = node
        .dependencies
        .iter()
        .map(|dependency| expand_forward(graph, dependency, depth + 1, path, expanded))
        .collect();
    path.pop();
    expanded.insert(canonical.clone());

    TreeNode {
        name: canonical,
        version: node.version.clone(),
        variation_used: node.variation_used,
        branch: Branch::Expanded,
        children,
    }
}

/// Render a tree with ASCII branches, one node per line.
fn render_forward_tree(root: &TreeNode) -> String {
    let mut lines = vec![tree_label(root)];
    render_children(&root.children, "", &mut lines);
    lines.join("\n")
}

fn render_children(children: &[TreeNode], prefix: &str, lines: &mut Vec<String>) {
    for (index, child) in children.iter().enumerate() {
        let last = index + 1 == children.len();
        lines.push(format!(
            "{prefix}{}{}",
            if last { "`- " } else { "|- " },
            tree_label(child)
        ));
        let next_prefix = format!("{prefix}{}", if last { "   " } else { "|  " });
        render_children(&child.children, &next_prefix, lines);
    }
}

/// One tree line: `name version* (branch note)`.
fn tree_label(node: &TreeNode) -> String {
    let mut label = name_and_version(&node.name, node.version.as_deref());
    if node.variation_used {
        label.push('*');
    }
    match node.branch {
        Branch::Expanded => {}
        Branch::Cycle => label.push_str(" (cycle)"),
        Branch::Repeated => label.push_str(" (already shown)"),
        Branch::DepthLimit => label.push_str(" (depth limit)"),
    }
    label
}

fn name_and_version(name: &str, version: Option<&str>) -> String {
    match version {
        Some(version) => format!("{name} {version}"),
        None => name.to_string(),
    }
}

/// `1 formula` / `2 formulae`.
fn count_label(count: usize, singular: &str, plural: &str) -> String {
    format!("{count} {}", if count == 1 { singular } else { plural })
}

/// One receipt in the reverse graph, with its expanded dependents.
#[derive(Debug)]
struct DependentTree<'a> {
    receipt: &'a Receipt,
    branch: Branch,
    children: Vec<DependentTree<'a>>,
}

impl DependentTree<'_> {
    /// Number of distinct dependent receipts below (and including) this node.
    fn count_descendants(&self) -> usize {
        let mut names = BTreeSet::new();
        for child in &self.children {
            child.collect_names(&mut names);
        }
        names.len()
    }

    fn collect_names<'a>(&'a self, names: &mut BTreeSet<&'a str>) {
        names.insert(self.receipt.name.as_str());
        for child in &self.children {
            child.collect_names(names);
        }
    }
}

/// Build the reverse tree for one installed target.
fn build_dependent_tree<'a>(receipts: &'a [Receipt], target: &'a Receipt) -> DependentTree<'a> {
    let mut expanded = HashSet::new();
    let mut path = Vec::new();
    expand_dependents(receipts, target, &mut path, &mut expanded)
}

fn expand_dependents<'a>(
    receipts: &'a [Receipt],
    target: &'a Receipt,
    path: &mut Vec<String>,
    expanded: &mut HashSet<String>,
) -> DependentTree<'a> {
    if path.iter().any(|ancestor| ancestor == &target.name) {
        return DependentTree {
            receipt: target,
            branch: Branch::Cycle,
            children: Vec::new(),
        };
    }
    if expanded.contains(&target.name) {
        return DependentTree {
            receipt: target,
            branch: Branch::Repeated,
            children: Vec::new(),
        };
    }

    path.push(target.name.clone());
    let children = direct_dependents(receipts, target)
        .into_iter()
        .map(|dependent| expand_dependents(receipts, dependent, path, expanded))
        .collect();
    path.pop();
    expanded.insert(target.name.clone());

    DependentTree {
        receipt: target,
        branch: Branch::Expanded,
        children,
    }
}

/// Receipts that pin `target` exactly, sorted by name.
fn direct_dependents<'a>(receipts: &'a [Receipt], target: &Receipt) -> Vec<&'a Receipt> {
    let mut dependents: Vec<&Receipt> = receipts
        .iter()
        .filter(|receipt| {
            receipt.name != target.name
                && receipt
                    .dependencies
                    .iter()
                    .any(|dependency| pin_matches(dependency, target))
        })
        .collect();
    dependents.sort_by(|left, right| left.name.cmp(&right.name));
    dependents
}

/// Whether a recorded pin names `target` at its exact installed version and
/// digest.
fn pin_matches(dependency: &Dependency, target: &Receipt) -> bool {
    dependency.name == target.name
        && dependency.version == target.version
        && dependency.sha256 == target.sha256
}

/// Convert a reverse tree into the shared render tree.
fn dependent_tree_to_tree(tree: &DependentTree<'_>) -> TreeNode {
    TreeNode {
        name: tree.receipt.name.clone(),
        version: Some(tree.receipt.version.clone()),
        variation_used: false,
        branch: tree.branch,
        children: tree.children.iter().map(dependent_tree_to_tree).collect(),
    }
}

/// Render a shortened digest for a compact table.
fn short_digest(digest: &str) -> String {
    digest.chars().take(SHORT_DIGEST_LEN).collect()
}

/// Replace control and non-ASCII characters and bound the rendered length.
///
/// Formula metadata is untrusted; every value that is printed without passing
/// through the table formatter goes through this first.
fn safe_text(value: &str, limit: usize) -> String {
    let mut rendered = String::new();
    for character in value.chars() {
        if rendered.len() >= limit {
            rendered.push('~');
            break;
        }
        rendered.push(if character.is_ascii_graphic() {
            character
        } else {
            '?'
        });
    }
    rendered
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::cell::RefCell;

    const TAG: &str = "arm64_sequoia";
    const DIGEST_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const DIGEST_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    fn parse_payload(value: Value) -> FormulaPayload {
        serde_json::from_value(value).expect("fixture payload")
    }

    fn payload(name: &str, stable: &str, dependencies: &[&str]) -> Value {
        json!({
            "name": name,
            "tap": CORE_TAP,
            "versions": { "stable": stable },
            "dependencies": dependencies,
        })
    }

    struct FixtureSource {
        docs: HashMap<String, String>,
        fetches: RefCell<HashMap<String, usize>>,
    }

    impl FixtureSource {
        fn new<S: Into<String>>(docs: Vec<(S, Value)>) -> Self {
            Self {
                docs: docs
                    .into_iter()
                    .map(|(name, doc)| (name.into(), doc.to_string()))
                    .collect(),
                fetches: RefCell::new(HashMap::new()),
            }
        }

        fn fetch_count(&self, name: &str) -> usize {
            self.fetches.borrow().get(name).copied().unwrap_or(0)
        }
    }

    impl FormulaSource for FixtureSource {
        async fn fetch(&self, name: &str) -> Result<FormulaPayload> {
            *self
                .fetches
                .borrow_mut()
                .entry(name.to_string())
                .or_insert(0) += 1;
            let doc = self
                .docs
                .get(name)
                .with_context(|| format!("no fixture for '{name}'"))?;
            serde_json::from_str(doc).with_context(|| format!("invalid fixture for '{name}'"))
        }
    }

    #[test]
    fn uses_variation_dependencies_when_declared() {
        let payload = parse_payload(json!({
            "name": "tool",
            "tap": CORE_TAP,
            "versions": { "stable": "1.0.0" },
            "dependencies": ["base"],
            "variations": { TAG: { "dependencies": ["linux-only"] } },
        }));
        let (dependencies, variation) = runtime_dependency_names(&payload, TAG);
        assert_eq!(dependencies, ["linux-only"]);
        assert!(variation);

        let (dependencies, variation) = runtime_dependency_names(&payload, "arm64_linux");
        assert_eq!(dependencies, ["base"]);
        assert!(!variation);
    }

    #[test]
    fn variation_without_dependencies_inherits_and_empty_removes() {
        let inherited = parse_payload(json!({
            "name": "tool",
            "tap": CORE_TAP,
            "dependencies": ["base"],
            "variations": { TAG: { "requirements": [] } },
        }));
        assert_eq!(runtime_dependency_names(&inherited, TAG).0, ["base"]);
        assert!(!runtime_dependency_names(&inherited, TAG).1);

        let removed = parse_payload(json!({
            "name": "tool",
            "tap": CORE_TAP,
            "dependencies": ["base"],
            "variations": { TAG: { "dependencies": [] } },
        }));
        assert!(runtime_dependency_names(&removed, TAG).0.is_empty());
        assert!(runtime_dependency_names(&removed, TAG).1);
    }

    #[test]
    fn builds_a_node_without_any_bottle_metadata() {
        let mut doc = payload("tool", "1.0.0", &["lib"]);
        doc["revision"] = json!(2);
        let (name, node) = build_node(parse_payload(doc), TAG).expect("node");
        assert_eq!(name, "tool");
        assert_eq!(node.version.as_deref(), Some("1.0.0_2"));
        assert_eq!(node.dependencies, ["lib"]);
        assert!(!node.variation_used);
        assert!(node.excluded.total() == 0);
    }

    #[test]
    fn build_node_deduplicates_and_rejects_unsafe_metadata() {
        let (_, node) = build_node(
            parse_payload(payload("tool", "1.0.0", &["lib", "lib", "util"])),
            TAG,
        )
        .expect("node");
        assert_eq!(node.dependencies, ["lib", "util"]);

        let mut other_tap = payload("tool", "1.0.0", &[]);
        other_tap["tap"] = json!("someone/else");
        assert!(build_node(parse_payload(other_tap), TAG).is_err());

        let mut bad_name = payload("tool", "1.0.0", &[]);
        bad_name["name"] = json!("../evil");
        assert!(build_node(parse_payload(bad_name), TAG).is_err());

        let mut bad_dependency = payload("tool", "1.0.0", &[]);
        bad_dependency["dependencies"] = json!(["../evil"]);
        assert!(build_node(parse_payload(bad_dependency), TAG).is_err());

        let too_many: Vec<String> = (0..=MAX_DEPS_PER_FORMULA)
            .map(|index| format!("dep{index}"))
            .collect();
        let mut oversized = payload("tool", "1.0.0", &[]);
        oversized["dependencies"] = json!(too_many);
        assert!(build_node(parse_payload(oversized), TAG).is_err());
    }

    #[test]
    fn excluded_entries_support_strings_objects_and_counts() {
        let payload = parse_payload(json!({
            "name": "tool",
            "tap": CORE_TAP,
            "build_dependencies": ["cmake", { "name": "pkgconf" }, { "name": "../evil" }],
            "test_dependencies": ["pytest"],
            "optional_dependencies": [],
            "recommended_dependencies": ["recommended-lib"],
            "uses_from_macos": ["krb5"],
        }));
        let counts = ExcludedCounts::from_payload(&payload);
        assert_eq!(counts.get(ExcludedKind::Build), 3);
        assert_eq!(counts.get(ExcludedKind::Test), 1);
        assert_eq!(counts.get(ExcludedKind::Recommended), 1);
        assert_eq!(counts.get(ExcludedKind::Macos), 1);
        assert_eq!(counts.total(), 6);

        let entries = excluded_entries(&payload);
        assert_eq!(
            entries,
            [
                ExcludedEntry {
                    kind: ExcludedKind::Build,
                    name: "cmake".into()
                },
                ExcludedEntry {
                    kind: ExcludedKind::Build,
                    name: "pkgconf".into()
                },
                ExcludedEntry {
                    kind: ExcludedKind::Test,
                    name: "pytest".into()
                },
                ExcludedEntry {
                    kind: ExcludedKind::Recommended,
                    name: "recommended-lib".into()
                },
                ExcludedEntry {
                    kind: ExcludedKind::Macos,
                    name: "krb5".into()
                },
            ]
        );

        assert_eq!(
            format_excluded(counts, &entries),
            "build: cmake, pkgconf (+1 more)\ntest: pytest\nrecommended: recommended-lib\nuses_from_macos: krb5"
        );
        assert_eq!(format_excluded(ExcludedCounts::default(), &[]), "none");
    }

    #[tokio::test]
    async fn direct_query_fetches_only_the_target() {
        let source = FixtureSource::new(vec![
            ("app", payload("app", "1.0.0", &["lib"])),
            ("lib", payload("lib", "1.0.0", &[])),
        ]);
        let graph = build_forward_graph(&source, "app", TAG, false)
            .await
            .expect("graph");
        assert_eq!(graph.nodes.len(), 1);
        assert_eq!(graph.edge_count(), 1);
        assert_eq!(source.fetch_count("app"), 1);
        assert_eq!(source.fetch_count("lib"), 0);
    }

    #[tokio::test]
    async fn recursive_query_fetches_shared_dependencies_once() {
        let source = FixtureSource::new(vec![
            ("app", payload("app", "1.0.0", &["left", "right"])),
            ("left", payload("left", "2.0.0", &["common"])),
            ("right", payload("right", "3.0.0", &["common"])),
            ("common", payload("common", "4.0.0", &[])),
        ]);
        let graph = build_forward_graph(&source, "app", TAG, true)
            .await
            .expect("graph");
        assert_eq!(graph.nodes.len(), 4);
        assert_eq!(graph.edge_count(), 4);
        for name in ["app", "left", "right", "common"] {
            assert_eq!(source.fetch_count(name), 1, "{name} fetched more than once");
        }

        assert_eq!(
            render_forward_tree(&forward_tree(&graph)),
            "app 1.0.0\n\
             |- left 2.0.0\n\
             |  `- common 4.0.0\n\
             `- right 3.0.0\n\
             \x20  `- common 4.0.0 (already shown)"
        );
    }

    #[tokio::test]
    async fn recursive_query_terminates_on_cycles() {
        let source = FixtureSource::new(vec![
            ("a", payload("a", "1.0.0", &["b"])),
            ("b", payload("b", "1.0.0", &["a"])),
        ]);
        let graph = build_forward_graph(&source, "a", TAG, true)
            .await
            .expect("graph");
        assert_eq!(graph.nodes.len(), 2);
        assert_eq!(source.fetch_count("a"), 1);
        assert_eq!(source.fetch_count("b"), 1);
        assert_eq!(
            render_forward_tree(&forward_tree(&graph)),
            "a 1.0.0\n`- b 1.0.0\n   `- a 1.0.0 (cycle)"
        );
    }

    #[tokio::test]
    async fn recursive_query_stops_at_the_depth_limit() {
        let mut docs: Vec<(String, Value)> = Vec::new();
        for index in 0..24 {
            let name = format!("n{index}");
            let next = format!("n{}", index + 1);
            let mut doc = payload(&name, "1.0.0", &[]);
            doc["dependencies"] = json!([next]);
            docs.push((name, doc));
        }
        let source = FixtureSource::new(docs);

        let graph = build_forward_graph(&source, "n0", TAG, true)
            .await
            .expect("graph");
        assert_eq!(graph.nodes.len(), MAX_DEPTH + 1);
        assert_eq!(graph.depth_cut_edges, 1);
        let rendered = render_forward_tree(&forward_tree(&graph));
        assert!(
            rendered.ends_with("(depth limit)"),
            "deepest level must be marked: {rendered}"
        );
        assert_eq!(source.fetch_count("n0"), 1);
    }

    #[tokio::test]
    async fn marks_a_variation_override_in_the_tree() {
        let mut doc = payload("tool", "1.0.0", &["base"]);
        doc["variations"] = json!({ TAG: { "dependencies": ["linux"] } });
        let source = FixtureSource::new(vec![
            ("tool", doc),
            ("linux", payload("linux", "1.0.0", &[])),
            ("base", payload("base", "1.0.0", &[])),
        ]);
        let graph = build_forward_graph(&source, "tool", TAG, true)
            .await
            .expect("graph");
        assert_eq!(
            render_forward_tree(&forward_tree(&graph)),
            "tool 1.0.0*\n`- linux 1.0.0"
        );
        assert_eq!(source.fetch_count("base"), 0);
    }

    fn receipt(name: &str, version: &str, requested: bool, pins: &[(&str, &str, &str)]) -> Receipt {
        Receipt {
            name: name.to_string(),
            version: version.to_string(),
            version_scheme: 0,
            sha256: DIGEST_A.to_string(),
            requested,
            keg_only: false,
            conflicts_with: Vec::new(),
            dependencies: pins
                .iter()
                .map(|(name, version, sha256)| Dependency {
                    name: (*name).to_string(),
                    version: (*version).to_string(),
                    sha256: (*sha256).to_string(),
                })
                .collect(),
        }
    }

    #[test]
    fn reverse_matches_exact_pins_only_and_sorts_by_name() {
        let target = receipt("lib", "1.0.0", true, &[]);
        let receipts = vec![
            receipt("zeta", "1.0.0", false, &[("lib", "1.0.0", DIGEST_A)]),
            receipt("alpha", "2.0.0", true, &[("lib", "1.0.0", DIGEST_A)]),
            receipt(
                "wrong-digest",
                "3.0.0",
                false,
                &[("lib", "1.0.0", DIGEST_B)],
            ),
            receipt(
                "wrong-version",
                "4.0.0",
                false,
                &[("lib", "2.0.0", DIGEST_A)],
            ),
            receipt("unrelated", "5.0.0", false, &[("other", "1.0.0", DIGEST_A)]),
        ];
        let dependents = direct_dependents(&receipts, &target);
        let names: Vec<&str> = dependents
            .iter()
            .map(|receipt| receipt.name.as_str())
            .collect();
        assert_eq!(names, ["alpha", "zeta"]);
    }

    #[test]
    fn reverse_tree_is_recursive_and_marks_repeats() {
        let target = receipt("lib", "1.0.0", true, &[]);
        let receipts = vec![
            receipt("lib", "1.0.0", true, &[]),
            receipt(
                "middle",
                "2.0.0",
                true,
                &[("lib", "1.0.0", DIGEST_A), ("base", "1.0.0", DIGEST_A)],
            ),
            receipt(
                "top",
                "3.0.0",
                true,
                &[("middle", "2.0.0", DIGEST_A), ("lib", "1.0.0", DIGEST_A)],
            ),
            receipt("base", "1.0.0", false, &[]),
        ];
        let tree = build_dependent_tree(&receipts, &target);
        assert_eq!(tree.count_descendants(), 2);
        assert_eq!(
            render_forward_tree(&dependent_tree_to_tree(&tree)),
            "lib 1.0.0\n\
             |- middle 2.0.0\n\
             |  `- top 3.0.0\n\
             `- top 3.0.0 (already shown)"
        );
    }

    #[test]
    fn reverse_tree_terminates_on_a_pin_cycle() {
        let target = receipt("lib", "1.0.0", true, &[]);
        let receipts = vec![
            receipt("a", "1.0.0", true, &[("lib", "1.0.0", DIGEST_A)]),
            receipt(
                "b",
                "1.0.0",
                true,
                &[("a", "1.0.0", DIGEST_A), ("lib", "1.0.0", DIGEST_A)],
            ),
        ];
        // `a` and `b` also pin each other in the reverse direction.
        let mut receipts = receipts;
        receipts[0].dependencies.push(Dependency {
            name: "b".into(),
            version: "1.0.0".into(),
            sha256: DIGEST_A.into(),
        });

        let tree = build_dependent_tree(&receipts, &target);
        let rendered = render_forward_tree(&dependent_tree_to_tree(&tree));
        assert!(rendered.contains("(cycle)"), "{rendered}");
        assert!(
            rendered.lines().count() <= 6,
            "cycle must stay bounded: {rendered}"
        );
    }

    #[test]
    fn reverse_query_on_an_absent_prefix_writes_nothing() {
        let directory = tempfile::TempDir::new().expect("tempdir");
        let prefix = directory.path().join("native");
        let config = Config::default();
        reverse_deps(&config, Some(&prefix), "wget", false).expect("reverse query");
        assert!(
            !prefix.exists(),
            "a reverse query must not create the prefix"
        );
    }

    #[tokio::test]
    async fn invalid_targets_are_rejected_before_any_query() {
        let config = Config::default();
        for target in ["", "../evil", "Ripgrep", "homebrew/cask/firefox"] {
            assert!(
                deps(&config, None, target, false, false).await.is_err(),
                "target '{target}' must be rejected before querying"
            );
            assert!(
                deps(&config, None, target, false, true).await.is_err(),
                "reverse target '{target}' must be rejected"
            );
        }
    }

    #[test]
    fn safe_text_bounds_and_sanitizes_untrusted_values() {
        assert_eq!(safe_text("1.2.3", 16), "1.2.3");
        assert_eq!(safe_text("1.2.3\u{1b}[31m", 8), "1.2.3?[3~");
        assert_eq!(safe_text(&"x".repeat(80), 8), "xxxxxxxx~");
    }
}
