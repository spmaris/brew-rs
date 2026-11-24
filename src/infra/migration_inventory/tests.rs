use super::*;
use std::fs;

fn write_receipt(path: &Path, tap: Option<&str>, requested: Option<bool>) {
    write_receipt_value(path, tap.map(serde_json::Value::from), requested);
}

fn write_receipt_value(path: &Path, tap: Option<serde_json::Value>, requested: Option<bool>) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut source = serde_json::Map::new();
    if let Some(tap) = tap {
        source.insert("tap".into(), tap);
    }
    let mut receipt = serde_json::Map::new();
    if let Some(requested) = requested {
        receipt.insert("installed_on_request".into(), requested.into());
    }
    receipt.insert("source".into(), source.into());
    fs::write(
        path,
        serde_json::to_vec(&serde_json::Value::Object(receipt)).unwrap(),
    )
    .unwrap();
}

fn formula<'a>(inventory: &'a Inventory, name: &str) -> &'a Formula {
    inventory
        .formulae
        .iter()
        .find(|formula| formula.name == name)
        .unwrap_or_else(|| panic!("missing formula {name}"))
}

#[test]
fn inventories_cellar_casks_taps_and_services() {
    let sandbox = tempfile::tempdir().unwrap();
    // A prefix with a space proves path joining does not tokenize.
    let prefix = sandbox.path().join("Homebrew Prefix");
    fs::create_dir_all(&prefix).unwrap();

    write_receipt(
        &prefix.join("Cellar/ripgrep/16.0.0/INSTALL_RECEIPT.json"),
        Some("homebrew/core"),
        Some(true),
    );
    write_receipt(
        &prefix.join("Cellar/ripgrep/15.1.0/INSTALL_RECEIPT.json"),
        Some("homebrew/core"),
        Some(true),
    );
    // Conflicting receipts leave the whole formula's field unknown: 9.0 is
    // in core and was requested, 10.0 came from a user tap as a dependency.
    write_receipt(
        &prefix.join("Cellar/mixed/9.0/INSTALL_RECEIPT.json"),
        Some("homebrew/core"),
        Some(true),
    );
    write_receipt(
        &prefix.join("Cellar/mixed/10.0/INSTALL_RECEIPT.json"),
        Some("acme/tools"),
        Some(false),
    );
    // Older receipts may omit both fields.
    write_receipt(
        &prefix.join("Cellar/pcre2@10/10.49/INSTALL_RECEIPT.json"),
        None,
        None,
    );
    // A missing receipt and a malformed receipt keep the version with
    // unknown receipt facts and one warning each.
    fs::create_dir_all(prefix.join("Cellar/bzip2/1.0.8")).unwrap();
    fs::create_dir_all(prefix.join("Cellar/broken/1.0")).unwrap();
    fs::write(
        prefix.join("Cellar/broken/1.0/INSTALL_RECEIPT.json"),
        b"{not json",
    )
    .unwrap();
    // A wrong field type is treated as a malformed receipt, not guessed.
    write_receipt_value(
        &prefix.join("Cellar/typed/1.0/INSTALL_RECEIPT.json"),
        Some("homebrew/core".into()),
        None,
    );
    let mut receipt = serde_json::Map::new();
    receipt.insert("installed_on_request".into(), "yes".into());
    fs::write(
        prefix.join("Cellar/typed/1.0/INSTALL_RECEIPT.json"),
        serde_json::to_vec(&serde_json::Value::Object(receipt)).unwrap(),
    )
    .unwrap();

    fs::create_dir_all(prefix.join("Caskroom/firefox/129.0")).unwrap();
    fs::create_dir_all(prefix.join("Caskroom/firefox/128.0")).unwrap();
    fs::create_dir_all(prefix.join("Caskroom/temurin@17/17.0.2")).unwrap();

    fs::create_dir_all(prefix.join("Library/Taps/homebrew/homebrew-core")).unwrap();
    fs::create_dir_all(prefix.join("Library/Taps/acme/homebrew-tools")).unwrap();
    fs::create_dir_all(prefix.join("Library/Taps/acme/foreign-repo")).unwrap();
    fs::write(prefix.join("Library/Taps/README.md"), b"foreign").unwrap();

    let agents = sandbox.path().join("LaunchAgents");
    fs::create_dir_all(&agents).unwrap();
    fs::write(agents.join("homebrew.mxcl.ripgrep.plist"), b"<plist/>").unwrap();
    fs::write(agents.join("com.example.other.PLIST"), b"<plist/>").unwrap();
    fs::write(agents.join("notes.txt"), b"not a service").unwrap();
    let other_agents = sandbox.path().join("Other Launch Agents");
    fs::create_dir_all(&other_agents).unwrap();
    fs::write(other_agents.join("homebrew.mxcl.pcre2.plist"), b"<plist/>").unwrap();

    let inventory = collect(&prefix, &[agents.clone(), other_agents.clone()]).unwrap();

    assert_eq!(
        inventory.formulae,
        vec![
            Formula {
                name: "broken".into(),
                versions: vec!["1.0".into()],
                tap: None,
                requested: None,
            },
            Formula {
                name: "bzip2".into(),
                versions: vec!["1.0.8".into()],
                tap: None,
                requested: None,
            },
            Formula {
                name: "mixed".into(),
                versions: vec!["10.0".into(), "9.0".into()],
                tap: None,
                requested: None,
            },
            Formula {
                name: "pcre2@10".into(),
                versions: vec!["10.49".into()],
                tap: None,
                requested: None,
            },
            Formula {
                name: "ripgrep".into(),
                versions: vec!["15.1.0".into(), "16.0.0".into()],
                tap: Some("homebrew/core".into()),
                requested: Some(true),
            },
            Formula {
                name: "typed".into(),
                versions: vec!["1.0".into()],
                tap: None,
                requested: None,
            },
        ]
    );
    assert_eq!(
        inventory.casks,
        vec![
            Cask {
                token: "firefox".into(),
                versions: vec!["128.0".into(), "129.0".into()],
            },
            Cask {
                token: "temurin@17".into(),
                versions: vec!["17.0.2".into()],
            },
        ]
    );
    assert_eq!(inventory.taps, vec!["acme/tools", "homebrew/core"]);
    assert_eq!(
        inventory.services,
        vec![
            Service {
                label: "homebrew.mxcl.pcre2".into(),
                path: other_agents.join("homebrew.mxcl.pcre2.plist"),
            },
            Service {
                label: "homebrew.mxcl.ripgrep".into(),
                path: agents.join("homebrew.mxcl.ripgrep.plist"),
            },
        ]
    );

    for expected in [
        "missing INSTALL_RECEIPT.json for bzip2 1.0.8",
        "malformed INSTALL_RECEIPT.json for broken 1.0",
        "malformed INSTALL_RECEIPT.json for typed 1.0",
        "versions of \"mixed\" name different source taps",
        "versions of \"mixed\" disagree about installed_on_request",
    ] {
        assert!(
            inventory
                .warnings
                .iter()
                .any(|warning| warning.contains(expected)),
            "missing warning {expected:?} in {:?}",
            inventory.warnings
        );
    }
    // The foreign tap layout and non-plist files are skipped without
    // inventing records; no warning is needed for the plain README.
    assert!(!inventory.warnings.iter().any(|w| w.contains("empty")));
}

#[test]
fn empty_prefix_is_a_valid_empty_inventory() {
    let sandbox = tempfile::tempdir().unwrap();
    let inventory = collect(sandbox.path(), &[]).unwrap();
    assert_eq!(inventory, Inventory::default());
}

#[test]
fn refuses_relative_unnormalized_or_missing_prefixes() {
    let sandbox = tempfile::tempdir().unwrap();
    assert!(collect(Path::new("relative/prefix"), &[]).is_err());
    assert!(collect(Path::new("/"), &[]).is_err());
    let unnormalized = sandbox.path().join("..").join("prefix");
    assert!(collect(&unnormalized, &[]).is_err());
    assert!(collect(&sandbox.path().join("missing"), &[]).is_err());
    let file = sandbox.path().join("not-a-directory");
    fs::write(&file, b"x").unwrap();
    assert!(collect(&file, &[]).is_err());
}

#[test]
fn reports_missing_service_directories_without_failing() {
    let sandbox = tempfile::tempdir().unwrap();
    let missing = sandbox.path().join("missing agents");
    let inventory = collect(sandbox.path(), std::slice::from_ref(&missing)).unwrap();
    assert!(inventory.services.is_empty());
    assert!(
        inventory.warnings.iter().any(|warning| warning
            .contains("Services: directory does not exist")
            && warning.contains("missing agents")),
        "{:?}",
        inventory.warnings
    );
}

#[test]
fn keeps_multiple_versions_and_caps_them() {
    let sandbox = tempfile::tempdir().unwrap();
    let prefix = sandbox.path();
    for index in 0..=MAX_VERSIONS_PER_PACKAGE {
        let version = format!("1.0.{index}");
        fs::create_dir_all(prefix.join("Cellar/many").join(version)).unwrap();
    }
    let inventory = collect(prefix, &[]).unwrap();
    let many = formula(&inventory, "many");
    assert_eq!(many.versions.len(), MAX_VERSIONS_PER_PACKAGE);
    assert!(
        inventory
            .warnings
            .iter()
            .any(|warning| warning.contains("keeping only")),
        "{:?}",
        inventory.warnings
    );
}

#[test]
fn caps_retained_warnings_and_says_how_many_were_omitted() {
    let sandbox = tempfile::tempdir().unwrap();
    let prefix = sandbox.path();
    for index in 0..MAX_WARNINGS + 5 {
        // Missing receipts produce one warning per version.
        fs::create_dir_all(prefix.join(format!("Cellar/f{index}/1.0"))).unwrap();
    }
    let inventory = collect(prefix, &[]).unwrap();
    assert_eq!(inventory.warnings.len(), MAX_WARNINGS + 1);
    assert!(
        inventory
            .warnings
            .last()
            .unwrap()
            .contains("more warnings omitted")
    );
}

#[test]
fn separates_requested_and_dependency_receipts() {
    let sandbox = tempfile::tempdir().unwrap();
    let prefix = sandbox.path();
    write_receipt(
        &prefix.join("Cellar/app/2.0/INSTALL_RECEIPT.json"),
        Some("acme/tools"),
        Some(true),
    );
    write_receipt(
        &prefix.join("Cellar/lib/1.0/INSTALL_RECEIPT.json"),
        Some("acme/tools"),
        Some(false),
    );
    let inventory = collect(prefix, &[]).unwrap();
    assert_eq!(formula(&inventory, "app").requested, Some(true));
    assert_eq!(formula(&inventory, "lib").requested, Some(false));
    assert_eq!(
        formula(&inventory, "app").tap.as_deref(),
        Some("acme/tools")
    );
}

#[test]
fn rejects_implausible_receipt_metadata_but_keeps_the_version() {
    let sandbox = tempfile::tempdir().unwrap();
    let prefix = sandbox.path();
    write_receipt(
        &prefix.join("Cellar/odd/1.0/INSTALL_RECEIPT.json"),
        Some("not a tap"),
        Some(true),
    );
    fs::create_dir_all(prefix.join("Cellar/odd/2.0")).unwrap();
    fs::write(
        prefix.join("Cellar/odd/2.0/INSTALL_RECEIPT.json"),
        vec![b' '; MAX_RECEIPT_BYTES as usize + 1],
    )
    .unwrap();
    let inventory = collect(prefix, &[]).unwrap();
    let odd = formula(&inventory, "odd");
    assert_eq!(odd.versions, vec!["1.0", "2.0"]);
    assert_eq!(odd.tap, None);
    assert_eq!(odd.requested, Some(true));
    assert!(
        inventory
            .warnings
            .iter()
            .any(|warning| warning.contains("implausible source tap")),
        "{:?}",
        inventory.warnings
    );
    assert!(
        inventory
            .warnings
            .iter()
            .any(|warning| warning.contains("exceeds the")),
        "{:?}",
        inventory.warnings
    );
}

#[cfg(unix)]
#[test]
fn never_follows_symlinks_for_cellar_casks_taps_or_services() {
    use std::os::unix::fs::symlink;

    let sandbox = tempfile::tempdir().unwrap();
    let outside = sandbox.path().join("outside");
    let prefix = sandbox.path().join("prefix");
    fs::create_dir_all(prefix.join("Cellar")).unwrap();
    fs::create_dir_all(prefix.join("Caskroom")).unwrap();
    fs::create_dir_all(prefix.join("Library/Taps/acme")).unwrap();

    // A symlinked formula directory pointing at a real receipt elsewhere.
    write_receipt(
        &outside.join("Cellar/evil/1.0/INSTALL_RECEIPT.json"),
        Some("homebrew/core"),
        Some(true),
    );
    symlink(outside.join("Cellar/evil"), prefix.join("Cellar/evil")).unwrap();

    // A real keg whose receipt is a symlink to an outside receipt.
    fs::create_dir_all(prefix.join("Cellar/linked/1.0")).unwrap();
    write_receipt(
        &outside.join("receipt.json"),
        Some("homebrew/core"),
        Some(true),
    );
    symlink(
        outside.join("receipt.json"),
        prefix.join("Cellar/linked/1.0/INSTALL_RECEIPT.json"),
    )
    .unwrap();

    // A symlinked version directory.
    fs::create_dir_all(prefix.join("Cellar/versioned")).unwrap();
    write_receipt(
        &outside.join("keg/2.0/INSTALL_RECEIPT.json"),
        Some("homebrew/core"),
        Some(true),
    );
    symlink(outside.join("keg/2.0"), prefix.join("Cellar/versioned/2.0")).unwrap();

    // A symlinked Caskroom token and tap repository.
    fs::create_dir_all(outside.join("Caskroom/app/1.0")).unwrap();
    symlink(outside.join("Caskroom/app"), prefix.join("Caskroom/app")).unwrap();
    fs::create_dir_all(outside.join("tap-repo")).unwrap();
    symlink(
        outside.join("tap-repo"),
        prefix.join("Library/Taps/acme/homebrew-evil"),
    )
    .unwrap();

    // A symlinked service plist.
    let agents = sandbox.path().join("agents");
    fs::create_dir_all(&agents).unwrap();
    fs::write(outside.join("service.plist"), b"<plist/>").unwrap();
    symlink(
        outside.join("service.plist"),
        agents.join("homebrew.mxcl.evil.plist"),
    )
    .unwrap();

    let inventory = collect(&prefix, &[agents]).unwrap();
    assert!(inventory.casks.is_empty());
    assert!(inventory.taps.is_empty());
    assert!(inventory.services.is_empty());
    assert_eq!(
        inventory
            .formulae
            .iter()
            .map(|formula| formula.name.as_str())
            .collect::<Vec<_>>(),
        vec!["linked"]
    );
    assert_eq!(formula(&inventory, "linked").tap, None);
    for expected in ["symlinked formula directory", "symlinked install receipt"] {
        assert!(
            inventory
                .warnings
                .iter()
                .any(|warning| warning.contains(expected)),
            "missing warning {expected:?} in {:?}",
            inventory.warnings
        );
    }
    assert!(
        !inventory
            .formulae
            .iter()
            .any(|formula| formula.name == "evil")
    );
}

#[cfg(unix)]
#[test]
fn skips_non_utf8_names_without_guessing() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;

    let sandbox = tempfile::tempdir().unwrap();
    let prefix = sandbox.path();
    fs::create_dir_all(prefix.join("Cellar")).unwrap();
    if let Err(error) = fs::create_dir(
        prefix
            .join("Cellar")
            .join(OsStr::from_bytes(b"bad\xffname")),
    ) {
        // Some harness sandboxes and filesystems (such as macOS APFS with
        // EILSEQ) refuse non-UTF-8 paths before creating them; the branch is
        // only unreachable on such hosts.
        if error.kind() == std::io::ErrorKind::PermissionDenied
            || (cfg!(target_os = "macos") && error.raw_os_error() == Some(92))
        {
            return;
        }
        panic!("could not create the non-UTF-8 fixture: {error}");
    }
    let inventory = collect(prefix, &[]).unwrap();
    assert!(inventory.formulae.is_empty());
    assert!(
        inventory
            .warnings
            .iter()
            .any(|warning| warning.contains("non-UTF-8")),
        "{:?}",
        inventory.warnings
    );
}
