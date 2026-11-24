use super::*;

const TAG: &str = "arm64_sequoia";
const SHA: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

#[tokio::test]
async fn audit_memoizes_shared_dependencies_and_failures() {
    struct CountingSource {
        fixture: FixtureSource,
        names: std::cell::RefCell<Vec<String>>,
    }
    impl FormulaSource for CountingSource {
        async fn fetch(&self, name: &str) -> Result<FormulaResponse> {
            self.names.borrow_mut().push(name.into());
            self.fixture.fetch(name).await
        }
    }
    let source = Memoized::new(CountingSource {
        fixture: FixtureSource::new(vec![
            ("first", formula_json("first", "1.0", 0, &["shared"])),
            ("second", formula_json("second", "2.0", 0, &["shared"])),
            ("shared", formula_json("shared", "1.0", 0, &[])),
        ]),
        names: Default::default(),
    });
    for name in ["first", "second"] {
        let plan = resolve_with(&source, &[name.into()], TAG).await.unwrap();
        assert_eq!(plan[0].name, "shared");
    }
    for _ in 0..2 {
        assert!(
            resolve_with(&source, &["missing".into()], TAG)
                .await
                .is_err()
        );
    }
    assert_eq!(
        *source.source.names.borrow(),
        ["first", "shared", "second", "missing"]
    );
}

/// Reports every name outside `docs` as an API 404.
struct NotFoundSource(FixtureSource);

impl FormulaSource for NotFoundSource {
    async fn fetch(&self, name: &str) -> Result<FormulaResponse> {
        if self.0.docs.contains_key(name) {
            self.0.fetch(name).await
        } else {
            Err(anyhow::Error::new(ApiFailure::NotFound))
        }
    }
}

#[tokio::test]
async fn missing_target_and_missing_dependency_render_differently() {
    let source = NotFoundSource(FixtureSource::new(vec![(
        "parent",
        formula_json("parent", "1.0", 0, &["ghost"]),
    )]));
    let error = resolve_with(&source, &["tail".into()], TAG)
        .await
        .unwrap_err();
    assert_eq!(format!("{error:#}"), "No Homebrew formula named 'tail'.");
    let error = resolve_with(&source, &["homebrew/core/tail".into()], TAG)
        .await
        .unwrap_err();
    assert_eq!(
        format!("{error:#}"),
        "No formula named 'tail' in tap homebrew/core."
    );
    let error = resolve_with(&source, &["parent".into()], TAG)
        .await
        .unwrap_err();
    assert_eq!(
        format!("{error:#}"),
        "Homebrew metadata for 'parent' lists dependency 'ghost', which does not exist."
    );
}

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
        serde_json::from_str(&formula_json(name, version, 0, dependencies)).expect("fixture json");
    doc["bottle"]["stable"]["files"][TAG]["cellar"] = serde_json::json!(":any_skip_relocation");
    doc.to_string()
}

/// A minimal fixture carrying an explicit `requirements` array.
fn formula_with_requirements(name: &str, requirements: serde_json::Value, tag: &str) -> String {
    let mut doc: serde_json::Value = serde_json::from_str(&formula_json_for_tag(
        name,
        "1.0.0",
        0,
        &[],
        tag,
        ":any_skip_relocation",
    ))
    .expect("fixture json");
    doc["requirements"] = requirements;
    doc.to_string()
}

/// The requirement object the official API publishes for `go`.
fn macos_requirement(version: &str) -> serde_json::Value {
    serde_json::json!({
        "name": "macos",
        "cask": null,
        "download": null,
        "version": version,
        "contexts": [],
        "specs": ["stable", "head"]
    })
}

/// A fixture reproducing the official `go` payload for `tag`.
fn official_go_json(requirement: serde_json::Value, tag: &str) -> String {
    let file = serde_json::json!({
        "cellar": ":any_skip_relocation",
        "url": format!("https://ghcr.io/v2/homebrew/core/go/blobs/sha256:{SHA}"),
        "sha256": SHA,
    });
    let mut files = serde_json::Map::new();
    files.insert(tag.to_string(), file);
    serde_json::json!({
        "name": "go",
        "tap": CORE_TAP,
        "versions": { "stable": "1.27.4", "head": "HEAD", "bottle": true },
        "revision": 0,
        "version_scheme": 0,
        "keg_only": false,
        "post_install_defined": false,
        "dependencies": [],
        "build_dependencies": [],
        "recommended_dependencies": [],
        "uses_from_macos": [],
        "uses_from_macos_bounds": [],
        "conflicts_with": [],
        "service": null,
        "pour_bottle_only_if": null,
        "requirements": [requirement],
        "bottle": { "stable": { "files": files } },
        "variations": {
            "arm64_linux": { "requirements": [] },
            "x86_64_linux": { "requirements": [] }
        }
    })
    .to_string()
}

/// The exact service object the official API publishes for `colima`.
fn colima_service() -> serde_json::Value {
    serde_json::json!({
        "run": ["$HOMEBREW_PREFIX/opt/colima/bin/colima", "start", "-f"],
        "run_type": "immediate",
        "keep_alive": { "successful_exit": true },
        "environment_variables": {
            "PATH": "$HOMEBREW_PREFIX/bin:$HOMEBREW_PREFIX/sbin:/usr/bin:/bin:/usr/sbin:/sbin"
        },
        "working_dir": "/$HOME",
        "log_path": "$HOMEBREW_PREFIX/var/log/colima.log",
        "error_log_path": "$HOMEBREW_PREFIX/var/log/colima.log"
    })
}

/// A fixture reproducing the official `colima` payload for `tag`.
fn official_colima_json(service: serde_json::Value, tag: &str) -> String {
    let file = serde_json::json!({
        "cellar": ":any_skip_relocation",
        "url": format!("https://ghcr.io/v2/homebrew/core/colima/blobs/sha256:{SHA}"),
        "sha256": SHA,
    });
    let mut files = serde_json::Map::new();
    files.insert(tag.to_string(), file);
    serde_json::json!({
        "name": "colima",
        "tap": CORE_TAP,
        "versions": { "stable": "0.10.3", "head": "HEAD", "bottle": true },
        "revision": 0,
        "version_scheme": 0,
        "keg_only": false,
        "post_install_defined": false,
        "dependencies": ["lima"],
        "build_dependencies": ["go"],
        "recommended_dependencies": [],
        "uses_from_macos": [],
        "uses_from_macos_bounds": [],
        "conflicts_with": [],
        "requirements": [],
        "service": service,
        "pour_bottle_only_if": null,
        "bottle": { "stable": { "files": files } },
        "variations": {}
    })
    .to_string()
}

/// A dependency-free fixture carrying an explicit `service` block.
fn formula_with_service(name: &str, service: serde_json::Value, tag: &str) -> String {
    let mut doc: serde_json::Value = serde_json::from_str(&formula_json_for_tag(
        name,
        "1.0.0",
        0,
        &[],
        tag,
        ":any_skip_relocation",
    ))
    .expect("fixture json");
    doc["service"] = service;
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

/// A fixture carrying explicit `uses_from_macos` metadata, shaped exactly
/// like the official API arrays.
fn formula_with_uses_from_macos(
    name: &str,
    uses_from_macos: serde_json::Value,
    uses_from_macos_bounds: serde_json::Value,
    tag: &str,
    cellar: &str,
) -> String {
    let mut doc: serde_json::Value =
        serde_json::from_str(&formula_json_for_tag(name, "1.0.0", 0, &[], tag, cellar))
            .expect("fixture json");
    doc["uses_from_macos"] = uses_from_macos;
    doc["uses_from_macos_bounds"] = uses_from_macos_bounds;
    doc.to_string()
}

#[tokio::test]
async fn accepts_official_pcre2_system_bzip2_on_arm64_sequoia() {
    // Reproduces the live pcre2 10.49 payload for this host as closely as
    // the planner reads it: `uses_from_macos: ["bzip2"]`,
    // `uses_from_macos_bounds: [{}]`, an arm64_sequoia `:any` bottle, Linux
    // variations, and `head_dependencies` that also declare bzip2. No
    // arm64_sequoia variation exists.
    let sha = "0ec47d18c50f5770db2b3f07b5b85644284cc6ee8e07e23b11d2727e95932b77";
    let doc = serde_json::json!({
        "name": "pcre2",
        "tap": CORE_TAP,
        "versions": { "stable": "10.49", "head": "HEAD", "bottle": true },
        "revision": 0,
        "version_scheme": 0,
        "keg_only": false,
        "post_install_defined": false,
        "dependencies": [],
        "uses_from_macos": ["bzip2"],
        "uses_from_macos_bounds": [{}],
        "bottle": {
            "stable": {
                "files": {
                    "arm64_sequoia": {
                        "cellar": ":any",
                        "url": format!("https://ghcr.io/v2/homebrew/core/pcre2/blobs/sha256:{sha}"),
                        "sha256": sha,
                    }
                }
            }
        },
        "variations": {
            "x86_64_linux": { "dependencies": ["zlib-ng-compat"] },
            "arm64_linux": { "dependencies": ["zlib-ng-compat"] }
        },
        "head_dependencies": {
            "build_dependencies": ["autoconf", "automake", "libtool"],
            "uses_from_macos": ["bzip2"],
            "uses_from_macos_bounds": [{}]
        }
    })
    .to_string();
    let source = FixtureSource::new(vec![("pcre2", doc)]);

    let plan = resolve_with(&source, &["pcre2".into()], TAG)
        .await
        .expect("plan");

    assert_eq!(plan.len(), 1);
    assert_eq!(plan[0].name, "pcre2");
    assert_eq!(plan[0].version, "10.49");
    // macOS supplies bzip2 itself, so it must not become a planned formula.
    assert!(
        plan[0].dependencies.is_empty(),
        "system-supplied bzip2 must not be planned as a formula: {:?}",
        plan[0].dependencies
    );
    assert!(
        plan[0]
            .install_blockers
            .iter()
            .all(|blocker| !blocker.contains("uses_from_macos")),
        "plain system bzip2 must not keep a uses_from_macos blocker: {:?}",
        plan[0].install_blockers
    );
    assert_eq!(plan[0].cellar, ":any");
    assert!(plan[0].install_blockers.is_empty());
}

#[tokio::test]
async fn rejects_official_pcre2_system_bzip2_on_linux() {
    // On Linux Homebrew installs bzip2 rather than using the OS copy, and
    // this planner does not expand uses_from_macos, so the plan must stay
    // blocked. `:any_skip_relocation` isolates the platform decision.
    let doc = formula_with_uses_from_macos(
        "pcre2",
        serde_json::json!(["bzip2"]),
        serde_json::json!([{}]),
        "arm64_linux",
        ":any_skip_relocation",
    );
    let source = FixtureSource::new(vec![("pcre2", doc)]);

    let plan = resolve_with(&source, &["pcre2".into()], "arm64_linux")
        .await
        .expect("plan");

    assert_eq!(plan.len(), 1);
    assert!(plan[0].dependencies.is_empty());
    assert_eq!(plan[0].install_blockers, [BLOCKER_USES_FROM_MACOS_PLATFORM]);
}

#[tokio::test]
async fn rejects_uses_from_macos_dependency_outside_the_allowlist() {
    let doc = formula_with_uses_from_macos(
        "tool",
        serde_json::json!(["unknown-system-lib"]),
        serde_json::json!([{}]),
        TAG,
        ":any_skip_relocation",
    );
    let source = FixtureSource::new(vec![("tool", doc)]);

    let plan = resolve_with(&source, &["tool".into()], TAG)
        .await
        .expect("plan");

    assert_eq!(
        plan[0].install_blockers,
        [BLOCKER_USES_FROM_MACOS_DEPENDENCY]
    );
    assert!(plan[0].dependencies.is_empty());
}

#[tokio::test]
async fn accepts_no_linkage_and_verified_macos_uses_from_macos() {
    for (uses, bounds) in [
        (
            serde_json::json!([{"cups": "no_linkage"}]),
            serde_json::json!([{}]),
        ),
        (
            serde_json::json!([{"cups": ["no_linkage"]}]),
            serde_json::json!([{}]),
        ),
        (
            serde_json::json!(["krb5", "libxml2", "libxslt", "openldap", "python"]),
            serde_json::json!([{}, {"since": "ventura"}, {}, {}, {}]),
        ),
    ] {
        let doc = formula_with_uses_from_macos(
            "tool",
            uses.clone(),
            bounds.clone(),
            TAG,
            ":any_skip_relocation",
        );
        let plan = resolve_with(
            &FixtureSource::new(vec![("tool", doc)]),
            &["tool".into()],
            TAG,
        )
        .await
        .expect("plan");
        assert!(
            plan[0].install_blockers.is_empty(),
            "verified uses_from_macos {uses} should pass on {TAG}: {:?}",
            plan[0].install_blockers
        );
    }

    let bad_no_linkage = formula_with_uses_from_macos(
        "tool",
        serde_json::json!([{"unknown-system-lib": "no_linkage"}]),
        serde_json::json!([{}]),
        TAG,
        ":any_skip_relocation",
    );
    let plan = resolve_with(
        &FixtureSource::new(vec![("tool", bad_no_linkage)]),
        &["tool".into()],
        TAG,
    )
    .await
    .expect("plan");
    assert_eq!(
        plan[0].install_blockers,
        [BLOCKER_USES_FROM_MACOS_DEPENDENCY]
    );

    let linux_no_linkage = formula_with_uses_from_macos(
        "tool",
        serde_json::json!([{"cups": "no_linkage"}]),
        serde_json::json!([{}]),
        "arm64_linux",
        ":any_skip_relocation",
    );
    let plan = resolve_with(
        &FixtureSource::new(vec![("tool", linux_no_linkage)]),
        &["tool".into()],
        "arm64_linux",
    )
    .await
    .expect("plan");
    assert_eq!(plan[0].install_blockers, [BLOCKER_USES_FROM_MACOS_PLATFORM]);
}

#[tokio::test]
async fn accepts_general_any_relocatable_bottles_on_macos_only() {
    for tag in [TAG, "sequoia"] {
        let doc = formula_json_for_tag("zstd", "1.5.7_1", 0, &[], tag, ":any");
        let plan = resolve_with(
            &FixtureSource::new(vec![("zstd", doc)]),
            &["zstd".into()],
            tag,
        )
        .await
        .expect("plan");
        assert!(
            plan[0].install_blockers.is_empty(),
            ":any formula on {tag} should have no blockers: {:?}",
            plan[0].install_blockers
        );
    }
    for tag in ["arm64_linux", "x86_64_linux"] {
        let doc = formula_json_for_tag("zstd", "1.5.7_1", 0, &[], tag, ":any");
        let plan = resolve_with(
            &FixtureSource::new(vec![("zstd", doc)]),
            &["zstd".into()],
            tag,
        )
        .await
        .expect("plan");
        assert_eq!(
            plan[0].install_blockers,
            ["requires bottle relocation not yet supported for this formula or platform"],
            ":any on {tag} must keep relocation blocker"
        );
    }
}

#[tokio::test]
async fn rejects_version_bounded_uses_from_macos() {
    for (lib, bounds, tag) in [
        ("bzip2", serde_json::json!([{ "since": "catalina" }]), TAG),
        ("expat", serde_json::json!([{ "since": "sequoia" }]), TAG),
        ("ncurses", serde_json::json!([{}]), TAG),
        ("libffi", serde_json::json!([{}]), TAG),
    ] {
        let doc = formula_with_uses_from_macos(
            "tool",
            serde_json::json!([lib]),
            bounds.clone(),
            tag,
            ":any_skip_relocation",
        );
        let plan = resolve_with(
            &FixtureSource::new(vec![("tool", doc)]),
            &["tool".into()],
            tag,
        )
        .await
        .expect("plan");
        assert!(
            plan[0].install_blockers.is_empty(),
            "{lib} with bounds {bounds} on {tag} should be accepted: {:?}",
            plan[0].install_blockers
        );
    }

    for (tag, bounds) in [
        (TAG, serde_json::json!([{ "since": "tahoe" }])),
        ("arm64_sonoma", serde_json::json!([{ "since": "sequoia" }])),
        (TAG, serde_json::json!([{ "before": "ventura" }])),
        (
            TAG,
            serde_json::json!([{ "since": "sonoma", "before": "tahoe" }]),
        ),
        (TAG, serde_json::json!([{ "since": "unknown_os" }])),
    ] {
        let doc = formula_with_uses_from_macos(
            "expat",
            serde_json::json!(["expat"]),
            bounds.clone(),
            tag,
            ":any_skip_relocation",
        );
        let source = FixtureSource::new(vec![("expat", doc)]);

        let plan = resolve_with(&source, &["expat".into()], tag)
            .await
            .expect("plan");

        assert_eq!(
            plan[0].install_blockers,
            [BLOCKER_USES_FROM_MACOS_SCOPING],
            "bounds {bounds} on {tag} must stay blocked: {:?}",
            plan[0].install_blockers
        );
    }
}

#[tokio::test]
async fn accepts_build_and_test_scoped_uses_from_macos_for_bottles() {
    for (uses_from_macos, bounds) in [
        (
            serde_json::json!([{ "bzip2": "build" }]),
            serde_json::json!([{}]),
        ),
        (
            serde_json::json!([{ "bzip2": ["build", "test"] }]),
            serde_json::json!([{}]),
        ),
        (
            serde_json::json!([{ "llvm": "build" }, { "python@3.13": "test" }]),
            serde_json::json!([{ "since": "monterey" }, { "before": "sequoia" }]),
        ),
    ] {
        let doc = formula_with_uses_from_macos(
            "tool",
            uses_from_macos.clone(),
            bounds.clone(),
            TAG,
            ":any_skip_relocation",
        );
        let source = FixtureSource::new(vec![("tool", doc)]);

        let plan = resolve_with(&source, &["tool".into()], TAG)
            .await
            .expect("plan");

        assert!(
            plan[0].install_blockers.is_empty(),
            "build/test uses_from_macos {uses_from_macos} / {bounds} must not block bottles: {:?}",
            plan[0].install_blockers
        );
    }
}

#[tokio::test]
async fn rejects_non_build_tagged_uses_from_macos() {
    for (uses_from_macos, bounds) in [
        (
            serde_json::json!([{ "bzip2": "recommended" }]),
            serde_json::json!([{}]),
        ),
        (
            serde_json::json!([{ "bzip2": "optional" }]),
            serde_json::json!([{}]),
        ),
        (
            serde_json::json!([{ "bzip2": ["build", "recommended"] }]),
            serde_json::json!([{}]),
        ),
        (
            serde_json::json!([{ "bzip2": [] }]),
            serde_json::json!([{}]),
        ),
        (
            serde_json::json!([{ "a": "build", "b": "test" }]),
            serde_json::json!([{}]),
        ),
        (
            serde_json::json!([{ "bzip2": "build" }]),
            serde_json::json!([{ "since": "unknown_os" }]),
        ),
    ] {
        let doc = formula_with_uses_from_macos(
            "tool",
            uses_from_macos.clone(),
            bounds.clone(),
            TAG,
            ":any_skip_relocation",
        );
        let source = FixtureSource::new(vec![("tool", doc)]);

        let plan = resolve_with(&source, &["tool".into()], TAG)
            .await
            .expect("plan");

        assert!(
            plan[0]
                .install_blockers
                .iter()
                .any(|blocker| blocker.contains("uses_from_macos")),
            "non-build tagged uses_from_macos {uses_from_macos} / {bounds} must stay blocked: {:?}",
            plan[0].install_blockers
        );
    }
}

#[tokio::test]
async fn accepts_version_scheme_keg_only_and_valid_conflicts() {
    let mut doc: serde_json::Value =
        serde_json::from_str(&portable_formula_json("m4", "1.4.21", &[])).unwrap();
    doc["version_scheme"] = serde_json::json!(2);
    doc["keg_only"] = serde_json::json!(true);
    doc["conflicts_with"] = serde_json::json!(["gnu-m4", "alt-m4", "gnu-m4"]);
    let plan = resolve_with(
        &FixtureSource::new(vec![("m4", doc.to_string())]),
        &["m4".into()],
        TAG,
    )
    .await
    .expect("plan");

    assert!(
        plan[0].install_blockers.is_empty(),
        "{:?}",
        plan[0].install_blockers
    );
    assert_eq!(plan[0].version_scheme, Some(2));
    assert!(plan[0].keg_only);
    assert_eq!(plan[0].conflicts_with, ["alt-m4", "gnu-m4"]);
}

#[tokio::test]
async fn blocks_missing_version_scheme_keg_only_and_invalid_conflicts() {
    let mut missing_scheme: serde_json::Value =
        serde_json::from_str(&portable_formula_json("tool", "1.0.0", &[])).unwrap();
    missing_scheme
        .as_object_mut()
        .unwrap()
        .remove("version_scheme");
    let plan = resolve_with(
        &FixtureSource::new(vec![("tool", missing_scheme.to_string())]),
        &["tool".into()],
        TAG,
    )
    .await
    .expect("plan");
    assert_eq!(
        plan[0].install_blockers,
        ["does not declare a Homebrew version scheme"]
    );

    let mut missing_keg_only: serde_json::Value =
        serde_json::from_str(&portable_formula_json("tool", "1.0.0", &[])).unwrap();
    missing_keg_only.as_object_mut().unwrap().remove("keg_only");
    let plan = resolve_with(
        &FixtureSource::new(vec![("tool", missing_keg_only.to_string())]),
        &["tool".into()],
        TAG,
    )
    .await
    .expect("plan");
    assert_eq!(
        plan[0].install_blockers,
        ["does not declare keg-only status"]
    );

    for bad_conflicts in [serde_json::json!(["../evil"]), serde_json::json!(["tool"])] {
        let mut bad: serde_json::Value =
            serde_json::from_str(&portable_formula_json("tool", "1.0.0", &[])).unwrap();
        bad["conflicts_with"] = bad_conflicts;
        let plan = resolve_with(
            &FixtureSource::new(vec![("tool", bad.to_string())]),
            &["tool".into()],
            TAG,
        )
        .await
        .expect("plan");
        assert_eq!(
            plan[0].install_blockers,
            ["declares an unsupported conflict rule"]
        );
    }
}

#[tokio::test]
async fn rejects_uses_from_macos_metadata_whose_arrays_disagree() {
    for (uses_from_macos, bounds) in [
        (serde_json::json!(["bzip2"]), serde_json::json!([])),
        (serde_json::json!([]), serde_json::json!([{}])),
        (
            serde_json::json!(["bzip2", "bzip2"]),
            serde_json::json!([{}]),
        ),
    ] {
        let doc = formula_with_uses_from_macos(
            "pcre2",
            uses_from_macos.clone(),
            bounds.clone(),
            TAG,
            ":any_skip_relocation",
        );
        let source = FixtureSource::new(vec![("pcre2", doc)]);

        let plan = resolve_with(&source, &["pcre2".into()], TAG)
            .await
            .expect("plan");

        assert_eq!(
            plan[0].install_blockers,
            [BLOCKER_USES_FROM_MACOS_SHAPE],
            "mismatched arrays {uses_from_macos} / {bounds} must stay blocked: {:?}",
            plan[0].install_blockers
        );
    }
}

#[tokio::test]
async fn rejects_unexpected_uses_from_macos_json_shapes() {
    for uses_from_macos in [
        serde_json::json!([42]),
        serde_json::json!([null]),
        serde_json::json!([["bzip2"]]),
        serde_json::json!([{ "name": "bzip2" }]),
    ] {
        let doc = formula_with_uses_from_macos(
            "pcre2",
            uses_from_macos.clone(),
            serde_json::json!([{}]),
            TAG,
            ":any_skip_relocation",
        );
        let source = FixtureSource::new(vec![("pcre2", doc)]);

        let plan = resolve_with(&source, &["pcre2".into()], TAG)
            .await
            .expect("plan");

        assert!(
            plan[0]
                .install_blockers
                .iter()
                .any(|blocker| blocker.contains("uses_from_macos")),
            "unrecognized uses_from_macos {uses_from_macos} must stay blocked: {:?}",
            plan[0].install_blockers
        );
    }
}

#[tokio::test]
async fn rejects_uses_from_macos_on_an_unverified_macos_tag() {
    // A macOS release this planner has not verified keeps the platform
    // blocked instead of assuming the system library exists.
    let tag = "arm64_futureos";
    let doc = formula_with_uses_from_macos(
        "pcre2",
        serde_json::json!(["bzip2"]),
        serde_json::json!([{}]),
        tag,
        ":any_skip_relocation",
    );
    let source = FixtureSource::new(vec![("pcre2", doc)]);

    let plan = resolve_with(&source, &["pcre2".into()], tag)
        .await
        .expect("plan");

    assert_eq!(plan[0].install_blockers, [BLOCKER_USES_FROM_MACOS_PLATFORM]);
}

#[tokio::test]
async fn recommended_dependencies_keep_their_own_blocker() {
    let mut doc: serde_json::Value =
        serde_json::from_str(&portable_formula_json("tool", "1.0.0", &[])).expect("fixture json");
    doc["recommended_dependencies"] = serde_json::json!(["recommended-lib"]);
    let source = FixtureSource::new(vec![("tool", doc.to_string())]);

    let plan = resolve_with(&source, &["tool".into()], TAG)
        .await
        .expect("plan");

    assert_eq!(plan.len(), 1);
    assert!(plan[0].dependencies.is_empty());
    assert_eq!(plan[0].install_blockers, [BLOCKER_RECOMMENDED_DEPENDENCY]);
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
            .any(|blocker| blocker.contains("fixed Homebrew prefix")),
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

#[test]
fn parse_macos_version_accepts_numeric_releases_and_rejects_malformed() {
    assert_eq!(
        parse_macos_version("10.15"),
        Some(MacosVersion {
            major: 10,
            minor: 15,
            patch: 0
        })
    );
    assert_eq!(
        parse_macos_version("12"),
        Some(MacosVersion {
            major: 12,
            minor: 0,
            patch: 0
        })
    );
    assert_eq!(
        parse_macos_version("14.2.1"),
        Some(MacosVersion {
            major: 14,
            minor: 2,
            patch: 1
        })
    );
    assert_eq!(
        parse_macos_version("26"),
        Some(MacosVersion {
            major: 26,
            minor: 0,
            patch: 0
        })
    );
    for bad in [
        "",
        "9",
        "012",
        "12.01",
        "12.0.0.1",
        "monterey",
        "12.",
        ".12",
        "1234",
        "12.0.0.0.0.0.0.0",
    ] {
        assert_eq!(parse_macos_version(bad), None, "{bad:?} must be rejected");
    }
}

#[tokio::test]
async fn accepts_official_go_macos_requirement_on_supported_hosts() {
    for tag in [TAG, "monterey", "arm64_monterey"] {
        let doc = official_go_json(macos_requirement("12"), tag);
        let source = FixtureSource::new(vec![("go", doc)]);
        let plan = resolve_with(&source, &["go".into()], tag)
            .await
            .expect("plan");
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].name, "go");
        assert!(
            plan[0].install_blockers.is_empty(),
            "go on {tag} should have no blockers: {:?}",
            plan[0].install_blockers
        );
    }
}

#[tokio::test]
async fn rejects_macos_requirement_on_older_release_and_non_macos_hosts() {
    let older_doc = official_go_json(macos_requirement("12"), "big_sur");
    let older_plan = resolve_with(
        &FixtureSource::new(vec![("go", older_doc)]),
        &["go".into()],
        "big_sur",
    )
    .await
    .expect("plan");
    assert_eq!(
        older_plan[0].install_blockers,
        [BLOCKER_REQUIREMENTS_VERSION]
    );

    let linux_tool = formula_with_requirements(
        "tool",
        serde_json::json!([macos_requirement("12")]),
        "arm64_linux",
    );
    let linux_plan = resolve_with(
        &FixtureSource::new(vec![("tool", linux_tool)]),
        &["tool".into()],
        "arm64_linux",
    )
    .await
    .expect("plan");
    assert_eq!(
        linux_plan[0].install_blockers,
        [BLOCKER_REQUIREMENTS_PLATFORM]
    );

    let go_linux = official_go_json(macos_requirement("12"), "arm64_linux");
    let go_linux_plan = resolve_with(
        &FixtureSource::new(vec![("go", go_linux)]),
        &["go".into()],
        "arm64_linux",
    )
    .await
    .expect("plan");
    assert!(
        go_linux_plan[0]
            .install_blockers
            .contains(&BLOCKER_REQUIREMENTS_PLATFORM),
        "{:?}",
        go_linux_plan[0].install_blockers
    );
}

#[tokio::test]
async fn rejects_unsupported_requirement_shapes() {
    let xcode_req = serde_json::json!([{
        "name": "xcode",
        "cask": null,
        "download": null,
        "version": "16.0",
        "contexts": [],
        "specs": ["stable"]
    }]);
    for tag in [TAG, "arm64_linux"] {
        let doc = formula_with_requirements("tool", xcode_req.clone(), tag);
        let plan = resolve_with(
            &FixtureSource::new(vec![("tool", doc)]),
            &["tool".into()],
            tag,
        )
        .await
        .expect("plan");
        assert_eq!(
            plan[0].install_blockers,
            [BLOCKER_REQUIREMENTS_SHAPE],
            "xcode requirement on {tag} must keep shape blocker"
        );
    }

    let mut bad_cases: Vec<(&str, serde_json::Value)> = vec![
        ("not an object", serde_json::json!(["macos"])),
        ("non-null cask", {
            let mut r = macos_requirement("12");
            r["cask"] = serde_json::json!("x");
            serde_json::json!([r])
        }),
        ("non-null download", {
            let mut r = macos_requirement("12");
            r["download"] = serde_json::json!("x");
            serde_json::json!([r])
        }),
        ("non-empty unsupported contexts", {
            let mut r = macos_requirement("12");
            r["contexts"] = serde_json::json!(["optional"]);
            serde_json::json!([r])
        }),
        ("empty specs", {
            let mut r = macos_requirement("12");
            r["specs"] = serde_json::json!([]);
            serde_json::json!([r])
        }),
        ("head-only specs", {
            let mut r = macos_requirement("12");
            r["specs"] = serde_json::json!(["head"]);
            serde_json::json!([r])
        }),
        ("extra specs", {
            let mut r = macos_requirement("12");
            r["specs"] = serde_json::json!(["stable", "head", "extra"]);
            serde_json::json!([r])
        }),
        ("extra key", {
            let mut r = macos_requirement("12");
            r["comparator"] = serde_json::json!(">=");
            serde_json::json!([r])
        }),
        ("missing key", {
            let mut r = macos_requirement("12");
            r.as_object_mut().unwrap().remove("contexts");
            serde_json::json!([r])
        }),
        (
            "too many requirements",
            serde_json::Value::Array(vec![macos_requirement("12"); 9]),
        ),
    ];
    for bad_ver in ["", "9", "012", "12.01", "12.0.0.1", "monterey"] {
        bad_cases.push((bad_ver, serde_json::json!([macos_requirement(bad_ver)])));
    }

    for (label, reqs) in bad_cases {
        let doc = formula_with_requirements("tool", reqs, TAG);
        let plan = resolve_with(
            &FixtureSource::new(vec![("tool", doc)]),
            &["tool".into()],
            TAG,
        )
        .await
        .expect("plan");
        assert_eq!(
            plan[0].install_blockers,
            [BLOCKER_REQUIREMENTS_SHAPE],
            "case {label:?} must return shape blocker: {:?}",
            plan[0].install_blockers
        );
    }
}

#[tokio::test]
async fn accepts_any_version_macos_requirement_on_macos_only() {
    let any_macos_req = serde_json::json!([{
        "name": "macos",
        "cask": null,
        "download": null,
        "version": null,
        "contexts": [],
        "specs": ["stable", "head"]
    }]);
    for tag in [TAG, "sequoia", "big_sur"] {
        let doc = formula_with_requirements("screenresolution", any_macos_req.clone(), tag);
        let plan = resolve_with(
            &FixtureSource::new(vec![("screenresolution", doc)]),
            &["screenresolution".into()],
            tag,
        )
        .await
        .expect("plan");
        assert!(
            plan[0].install_blockers.is_empty(),
            "version:null macos req on {tag} should pass: {:?}",
            plan[0].install_blockers
        );
    }

    let linux_doc = formula_with_requirements("screenresolution", any_macos_req, "arm64_linux");
    let linux_plan = resolve_with(
        &FixtureSource::new(vec![("screenresolution", linux_doc)]),
        &["screenresolution".into()],
        "arm64_linux",
    )
    .await
    .expect("plan");
    assert_eq!(
        linux_plan[0].install_blockers,
        [BLOCKER_REQUIREMENTS_PLATFORM]
    );

    for bad_version in [serde_json::json!(12), serde_json::json!(true)] {
        let bad_req = serde_json::json!([{
            "name": "macos",
            "cask": null,
            "download": null,
            "version": bad_version,
            "contexts": [],
            "specs": ["stable"]
        }]);
        let doc = formula_with_requirements("screenresolution", bad_req, TAG);
        let plan = resolve_with(
            &FixtureSource::new(vec![("screenresolution", doc)]),
            &["screenresolution".into()],
            TAG,
        )
        .await
        .expect("plan");
        assert_eq!(plan[0].install_blockers, [BLOCKER_REQUIREMENTS_SHAPE]);
    }
}

#[tokio::test]
async fn accepts_arch_and_build_only_requirements_and_rejects_mismatch() {
    let arch_req = |arch: &str| {
        serde_json::json!([{
            "name": "arch",
            "cask": null,
            "download": null,
            "version": arch,
            "contexts": [],
            "specs": ["stable"]
        }])
    };

    for (tag, required) in [
        (TAG, "arm64"),
        ("sequoia", "x86_64"),
        ("x86_64_linux", "x86_64"),
        ("arm64_linux", "arm64"),
    ] {
        let doc = formula_with_requirements("tool", arch_req(required), tag);
        let plan = resolve_with(
            &FixtureSource::new(vec![("tool", doc)]),
            &["tool".into()],
            tag,
        )
        .await
        .expect("plan");
        assert!(
            plan[0].install_blockers.is_empty(),
            "arch {required} on {tag} should pass: {:?}",
            plan[0].install_blockers
        );
    }

    for (tag, required) in [
        (TAG, "x86_64"),
        ("sequoia", "arm64"),
        ("x86_64_linux", "arm64"),
    ] {
        let doc = formula_with_requirements("tool", arch_req(required), tag);
        let plan = resolve_with(
            &FixtureSource::new(vec![("tool", doc)]),
            &["tool".into()],
            tag,
        )
        .await
        .expect("plan");
        assert_eq!(
            plan[0].install_blockers,
            [BLOCKER_REQUIREMENTS_ARCH],
            "arch {required} mismatch on {tag}"
        );
    }

    for bad_ver in [serde_json::json!("ppc"), serde_json::Value::Null] {
        let doc = formula_with_requirements(
            "tool",
            serde_json::json!([{
                "name": "arch",
                "cask": null,
                "download": null,
                "version": bad_ver,
                "contexts": [],
                "specs": ["stable"]
            }]),
            TAG,
        );
        let plan = resolve_with(
            &FixtureSource::new(vec![("tool", doc)]),
            &["tool".into()],
            TAG,
        )
        .await
        .expect("plan");
        assert_eq!(plan[0].install_blockers, [BLOCKER_REQUIREMENTS_SHAPE]);
    }

    let unknown_tag_doc = formula_with_requirements("tool", arch_req("arm64"), "unknown_tag");
    let unknown_plan = resolve_with(
        &FixtureSource::new(vec![("tool", unknown_tag_doc)]),
        &["tool".into()],
        "unknown_tag",
    )
    .await
    .expect("plan");
    assert_eq!(
        unknown_plan[0].install_blockers,
        [BLOCKER_REQUIREMENTS_PLATFORM]
    );

    for contexts in [
        serde_json::json!(["build"]),
        serde_json::json!(["test"]),
        serde_json::json!(["build", "test"]),
    ] {
        for tag in [TAG, "arm64_linux"] {
            let doc = formula_with_requirements(
                "openjdk@21",
                serde_json::json!([{
                    "name": "xcode",
                    "cask": null,
                    "download": null,
                    "version": null,
                    "contexts": contexts,
                    "specs": ["stable"]
                }]),
                tag,
            );
            let plan = resolve_with(
                &FixtureSource::new(vec![("openjdk@21", doc)]),
                &["openjdk@21".into()],
                tag,
            )
            .await
            .expect("plan");
            assert!(
                plan[0].install_blockers.is_empty(),
                "build/test xcode requirement on {tag} should pass: {:?}",
                plan[0].install_blockers
            );
        }
    }
}

#[tokio::test]
async fn accepts_official_colima_optional_service_on_macos() {
    let source = FixtureSource::new(vec![
        ("colima", official_colima_json(colima_service(), TAG)),
        ("lima", portable_formula_json("lima", "2.3.0", &[])),
    ]);
    let plan = resolve_with(&source, &["colima".into()], TAG)
        .await
        .expect("plan");
    let names: Vec<&str> = plan.iter().map(|f| f.name.as_str()).collect();
    assert_eq!(names, ["lima", "colima"]);
    assert!(!plan[0].has_service);
    assert!(plan[1].has_service);
    assert!(
        plan.iter().all(|f| f.install_blockers.is_empty()),
        "colima and lima must have no blockers: {:?}",
        plan.iter()
            .map(|f| (&f.name, &f.install_blockers))
            .collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn accepts_valid_optional_services_on_macos_and_rejects_invalid_or_linux() {
    let valid_services = [
        colima_service(),
        serde_json::json!({
            "run": ["$HOMEBREW_PREFIX/opt/zellij/bin/zellij", "web"],
            "run_type": "immediate",
            "keep_alive": { "always": true },
            "environment_variables": { "PATH": "$HOMEBREW_PREFIX/bin:/usr/bin" },
            "log_path": "$HOMEBREW_PREFIX/var/log/zellij.log",
            "error_log_path": "$HOMEBREW_PREFIX/var/log/zellij.log"
        }),
        serde_json::json!({
            "name": { "macos": "homebrew.mxcl.redis" },
            "run": { "macos": ["$HOMEBREW_PREFIX/opt/redis/bin/redis-server"] },
            "run_type": "immediate",
            "working_dir": "$HOMEBREW_PREFIX/var"
        }),
        serde_json::json!({ "name": "simple-daemon" }),
    ];
    for service in valid_services {
        let doc = formula_with_service("svc", service.clone(), TAG);
        let plan = resolve_with(
            &FixtureSource::new(vec![("svc", doc)]),
            &["svc".into()],
            TAG,
        )
        .await
        .expect("plan");
        assert!(
            plan[0].install_blockers.is_empty(),
            "valid macOS service {service} must not block: {:?}",
            plan[0].install_blockers
        );
        assert!(plan[0].has_service);
    }

    let linux_colima = official_colima_json(colima_service(), "arm64_linux");
    let linux_lima = formula_json_for_tag(
        "lima",
        "2.3.0",
        0,
        &[],
        "arm64_linux",
        ":any_skip_relocation",
    );
    let linux_plan = resolve_with(
        &FixtureSource::new(vec![("colima", linux_colima), ("lima", linux_lima)]),
        &["colima".into()],
        "arm64_linux",
    )
    .await
    .expect("plan");
    assert_eq!(
        linux_plan
            .iter()
            .find(|f| f.name == "colima")
            .unwrap()
            .install_blockers,
        [BLOCKER_SERVICE]
    );

    let mutations: Vec<(&str, serde_json::Value)> = vec![
        ("non-object", serde_json::json!(true)),
        ("empty object", serde_json::json!({})),
        ("extra key", {
            let mut s = colima_service();
            s["extra"] = serde_json::json!(1);
            s
        }),
        (
            "missing run and name",
            serde_json::json!({ "run_type": "immediate" }),
        ),
        ("empty run array", serde_json::json!({ "run": [] })),
        ("non-string run", serde_json::json!({ "run": 42 })),
        (
            "unsupported os run map",
            serde_json::json!({ "run": { "windows": "svc.exe" } }),
        ),
    ];

    for (label, service) in mutations {
        let source = FixtureSource::new(vec![
            ("colima", official_colima_json(service, TAG)),
            ("lima", portable_formula_json("lima", "2.3.0", &[])),
        ]);
        let plan = resolve_with(&source, &["colima".into()], TAG)
            .await
            .expect("plan");
        let colima = plan.iter().find(|f| f.name == "colima").unwrap();
        assert_eq!(
            colima.install_blockers,
            [BLOCKER_SERVICE],
            "service mutation {label:?} must stay blocked"
        );
    }

    let mut pour_doc: serde_json::Value =
        serde_json::from_str(&portable_formula_json("tool", "1.0.0", &[])).unwrap();
    pour_doc["pour_bottle_only_if"] = serde_json::json!("clt_installed");
    let pour_plan = resolve_with(
        &FixtureSource::new(vec![("tool", pour_doc.to_string())]),
        &["tool".into()],
        TAG,
    )
    .await
    .expect("plan");
    assert_eq!(pour_plan[0].install_blockers, [BLOCKER_POUR_BOTTLE_ONLY]);
}

#[test]
fn recognizes_actual_macos_bottle_tags() {
    assert!(is_supported_macos_tag("sequoia"));
    assert!(is_supported_macos_tag("arm64_sequoia"));
    assert!(!is_supported_macos_tag("x86_64_linux"));
    assert!(!is_supported_macos_tag("arm64_linux"));
}

#[test]
fn macos_platform_keys_match_homebrew() {
    assert_eq!(macos_tag("aarch64", "15.7.1").unwrap(), "arm64_sequoia");
    assert_eq!(macos_tag("x86_64", "15.7").unwrap(), "sequoia");
    assert_eq!(macos_tag("aarch64", "26.1").unwrap(), "arm64_tahoe");
    assert!(macos_tag("aarch64", "27.0").is_err());
}
