use super::*;

#[test]
fn package_count_wording() {
    assert_eq!(package_count(1, 0), "1 package");
    assert_eq!(package_count(2, 0), "2 packages");
    assert_eq!(
        package_count(1, 1),
        "2 packages (1 requested, 1 dependency)"
    );
    assert_eq!(
        package_count(1, 3),
        "4 packages (1 requested, 3 dependencies)"
    );
    assert_eq!(
        package_count(2, 1),
        "3 packages (2 requested, 1 dependency)"
    );
}

#[test]
fn install_question_covers_installs_promotions_and_both() {
    assert_eq!(install_question(1, 0), "Install 1 package?");
    assert_eq!(install_question(2, 0), "Install 2 packages?");
    assert_eq!(install_question(0, 1), "Mark 1 package as requested?");
    assert_eq!(install_question(0, 2), "Mark 2 packages as requested?");
    assert_eq!(
        install_question(2, 1),
        "Install 2 packages and mark 1 as requested?"
    );
    assert_eq!(
        install_question(1, 3),
        "Install 1 package and mark 3 as requested?"
    );
}

#[test]
fn limited_list_keeps_four_and_summarises_the_rest() {
    assert_eq!(limited_list(&[]), "");
    assert_eq!(limited_list(&["jq 1.8.2".to_string()]), "jq 1.8.2");
    let four = ["a 1", "b 1", "c 1", "d 1"].map(str::to_string);
    assert_eq!(limited_list(&four), "a 1, b 1, c 1, d 1");
    let five = ["a 1", "b 1", "c 1", "d 1", "e 1"].map(str::to_string);
    assert_eq!(limited_list(&five), "a 1, b 1, c 1, d 1, and 1 more");
    let seven = ["a 1", "b 1", "c 1", "d 1", "e 1", "f 1", "g 1"].map(str::to_string);
    assert_eq!(limited_list(&seven), "a 1, b 1, c 1, d 1, and 3 more");
}

#[test]
fn installed_summary_names_requested_formulae_and_counts_dependencies() {
    assert_eq!(
        installed_summary(&["jq 1.8.2".to_string()], 1),
        "Installed jq 1.8.2 (+1 dependency)"
    );
    assert_eq!(
        installed_summary(&["jq 1.8.2".to_string(), "curl 8.0".to_string()], 0),
        "Installed jq 1.8.2, curl 8.0"
    );
    assert_eq!(installed_summary(&[], 2), "Installed 2 packages");
    assert_eq!(installed_summary(&[], 1), "Installed 1 package");
    let many = ["a 1", "b 1", "c 1", "d 1", "e 1"].map(str::to_string);
    assert_eq!(
        installed_summary(&many, 2),
        "Installed a 1, b 1, c 1, d 1, and 1 more (+2 dependencies)"
    );
}

#[test]
fn upgraded_summary_names_old_and_new_versions() {
    assert_eq!(
        upgraded_summary(&["jq 1.8.1 → 1.8.2".to_string()]),
        "Upgraded jq 1.8.1 → 1.8.2"
    );
    let many = ["a 1 → 2", "b 1 → 2", "c 1 → 2", "d 1 → 2", "e 1 → 2"].map(str::to_string);
    assert_eq!(
        upgraded_summary(&many),
        "Upgraded a 1 → 2, b 1 → 2, c 1 → 2, d 1 → 2, and 1 more"
    );
}

#[test]
fn removed_summary_counts_unused_dependencies() {
    assert_eq!(
        removed_summary(&["jq".to_string()], &["oniguruma".to_string()]),
        "Removed jq (+1 unused dependency)"
    );
    assert_eq!(
        removed_summary(&["jq".to_string()], &["a".to_string(), "b".to_string()]),
        "Removed jq (+2 unused dependencies)"
    );
    assert_eq!(removed_summary(&["jq".to_string()], &[]), "Removed jq");
    assert_eq!(
        removed_summary(&[], &["a".to_string(), "b".to_string()]),
        "Removed a, b"
    );
}

#[test]
fn next_hint_appears_only_when_bin_is_missing_from_path() {
    let temp = tempfile::tempdir().unwrap();
    let bin = temp.path().join("bin");
    fs::create_dir(&bin).unwrap();
    let empty = std::ffi::OsStr::new("");
    assert_eq!(
        next_path_hint(empty, &bin, None).as_deref(),
        Some("eval \"$(brewrs shellenv)\"  (add it to ~/.zprofile to keep it)")
    );

    let exact = bin.as_os_str();
    assert_eq!(next_path_hint(exact, &bin, None), None);

    let canonical = fs::canonicalize(&bin).unwrap();
    assert_eq!(next_path_hint(canonical.as_os_str(), &bin, None), None);

    let other = temp.path().join("other");
    let with_other = std::ffi::OsString::from(other.as_os_str());
    assert!(next_path_hint(&with_other, &bin, None).is_some());
}

#[test]
fn next_hint_quotes_an_explicit_prefix() {
    let temp = tempfile::tempdir().unwrap();
    let bin = temp.path().join("bin");
    let prefix = Path::new("/tmp/it's here");
    let hint = next_path_hint(std::ffi::OsStr::new(""), &bin, Some(prefix)).unwrap();
    assert_eq!(
        hint,
        "eval \"$(brewrs shellenv --prefix '/tmp/it'\\''s here')\"  (add it to ~/.zprofile to keep it)"
    );
}

fn formula() -> PlannedFormula {
    PlannedFormula {
        source: Default::default(),
        name: "hello".into(),
        version: "1.2".into(),
        version_scheme: Some(0),
        keg_only: false,
        has_service: false,
        caveats: None,
        conflicts_with: Vec::new(),
        requested: true,
        url: String::new(),
        sha256: "a".repeat(64),
        cellar: ":any_skip_relocation".into(),
        install_blockers: Vec::new(),
        dependencies: Vec::new(),
    }
}

#[test]
fn preview_and_apply_share_eligibility_checks() {
    let mut package = formula();
    package.install_blockers.push("has a post-install hook");
    let plan = [package];
    assert!(
        pending_packages(&plan, &[], &[])
            .unwrap_err()
            .to_string()
            .contains("post-install")
    );
}

fn installed_hello(version: &str) -> Receipt {
    Receipt {
        source: Default::default(),
        name: "hello".into(),
        version: version.into(),
        version_scheme: 0,
        sha256: "a".repeat(64),
        requested: true,
        keg_only: false,
        conflicts_with: Vec::new(),
        dependencies: Vec::new(),
    }
}

fn package_receipt(name: &str, requested: bool, dependencies: &[&str]) -> Receipt {
    Receipt {
        name: name.into(),
        requested,
        dependencies: dependencies
            .iter()
            .map(|name| Dependency {
                name: (*name).into(),
                version: "1.2".into(),
                sha256: "a".repeat(64),
            })
            .collect(),
        ..installed_hello("1.2")
    }
}

#[test]
fn autoremove_keeps_the_full_requested_closure() {
    let installed = [
        package_receipt("app", true, &["middle"]),
        package_receipt("middle", false, &["leaf"]),
        package_receipt("leaf", false, &[]),
        package_receipt("unused", false, &["leaf"]),
    ];
    assert_eq!(
        autoremove_plan(&installed, &[]).unwrap(),
        [installed[3].clone()]
    );
    assert!(autoremove_plan(&[], &[]).unwrap().is_empty());
    assert!(autoremove_plan(&installed[..2], &[]).is_err());
}

#[test]
fn combined_removal_leaves_existing_orphans_and_their_dependencies() {
    let installed = [
        package_receipt("app", true, &["shared", "sole"]),
        package_receipt("old-orphan", false, &["shared"]),
        package_receipt("shared", false, &[]),
        package_receipt("sole", false, &[]),
    ];
    let targets = ["app".into()];
    assert_eq!(
        planned_removals(&targets, &installed, true, &[]).unwrap(),
        [installed[0].clone(), installed[3].clone()]
    );
    assert_eq!(
        planned_removals(&targets, &installed, false, &[]).unwrap(),
        [installed[0].clone()]
    );
    assert!(planned_removals(&["shared".into()], &installed, true, &[]).is_err());
}

#[test]
fn explicitly_installing_a_dependency_plans_a_promotion_without_a_download() {
    let installed = [package_receipt("hello", false, &[])];
    let mut plan = [formula()];
    assert!(pending_packages(&plan, &installed, &[]).unwrap().is_empty());
    assert_eq!(requested_promotions(&plan, &installed), installed);
    plan[0].requested = false;
    assert!(requested_promotions(&plan, &installed).is_empty());
}

#[test]
fn upgrade_only_selects_strictly_newer_compatible_bottles() {
    let mut formula = formula();
    let mut old = [installed_hello("1.2")];
    assert!(upgrade_plan(&[formula.clone()], &old).unwrap().is_empty());
    formula.version = "1.2_1".into();
    formula.sha256 = "b".repeat(64);
    let planned = [formula.clone()];
    let changes = upgrade_plan(&planned, &old).unwrap();
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0].0.version, "1.2");
    assert_eq!(changes[0].1.receipt.version, "1.2_1");

    // Higher version_scheme upgrades even when numeric version resets.
    formula.version_scheme = Some(1);
    formula.version = "1.0".into();
    let scheme_planned = [formula.clone()];
    let scheme_upgrade = upgrade_plan(&scheme_planned, &old).unwrap();
    assert_eq!(scheme_upgrade.len(), 1);
    assert_eq!(scheme_upgrade[0].1.receipt.version_scheme, 1);

    // Lower version_scheme is rejected as a downgrade.
    old[0].version_scheme = 1;
    old[0].version = "1.0".into();
    formula.version_scheme = Some(0);
    formula.version = "2025.0".into();
    assert!(
        upgrade_plan(&[formula.clone()], &old)
            .unwrap_err()
            .to_string()
            .contains("downgrade")
    );

    // Unknown version_scheme is rejected.
    old[0].version_scheme = 0;
    formula.version_scheme = None;
    assert!(
        upgrade_plan(&[formula.clone()], &old)
            .unwrap_err()
            .to_string()
            .contains("version scheme")
    );
    formula.version_scheme = Some(0);
    formula.install_blockers.push("needs relocation");
    assert!(
        upgrade_plan(&[formula], &old)
            .unwrap_err()
            .to_string()
            .contains("needs relocation")
    );
}

#[test]
fn installing_a_dependency_records_its_exact_bottle_identity() {
    let mut app = formula();
    app.dependencies.push("library".into());
    let mut library = formula();
    library.name = "library".into();
    library.requested = false;
    library.sha256 = "c".repeat(64);

    assert!(pending_packages(&[app.clone()], &[], &[]).is_err());
    let plan = [library.clone(), app];
    let pending = pending_packages(&plan, &[], &[]).unwrap();
    assert_eq!(pending.len(), 2);
    assert!(!pending[0].receipt.requested);
    assert_eq!(
        pending[1].receipt.dependencies,
        [Dependency {
            name: library.name,
            version: library.version,
            sha256: library.sha256,
        }]
    );
    let already_installed = [pending[0].receipt.clone()];
    let pending = pending_packages(&plan, &already_installed, &[]).unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].receipt.name, "hello");
}

#[test]
fn unchanged_bottle_keeps_its_recorded_dependency_pin_during_upgrade() {
    let mut app = formula();
    app.dependencies.push("library".into());
    let mut library = formula();
    library.name = "library".into();
    let mut old_library = installed_hello("1.1");
    old_library.name = "library".into();
    let mut old_app = installed_hello("1.2");
    old_app.dependencies.push(Dependency {
        name: old_library.name.clone(),
        version: old_library.version.clone(),
        sha256: old_library.sha256.clone(),
    });
    let plan = [library, app];
    let installed = [old_library, old_app];
    assert!(upgrade_plan(&plan, &installed[1..]).unwrap().is_empty());
    let changes = upgrade_plan(&plan, &installed).unwrap();
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0].1.receipt.name, "library");
    assert!(
        crate::native::store::validate_dependencies(&[
            changes[0].1.receipt.clone(),
            installed[1].clone()
        ])
        .is_err()
    );
}

#[test]
fn upgrade_rejects_equal_version_replacements_and_downgrades() {
    let old = [installed_hello("1.2")];
    let mut formula = formula();
    formula.sha256 = "b".repeat(64);
    assert!(
        upgrade_plan(&[formula.clone()], &old)
            .unwrap_err()
            .to_string()
            .contains("same version")
    );
    formula.version = "1.2.0".into();
    assert!(
        upgrade_plan(&[formula.clone()], &old)
            .unwrap_err()
            .to_string()
            .contains("same version")
    );
    formula.version = "1.1".into();
    assert!(
        upgrade_plan(&[formula.clone()], &old)
            .unwrap_err()
            .to_string()
            .contains("downgrade")
    );
    formula.version = "1.3-beta".into();
    assert!(
        upgrade_plan(&[formula], &old)
            .unwrap_err()
            .to_string()
            .contains("Cannot compare")
    );
}

#[test]
fn reinstall_is_a_noop_only_for_the_same_version_and_digest() {
    let plan = [formula()];
    let mut receipt = Receipt {
        source: Default::default(),
        name: "hello".into(),
        version: "1.2".into(),
        version_scheme: 0,
        sha256: "a".repeat(64),
        requested: true,
        keg_only: false,
        conflicts_with: Vec::new(),
        dependencies: Vec::new(),
    };
    assert!(
        pending_packages(&plan, std::slice::from_ref(&receipt), &[])
            .unwrap()
            .is_empty()
    );
    receipt.sha256 = "b".repeat(64);
    assert!(pending_packages(&plan, &[receipt], &[]).is_err());
}

fn receipt(name: &str, version: &str, digest: char) -> Receipt {
    Receipt {
        source: Default::default(),
        name: name.into(),
        version: version.into(),
        version_scheme: 0,
        sha256: digest.to_string().repeat(64),
        requested: true,
        keg_only: false,
        conflicts_with: Vec::new(),
        dependencies: Vec::new(),
    }
}

#[test]
fn upstream_installed_catalog_targets_are_reported_not_resolved() {
    let catalog = super::super::portable::catalog().unwrap();
    let mut neovim = receipt("neovim", "0.12.6", 'a');
    neovim.source = PackageSource::Upstream;
    let targets = ["nvim".to_string(), "ripgrep".to_string()];

    let (rest, notes) = partition_upstream_installed(catalog, &targets, &[neovim.clone()]);
    assert_eq!(rest, ["ripgrep"]);
    assert_eq!(
        notes,
        ["nvim → neovim 0.12.6 is already installed (official upstream build)."]
    );

    // A Homebrew-sourced receipt, or nothing installed, resolves normally.
    neovim.source = PackageSource::Homebrew;
    for installed in [vec![neovim], Vec::new()] {
        let (rest, notes) = partition_upstream_installed(catalog, &targets, &installed);
        assert_eq!(rest, targets);
        assert!(notes.is_empty());
    }
}

#[test]
fn check_updates_and_upgrade_share_the_same_change_plan() {
    let current = [receipt("hello", "1.2", 'a')];
    let mut available = formula();
    assert!(
        planned_changes(&[available.clone()], &current, &current, &[])
            .unwrap()
            .is_empty()
    );
    assert!(
        upgrade_plan(&[available.clone()], &current)
            .unwrap()
            .is_empty()
    );

    available.version = "1.3".into();
    available.sha256 = "b".repeat(64);
    let changes = planned_changes(&[available], &current, &current, &[]).unwrap();
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0].old.name, "hello");
    assert_eq!(changes[0].old.version, "1.2");
    assert_eq!(changes[0].new.version, "1.3");
    assert_eq!(changes[0].formula.version, "1.3");
    let rows = upgrade_rows(&changes);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].action, "upgrade");
    assert_eq!(
        rows[0].cells,
        [
            "hello".to_string(),
            "1.2 → 1.3".to_string(),
            "homebrew".to_string()
        ]
    );
    assert!(rows[0].note.is_empty());
    assert!(rows[0].requested);
}

#[test]
fn a_same_version_bottle_change_names_both_installed_and_available_pins() {
    let current = [receipt("hello", "1.2", 'a')];
    let mut available = formula();
    available.sha256 = "b".repeat(64);
    let message = planned_changes(&[available], &current, &current, &[])
        .unwrap_err()
        .to_string();
    assert!(message.contains("same version"), "{message}");
    assert!(message.contains("1.2"), "{message}");
    assert!(message.contains("aaaaaaaaaaaa"), "{message}");
    assert!(message.contains("bbbbbbbbbbbb"), "{message}");
}

#[test]
fn install_explains_newer_versions_and_differing_pins() {
    let current = [receipt("hello", "1.2", 'a')];
    let mut available = formula();
    available.version = "1.3".into();
    available.sha256 = "b".repeat(64);
    let message = pending_packages(&[available], &current, &[])
        .unwrap_err()
        .to_string();
    assert!(message.contains("hello 1.2 is installed"), "{message}");
    assert!(message.contains("1.3 is available"), "{message}");
    assert!(message.contains("native upgrade"), "{message}");

    let mut available = formula();
    available.sha256 = "b".repeat(64);
    let message = pending_packages(&[available], &current, &[])
        .unwrap_err()
        .to_string();
    assert!(message.contains("same version"), "{message}");
    assert!(message.contains("aaaaaaaaaaaa"), "{message}");
    assert!(message.contains("bbbbbbbbbbbb"), "{message}");

    // A receipt written before dependency pins existed records none, while
    // the plan now resolves a dependency for the identical bottle.
    let mut available = formula();
    available.dependencies.push("library".into());
    let mut library = formula();
    library.name = "library".into();
    library.requested = false;
    let message = pending_packages(&[library, available], &current, &[])
        .unwrap_err()
        .to_string();
    assert!(message.contains("hello 1.2"), "{message}");
    assert!(message.contains("dependency pins"), "{message}");
}

#[test]
fn upgrade_preview_accepts_retaining_an_exact_old_pin() {
    let mut library = formula();
    library.name = "library".into();
    library.version = "1.2".into();
    library.sha256 = "d".repeat(64);
    let mut app = formula();
    app.dependencies.push("library".into());

    let old_library = receipt("library", "1.1", 'c');
    let mut old_app = receipt("hello", "1.2", 'a');
    old_app.dependencies.push(Dependency {
        name: "library".into(),
        version: "1.1".into(),
        sha256: "c".repeat(64),
    });
    let mut installed = [old_library, old_app];
    let plan = [library, app];

    let changes = planned_changes(&plan, &installed, &installed, &[]).unwrap();
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0].old.version, "1.1");
    assert_eq!(changes[0].new.version, "1.2");
    assert_eq!(installed[1].dependencies[0].sha256, "c".repeat(64));

    installed[1].dependencies[0].sha256 = "e".repeat(64);
    let error = planned_changes(&plan, &installed, &installed, &[]).unwrap_err();
    assert!(format!("{error:#}").contains("unmet dependency pins"));
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn check_updates_is_read_only_and_absent_for_an_empty_prefix() {
    let home = tempfile::tempdir().unwrap();
    let prefix = home.path().join("native");
    let updates = check_updates(&Config::default(), Some(&prefix))
        .await
        .unwrap();
    assert!(!updates);
    assert!(!prefix.exists());
}

#[test]
fn install_and_upgrade_reject_symmetric_conflicts() {
    let mut app = formula();
    app.name = "app".into();
    app.conflicts_with = vec!["other".into()];
    let mut other = formula();
    other.name = "other".into();

    // Conflict within one install batch.
    assert!(
        pending_packages(&[app.clone(), other.clone()], &[], &[])
            .unwrap_err()
            .to_string()
            .contains("conflicts with")
    );

    // Installed package declares conflict with newly requested package.
    let mut installed_app = receipt("app", "1.2", 'a');
    installed_app.conflicts_with = vec!["other".into()];
    assert!(
        pending_packages(&[other.clone()], std::slice::from_ref(&installed_app), &[])
            .unwrap_err()
            .to_string()
            .contains("conflicts with")
    );

    // Upgrade introduces a conflict with another installed package.
    let installed_other = receipt("other", "1.2", 'a');
    let mut app_v2 = app.clone();
    app_v2.version = "1.3".into();
    app_v2.sha256 = "b".repeat(64);
    assert!(
        planned_changes(
            &[app_v2],
            &[receipt("app", "1.2", 'a'), installed_other],
            &[receipt("app", "1.2", 'a')],
            &[],
        )
        .unwrap_err()
        .to_string()
        .contains("conflicts with")
    );
}

#[test]
fn render_shellenv_quotes_paths_for_posix_and_fish() {
    let prefix = Path::new("/tmp/brew's space/native/");
    let posix = render_shellenv(prefix, Some("zsh"), None).unwrap();
    assert_eq!(
        posix,
        "export BREWRS_PREFIX='/tmp/brew'\\''s space/native';\nexport PATH='/tmp/brew'\\''s space/native/bin:/tmp/brew'\\''s space/native/sbin':\"$PATH\";\n"
    );

    let fish_auto = render_shellenv(
        Path::new("/tmp/space/native"),
        None,
        Some(std::ffi::OsStr::new("/opt/homebrew/bin/fish")),
    )
    .unwrap();
    assert_eq!(
        fish_auto,
        "set -gx BREWRS_PREFIX '/tmp/space/native';\nfish_add_path -gP '/tmp/space/native/bin' '/tmp/space/native/sbin';\n"
    );

    let fish_quoted = render_shellenv(prefix, Some("fish"), None).unwrap();
    assert_eq!(
        fish_quoted,
        "set -gx BREWRS_PREFIX '/tmp/brew\\'s space/native';\nfish_add_path -gP '/tmp/brew\\'s space/native/bin' '/tmp/brew\\'s space/native/sbin';\n"
    );

    assert!(render_shellenv(Path::new("relative/path"), Some("sh"), None).is_err());
    assert!(render_shellenv(Path::new("/tmp/bad\npath"), Some("sh"), None).is_err());
}

const TAG: &str = "arm64_sequoia";

fn named(name: &str, requested: bool, dependencies: &[&str]) -> PlannedFormula {
    PlannedFormula {
        name: name.into(),
        requested,
        dependencies: dependencies.iter().map(|name| (*name).into()).collect(),
        ..formula()
    }
}

fn blocked_message(plan: &[PlannedFormula]) -> String {
    let error = pending_packages(plan, &[], &[]).unwrap_err();
    with_upstream_hint(super::super::portable::catalog(), error, plan, &[], TAG).to_string()
}

#[test]
fn blocked_target_with_catalog_entry_gets_an_upstream_hint() {
    let mut neovim = named("neovim", true, &["gettext"]);
    neovim.install_blockers.clear();
    let mut gettext = named("gettext", false, &[]);
    gettext
        .install_blockers
        .push("requires a fixed Homebrew prefix");
    let message = blocked_message(&[gettext, neovim]);
    assert!(message.starts_with("Cannot install gettext natively: requires a fixed"));
    assert!(message.contains(
        "An official upstream build is available: brewrs install --source upstream neovim"
    ));
}

#[test]
fn blocked_dependency_gets_a_reason_and_its_requested_dependents() {
    let mut library = named("lib", false, &[]);
    library
        .install_blockers
        .push("requires a fixed Homebrew prefix");
    let message = pending_packages(&[library, named("app", true, &["lib"])], &[], &[])
        .unwrap_err()
        .to_string();
    assert_eq!(
        message,
        "Cannot install lib natively: requires a fixed Homebrew prefix.\n  Homebrew built this bottle for /opt/homebrew only; brewrs installs into its own prefix, so this bottle can't be used.\n  Needed by: app"
    );
}

#[test]
fn blocked_target_without_catalog_entry_gets_no_hint() {
    let mut tool = named("sometool", true, &[]);
    tool.install_blockers.push("needs relocation");
    let message = blocked_message(&[tool]);
    assert_eq!(
        message,
        "Cannot install sometool natively: needs relocation."
    );
}

#[test]
fn blocked_catalog_dependency_does_not_hint_for_an_unrelated_target() {
    // neovim is only a (blocked) dependency of the requested target, and
    // the target itself has no catalog entry.
    let mut neovim = named("neovim", false, &[]);
    neovim.install_blockers.push("needs relocation");
    let message = blocked_message(&[neovim, named("sometool", true, &["neovim"])]);
    assert!(!message.contains("Hint"));
}

#[test]
fn unblocked_homebrew_target_with_catalog_entry_stays_homebrew() {
    let plan = [named("neovim", true, &[])];
    let pending = pending_packages(&plan, &[], &[]).unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].formula.source, PackageSource::Homebrew);
    assert_eq!(pending[0].receipt.source, PackageSource::Homebrew);
}

#[test]
fn upstream_never_satisfies_a_homebrew_dependency() {
    let mut upstream = receipt("lib", "1.0", 'c');
    upstream.source = PackageSource::Upstream;
    let app = named("app", true, &["lib"]);
    let message = validate_sources(std::slice::from_ref(&app), &[upstream])
        .unwrap_err()
        .to_string();
    assert!(
        message.contains("app") && message.contains("lib"),
        "{message}"
    );

    let mut planned = named("lib", false, &[]);
    planned.source = PackageSource::Upstream;
    let message = validate_sources(&[planned, app], &[])
        .unwrap_err()
        .to_string();
    assert!(message.contains("planned from upstream"), "{message}");
}

#[test]
fn switching_source_requires_uninstalling_first() {
    let installed = [receipt("hello", "1.2", 'a')];
    let mut upstream = formula();
    upstream.source = PackageSource::Upstream;
    assert_eq!(
        validate_sources(&[upstream], &installed)
            .unwrap_err()
            .to_string(),
        "hello is installed from Homebrew; uninstall it first to switch"
    );
    assert!(validate_sources(&[formula()], &installed).is_ok());
}

#[test]
fn upgrade_refuses_to_switch_source() {
    let mut old = receipt("hello", "1.0", 'a');
    old.source = PackageSource::Upstream;
    assert!(
        upgrade_plan(&[formula()], &[old])
            .unwrap_err()
            .to_string()
            .contains("Refusing to change the installed source")
    );
}

#[test]
fn removal_resolves_a_catalog_alias_only_to_an_upstream_receipt() {
    let installed = [receipt("neovim", "0.12.6", 'a')];
    assert_eq!(
        removal_plan(&["nvim".into()], &installed).unwrap()[0].name,
        "neovim"
    );
    assert!(removal_plan(&["nvim".into()], &[]).is_err());
}
