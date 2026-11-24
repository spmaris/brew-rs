use std::collections::{BTreeSet, HashSet};

use crate::infra::store::{Dependency, Receipt};

use super::tree::{Branch, TreeNode};

/// Digits of a digest rendered in a table.
pub(super) const SHORT_DIGEST_LEN: usize = 12;

/// One receipt in the reverse graph, with its expanded dependents.
#[derive(Debug)]
pub(super) struct DependentTree<'a> {
    pub(super) receipt: &'a Receipt,
    pub(super) branch: Branch,
    pub(super) children: Vec<DependentTree<'a>>,
}

impl DependentTree<'_> {
    /// Number of distinct dependent receipts below (and including) this node.
    pub(super) fn count_descendants(&self) -> usize {
        let mut names = BTreeSet::new();
        for child in &self.children {
            child.collect_names(&mut names);
        }
        names.len()
    }

    pub(super) fn collect_names<'a>(&'a self, names: &mut BTreeSet<ReceiptKey<'a>>) {
        names.insert(receipt_key(self.receipt));
        for child in &self.children {
            child.collect_names(names);
        }
    }
}

/// Build the reverse tree for one installed target.
pub(super) fn build_dependent_tree<'a>(
    receipts: &'a [Receipt],
    target: &'a Receipt,
) -> DependentTree<'a> {
    let mut expanded = HashSet::new();
    let mut path = Vec::new();
    expand_dependents(receipts, target, &mut path, &mut expanded)
}

pub(super) type ReceiptKey<'a> = (&'a str, &'a str, &'a str);

pub(super) fn receipt_key(receipt: &Receipt) -> ReceiptKey<'_> {
    (&receipt.name, &receipt.version, &receipt.sha256)
}

pub(super) fn expand_dependents<'a>(
    receipts: &'a [Receipt],
    target: &'a Receipt,
    path: &mut Vec<ReceiptKey<'a>>,
    expanded: &mut HashSet<ReceiptKey<'a>>,
) -> DependentTree<'a> {
    let key = receipt_key(target);
    if path.contains(&key) {
        return DependentTree {
            receipt: target,
            branch: Branch::Cycle,
            children: Vec::new(),
        };
    }
    if expanded.contains(&key) {
        return DependentTree {
            receipt: target,
            branch: Branch::Repeated,
            children: Vec::new(),
        };
    }

    path.push(key);
    let children = direct_dependents(receipts, target)
        .into_iter()
        .map(|dependent| expand_dependents(receipts, dependent, path, expanded))
        .collect();
    path.pop();
    expanded.insert(key);

    DependentTree {
        receipt: target,
        branch: Branch::Expanded,
        children,
    }
}

/// Receipts that pin `target` exactly, sorted by name.
pub(super) fn direct_dependents<'a>(receipts: &'a [Receipt], target: &Receipt) -> Vec<&'a Receipt> {
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
pub(super) fn pin_matches(dependency: &Dependency, target: &Receipt) -> bool {
    dependency.name == target.name
        && dependency.version == target.version
        && dependency.sha256 == target.sha256
}

/// Convert a reverse tree into the shared render tree.
pub(super) fn dependent_tree_to_tree(tree: &DependentTree<'_>) -> TreeNode {
    TreeNode {
        name: tree.receipt.name.clone(),
        version: Some(tree.receipt.version.clone()),
        variation_used: false,
        branch: tree.branch,
        children: tree.children.iter().map(dependent_tree_to_tree).collect(),
    }
}

/// Render a shortened digest for a compact table.
pub(super) fn short_digest(digest: &str) -> String {
    digest.chars().take(SHORT_DIGEST_LEN).collect()
}

/// Replace control and non-ASCII characters and bound the rendered length.
///
/// Formula metadata is untrusted; every value that is printed without passing
/// through the table formatter goes through this first.
pub(super) fn safe_text(value: &str, limit: usize) -> String {
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
