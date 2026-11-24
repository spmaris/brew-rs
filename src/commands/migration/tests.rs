use super::*;

fn formula(name: &str, requested: bool, install_blockers: Vec<&'static str>) -> PlannedFormula {
    PlannedFormula {
        source: Default::default(),
        name: name.into(),
        version: "1.2".into(),
        version_scheme: Some(0),
        keg_only: false,
        has_service: false,
        caveats: None,
        conflicts_with: Vec::new(),
        requested,
        dependencies: Vec::new(),
        url: String::new(),
        sha256: String::new(),
        cellar: ":any_skip_relocation".into(),
        install_blockers,
    }
}

#[test]
fn assessment_propagates_dependency_blockers_and_distinguishes_candidates() {
    let target = formula("app", true, vec![]);
    let dep = formula("lib", false, vec![]);
    assert_eq!(
        assess_plan(&[dep.clone(), target.clone()]),
        Assessment::Candidate {
            version: "1.2".into(),
            dependencies: 1
        }
    );
    assert_eq!(
        assess_plan(&[formula("lib", false, vec!["requires relocation"]), target]),
        Assessment::Blocked("lib: requires relocation".into())
    );
    assert!(matches!(assess_plan(&[dep]), Assessment::Unresolved(_)));
}

#[test]
fn overlapping_source_and_destination_are_rejected_before_access() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("homebrew");
    assert!(ensure_separated(&source, &source.join("native")).is_err());
    assert!(ensure_separated(&source, temp.path()).is_err());
    ensure_separated(&source, &temp.path().join("native")).unwrap();
    assert!(!source.exists());
}

#[test]
fn unsafe_terminal_bytes_render_as_text() {
    assert_eq!(clean("a\x1b[31mb\n"), "a�[31mb�");
    assert_eq!(clean(&"a".repeat(513)).chars().count(), 513);
    assert_eq!(intention(None), "Unknown");
}

#[test]
fn assess_cask_plan_distinguishes_candidates_and_blockers() {
    use crate::domain::cask_plan::{CaskSourcePath, ContainerHint, PlannedCaskArtifact};

    let candidate = PlannedCask {
        token: "claude-code@latest".into(),
        name: vec!["Claude Code".into()],
        version: "2.1.330".into(),
        auto_updates: false,
        url: "https://example.com/claude".into(),
        sha256: "a".repeat(64),
        container: ContainerHint::Auto,
        artifacts: vec![PlannedCaskArtifact::Binary {
            source: CaskSourcePath::Payload("claude".into()),
            link_name: "claude".into(),
        }],
        conflicts_with_casks: Vec::new(),
        conflicts_with_formulae: Vec::new(),
        install_blockers: Vec::new(),
    };
    assert_eq!(
        assess_cask_plan(&candidate),
        Assessment::CaskCandidate {
            version: "2.1.330".into(),
            artifacts: 1,
            auto_updates: false,
        }
    );

    let mut blocked = candidate;
    blocked.install_blockers.push("sha256: no_check".into());
    assert_eq!(
        assess_cask_plan(&blocked),
        Assessment::Blocked("sha256: no_check".into())
    );
}
