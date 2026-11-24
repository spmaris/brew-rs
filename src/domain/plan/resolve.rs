//! Graph discovery and dependency-first ordering.

use std::collections::{HashMap, HashSet, VecDeque};

use anyhow::{Context, Result, bail};

use super::super::plan_error::from_fetch_error;
use super::model::{FormulaNode, PackageSource, PlannedFormula};
use super::node::build_node;
use super::source::FormulaSource;
use super::validate::{parse_target, validate_tag};

/// Maximum number of targets accepted in one plan.
const MAX_TARGETS: usize = 128;
/// Maximum number of distinct formulae in one plan.
const MAX_FORMULAE: usize = 1024;
/// Maximum number of distinct names (targets plus dependencies) queued.
const MAX_REQUESTS: usize = 4096;

/// Build a native install plan for `targets` on host tag `tag`, reading
/// formula metadata through `source`.
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
pub(crate) async fn resolve_with<S: FormulaSource>(
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
                return Err(from_fetch_error(
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
