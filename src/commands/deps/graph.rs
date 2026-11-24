use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};

use anyhow::{Context, Result, bail};

use crate::domain::plan;

use super::dependents::safe_text;
use super::excluded::{ExcludedCounts, ExcludedEntry, excluded_entries};
use super::source::{CORE_TAP, FormulaPayload, FormulaSource};

/// Maximum dependency levels rendered below the queried formula.
pub(super) const MAX_DEPTH: usize = 16;

/// Maximum distinct formulae read for one recursive forward query.
pub(super) const MAX_FORMULAE: usize = 256;

/// Maximum runtime dependencies read from a single formula.
pub(super) const MAX_DEPS_PER_FORMULA: usize = 256;

/// Maximum distinct names queued for a recursive forward query.
pub(super) const MAX_REQUESTS: usize = 1024;

/// Maximum rendered version length.
pub(super) const MAX_DISPLAY_VERSION_LEN: usize = 64;

/// A validated formula in the forward graph.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ForwardNode {
    pub(super) version: Option<String>,
    pub(super) dependencies: Vec<String>,
    pub(super) variation_used: bool,
    pub(super) excluded: ExcludedCounts,
    pub(super) excluded_entries: Vec<ExcludedEntry>,
}

/// A complete forward query result.
#[derive(Debug, Default)]
pub(super) struct ForwardGraph {
    /// Canonical name of the queried formula.
    pub(super) root: String,
    /// Host tag whose variation was consulted.
    pub(super) tag: String,
    /// Every requested name that has been resolved to a canonical name.
    pub(super) canonical_for: HashMap<String, String>,
    /// Validated nodes keyed by canonical name.
    pub(super) nodes: BTreeMap<String, ForwardNode>,
    /// Dependency edges cut off by [`MAX_DEPTH`].
    pub(super) depth_cut_edges: usize,
}

impl ForwardGraph {
    /// Resolve a requested name through the alias map, if it is known.
    pub(super) fn canonical_name(&self, requested: &str) -> String {
        self.canonical_for
            .get(requested)
            .cloned()
            .unwrap_or_else(|| requested.to_string())
    }

    /// Total number of recorded runtime edges.
    pub(super) fn edge_count(&self) -> usize {
        self.nodes
            .values()
            .map(|node| node.dependencies.len())
            .sum()
    }
}

/// Discover the forward graph breadth first, fetching each name at most once.
pub(super) async fn build_forward_graph<S: FormulaSource>(
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
pub(super) fn build_node(payload: FormulaPayload, tag: &str) -> Result<(String, ForwardNode)> {
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
pub(super) fn runtime_dependency_names<'a>(
    payload: &'a FormulaPayload,
    tag: &str,
) -> (&'a [String], bool) {
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
pub(super) fn render_version(stable: Option<&str>, revision: Option<u32>) -> Option<String> {
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
