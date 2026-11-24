use std::collections::HashSet;

use super::graph::{ForwardGraph, MAX_DEPTH};

/// Why a tree node's children are (or are not) rendered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Branch {
    Expanded,
    Cycle,
    Repeated,
    DepthLimit,
}

/// One rendered node of a dependency or dependent tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct TreeNode {
    pub(super) name: String,
    pub(super) version: Option<String>,
    pub(super) variation_used: bool,
    pub(super) branch: Branch,
    pub(super) children: Vec<TreeNode>,
}

/// Build the bounded, cycle-safe render tree for a forward graph.
pub(super) fn forward_tree(graph: &ForwardGraph) -> TreeNode {
    let mut expanded = HashSet::new();
    let mut path = Vec::new();
    expand_forward(graph, &graph.root, 0, &mut path, &mut expanded)
}

pub(super) fn expand_forward(
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
pub(super) fn render_forward_tree(root: &TreeNode) -> String {
    let mut lines = vec![tree_label(root)];
    render_children(&root.children, "", &mut lines);
    lines.join("\n")
}

pub(super) fn render_children(children: &[TreeNode], prefix: &str, lines: &mut Vec<String>) {
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
pub(super) fn tree_label(node: &TreeNode) -> String {
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

pub(super) fn name_and_version(name: &str, version: Option<&str>) -> String {
    match version {
        Some(version) => format!("{name} {version}"),
        None => name.to_string(),
    }
}

/// `1 formula` / `2 formulae`.
pub(super) fn count_label(count: usize, singular: &str, plural: &str) -> String {
    format!("{count} {}", if count == 1 { singular } else { plural })
}
