use super::*;

const FIXTURE: &str = r#"
        [[packages]]
        name = "sample"
        aliases = ["alias"]
        version = "2.0.0"
        executables = ["bin/sample"]
        required_files = ["share/sample/data"]
        [packages.artifacts.arm64]
        root = "sample-mac"
        url = "https://example.org/sample.tar.gz"
        sha256 = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
    "#;

#[test]
fn a_second_package_only_needs_catalog_data() {
    let catalog = Catalog::parse(&format!("{BUNDLED_TOML}\n{FIXTURE}")).unwrap();
    let sample = catalog.find("alias").unwrap();
    assert_eq!(sample.plan("arm64_sequoia").unwrap().name, "sample");
    assert!(sample.plan("sequoia").is_err());
    assert!(catalog.find("homebrew/core/sample").is_none());
}

#[test]
fn rejects_ambiguous_names_and_unsafe_catalog_fields() {
    for bad in [
        format!("{FIXTURE}\n{FIXTURE}"),
        FIXTURE.replace("bin/sample", "bin/../sample"),
        FIXTURE.replace("sample-mac", "../outside"),
        FIXTURE.replace("https:", "http:"),
        FIXTURE.replace("required_files", "unknown_key"),
    ] {
        assert!(Catalog::parse(&bad).is_err(), "accepted {bad}");
    }
}

#[test]
fn binds_plan_to_exact_artifact() {
    for package in &catalog().unwrap().packages {
        for (arch, tag) in [("arm64", "arm64_sequoia"), ("x86_64", "sequoia")] {
            if !package.artifacts.contains_key(arch) {
                continue;
            }
            let mut plan = package.plan(tag).unwrap();
            assert_eq!(artifact_for(&plan).unwrap().1, arch);
            plan.sha256.replace_range(..1, "z");
            assert!(artifact_for(&plan).is_err());
        }
    }
}

#[test]
fn upstream_source_plans_catalog_entries_and_rejects_unknown_names() {
    let catalog = catalog().unwrap();
    let plan = catalog
        .plan_targets(&["nvim".into(), "neovim".into()], "arm64_sequoia")
        .unwrap();
    assert_eq!(plan.len(), 1);
    assert_eq!(plan[0].name, "neovim");
    assert_eq!(plan[0].source, PackageSource::Upstream);
    let error = catalog
        .plan_targets(&["nosuchpackage".into()], "arm64_sequoia")
        .unwrap_err();
    assert_eq!(
        format!("{error:#}"),
        "No reviewed upstream build for 'nosuchpackage'.\n  Available upstream builds: neovim (nvim)"
    );
    assert!(
        catalog
            .plan_targets(&["homebrew/core/neovim".into()], "arm64_sequoia")
            .is_err()
    );
}

#[test]
fn upstream_source_requires_an_artifact_for_the_host() {
    let catalog = catalog().unwrap();
    // The bundled neovim entry ships arm64 only.
    assert!(catalog.plan_targets(&["neovim".into()], "sequoia").is_err());
    assert!(
        catalog
            .plan_targets(&["neovim".into()], "arm64_linux")
            .is_err()
    );
    assert!(!catalog.find("neovim").unwrap().supports("sequoia"));
    assert!(catalog.find("neovim").unwrap().supports("arm64_sequoia"));
}
