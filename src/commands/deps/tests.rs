use std::cell::RefCell;
use std::collections::HashMap;

use anyhow::{Context, Result};
use serde_json::{Value, json};

use crate::config::Config;
use crate::infra::store::{Dependency, Receipt};

use super::dependents::*;
use super::excluded::*;
use super::graph::*;
use super::source::*;
use super::tree::*;
use super::*;

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
        source: Default::default(),
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
fn reverse_tree_distinguishes_same_name_receipt_versions() {
    let base = receipt("base", "1.0.0", false, &[]);
    let old_library = receipt("library", "1.0.0", false, &[("base", "1.0.0", DIGEST_A)]);
    let new_library = receipt("library", "2.0.0", false, &[("base", "1.0.0", DIGEST_A)]);
    let app = receipt("app", "1.0.0", true, &[("library", "1.0.0", DIGEST_A)]);
    let receipts = [old_library.clone(), new_library.clone(), app];

    assert_eq!(direct_dependents(&receipts, &old_library).len(), 1);
    assert!(direct_dependents(&receipts, &new_library).is_empty());
    let tree = build_dependent_tree(&receipts, &base);
    assert_eq!(tree.count_descendants(), 3);
    assert!(
        tree.children
            .iter()
            .all(|child| child.branch == Branch::Expanded)
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
