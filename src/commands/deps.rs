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

use std::path::Path;

use anyhow::{Context, Result};

use super::install::prefix_path;
use crate::config::Config;
use crate::domain::plan;
use crate::infra::api::http_client::HttpClient;
use crate::infra::platform::default_target;
use crate::infra::store::Store;
use crate::ui::DnfStyleFormatter;

use self::dependents::{
    build_dependent_tree, dependent_tree_to_tree, direct_dependents, receipt_key, short_digest,
};
use self::excluded::format_excluded;
use self::graph::{MAX_DEPTH, build_forward_graph};
use self::source::HttpFormulaSource;
use self::tree::{count_label, forward_tree, name_and_version, render_forward_tree};

mod dependents;
mod excluded;
mod graph;
mod source;
mod tree;

/// Scope line for forward queries.
const SCOPE_RUNTIME: &str = "runtime dependencies only: the host-tag `variations` override when declared, otherwise `dependencies`\nbuild, test, optional, recommended and `uses_from_macos` entries are excluded; `*` marks a variation override";

/// Scope line for reverse queries.
const SCOPE_REVERSE: &str = "active and retained brewrs receipts in this prefix, matched by exact (name, version, sha256) pins\nno network access and no writes";

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
    let tag = default_target()?;
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
        println!(
            "\n{}",
            formatter.table(
                &format!("Dependencies ({})", rows.len()),
                &["Dependency"],
                &rows
            )
        );
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
    let mut receipts = store.installed()?;
    let active_count = receipts.len();
    receipts.extend(store.retained()?);
    let formatter = DnfStyleFormatter::new(config);
    let prefix_display = store.prefix().display().to_string();

    println!(
        "{}  {}",
        formatter.section("Native dependents"),
        formatter.info(&prefix_display)
    );

    let mut targets = receipts
        .iter()
        .enumerate()
        .filter(|(_, receipt)| receipt.name == name)
        .peekable();
    if targets.peek().is_none() {
        println!("\n'{name}' has no active or retained receipt in this brewrs prefix.");
        println!("Use `brewrs list` to see active packages.");
        return Ok(());
    }
    println!("{}", formatter.field("Scope", SCOPE_REVERSE));

    for (index, target) in targets {
        let state = if index < active_count {
            "Active keg"
        } else {
            "Retained keg"
        };
        println!(
            "\n{}",
            formatter.field(
                state,
                &name_and_version(&target.name, Some(&target.version))
            )
        );
        if tree {
            let root = build_dependent_tree(&receipts, target);
            let count = root.count_descendants();
            println!("{}", formatter.field("Dependents", &count.to_string()));
            let rendered = render_forward_tree(&dependent_tree_to_tree(&root));
            println!("\n{rendered}");
            continue;
        }

        let dependents = direct_dependents(&receipts, target);
        println!(
            "{}",
            formatter.field("Dependents", &dependents.len().to_string())
        );
        if dependents.is_empty() {
            continue;
        }
        let rows: Vec<Vec<String>> = dependents
            .into_iter()
            .map(|dependent| {
                let reason = if receipts[active_count..]
                    .iter()
                    .any(|retained| receipt_key(retained) == receipt_key(dependent))
                {
                    "Retained"
                } else if dependent.requested {
                    "requested"
                } else {
                    "dependency"
                };
                vec![
                    dependent.name.clone(),
                    dependent.version.clone(),
                    short_digest(&target.sha256),
                    reason.into(),
                ]
            })
            .collect();
        println!(
            "{}",
            formatter.table(
                "Dependents",
                &["Dependent", "Version", "Pinned digest", "Reason"],
                &rows
            )
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests;
