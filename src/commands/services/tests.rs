use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};

use crate::config::Config;
use crate::domain::plan::InstalledServiceMetadata;
use crate::infra::store::{Dependency, Receipt};

use super::colima::*;
use super::launchd::*;
use super::observe::*;
use super::preview::*;
use super::*;

fn seed_colima_prefix(prefix: &Path) -> (PathBuf, PathBuf, Vec<Receipt>) {
    let colima_bin = prefix.join("Cellar/colima/0.10.3/bin/colima");
    let lima_bin = prefix.join("Cellar/lima/2.3.0/bin/limactl");
    for bin in [&colima_bin, &lima_bin] {
        fs::create_dir_all(bin.parent().unwrap()).unwrap();
        fs::write(bin, b"#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(bin, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let installed = vec![
        Receipt {
            source: Default::default(),
            name: "colima".into(),
            version: "0.10.3".into(),
            version_scheme: 0,
            sha256: "a".repeat(64),
            requested: true,
            keg_only: false,
            conflicts_with: Vec::new(),
            dependencies: vec![Dependency {
                name: "lima".into(),
                version: "2.3.0".into(),
                sha256: "b".repeat(64),
            }],
        },
        Receipt {
            source: Default::default(),
            name: "lima".into(),
            version: "2.3.0".into(),
            version_scheme: 0,
            sha256: "b".repeat(64),
            requested: false,
            keg_only: false,
            conflicts_with: Vec::new(),
            dependencies: Vec::new(),
        },
    ];
    (colima_bin, lima_bin, installed)
}

fn seed_keg(prefix: &Path, name: &str, version: &str, member: &str) -> PathBuf {
    let keg = prefix.join("Cellar").join(name).join(version);
    let bin = keg.join("bin");
    fs::create_dir_all(&bin).unwrap();
    let executable = bin.join(member);
    fs::write(&executable, b"#!/bin/sh\nexit 0\n").unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();
    fs::create_dir_all(prefix.join("opt")).unwrap();
    let target = PathBuf::from("../Cellar").join(name).join(version);
    std::os::unix::fs::symlink(&target, prefix.join("opt").join(name)).unwrap();
    executable
}

fn metadata(service: Value) -> InstalledServiceMetadata {
    InstalledServiceMetadata {
        name: "zellij".into(),
        version: "0.46.0".into(),
        service,
    }
}

fn zellij_service() -> Value {
    json!({
        "run": ["$HOMEBREW_PREFIX/opt/zellij/bin/zellij", "web"],
        "run_type": "immediate",
        "keep_alive": { "always": true },
        "environment_variables": {
            "PATH": "$HOMEBREW_PREFIX/bin:$HOMEBREW_PREFIX/sbin:/usr/bin:/bin:/usr/sbin:/sbin"
        },
        "log_path": "$HOMEBREW_PREFIX/var/log/zellij.log",
        "error_log_path": "$HOMEBREW_PREFIX/var/log/zellij.log"
    })
}

#[test]
fn colima_plan_builds_isolated_preview_for_space_bearing_prefix() {
    let temp = tempfile::tempdir().unwrap();
    let prefix = temp.path().join("Application Support/brewrs");
    fs::create_dir_all(&prefix).unwrap();
    let canonical = prefix.canonicalize().unwrap();
    let (colima_bin, lima_bin, installed) = seed_colima_prefix(&canonical);

    let plan = colima_plan(&canonical, &installed, &[]).unwrap();
    assert!(plan.label.starts_with("local.brewrs."));
    assert!(plan.label.ends_with(".colima"));
    assert_eq!(
        plan.label.len(),
        "local.brewrs.".len() + 16 + ".colima".len()
    );
    assert!(plan.profile.starts_with("brewrs-"));
    assert_eq!(plan.profile.len(), "brewrs-".len() + 8);
    assert_eq!(plan.colima, colima_bin);
    assert_eq!(plan.lima, lima_bin);
    assert_eq!(
        plan.home,
        canonical.join(".brewrs/service-trials/colima/home")
    );
    assert_eq!(plan.colima_home, plan.home.join(".colima"));
    assert_eq!(plan.lima_home, plan.colima_home.join("_lima"));
    assert_eq!(
        plan.path,
        format!(
            "{}:/usr/bin:/bin:/usr/sbin:/sbin",
            lima_bin.parent().unwrap().display()
        )
    );
    // Same prefix produces the exact same deterministic plan.
    let again = colima_plan(&canonical, &installed, &[]).unwrap();
    assert_eq!(again.label, plan.label);
    assert_eq!(again.profile, plan.profile);
}

#[test]
fn colima_plan_rejects_unreviewed_versions_missing_pins_and_bad_binaries() {
    let temp = tempfile::tempdir().unwrap();
    let prefix = temp.path().join("prefix");
    fs::create_dir_all(&prefix).unwrap();
    let canonical = prefix.canonicalize().unwrap();
    let (colima_bin, lima_bin, mut installed) = seed_colima_prefix(&canonical);

    // Unreviewed version
    installed[0].version = "0.10.2".into();
    let err = colima_plan(&canonical, &installed, &[]).err().unwrap();
    assert!(err.to_string().contains("0.10.2"));
    installed[0].version = "0.10.3".into();

    // Missing lima pin
    installed[0].dependencies.clear();
    let err = colima_plan(&canonical, &installed, &[]).err().unwrap();
    assert!(err.to_string().contains("Lima"));
    installed[0].dependencies.push(Dependency {
        name: "lima".into(),
        version: "2.3.0".into(),
        sha256: "b".repeat(64),
    });

    // Non-executable binary
    fs::set_permissions(&colima_bin, fs::Permissions::from_mode(0o644)).unwrap();
    let err = colima_plan(&canonical, &installed, &[]).err().unwrap();
    assert!(err.to_string().contains("not executable"));
    fs::set_permissions(&colima_bin, fs::Permissions::from_mode(0o755)).unwrap();

    // Symlinked executable
    let other = temp.path().join("other");
    fs::write(&other, b"#!/bin/sh\n").unwrap();
    fs::remove_file(&lima_bin).unwrap();
    std::os::unix::fs::symlink(&other, &lima_bin).unwrap();
    let err = colima_plan(&canonical, &installed, &[]).err().unwrap();
    assert!(err.to_string().contains("regular managed executable"));
    fs::remove_file(&lima_bin).unwrap();
    fs::write(&lima_bin, b"#!/bin/sh\nexit 0\n").unwrap();
    fs::set_permissions(&lima_bin, fs::Permissions::from_mode(0o755)).unwrap();

    // Existing trial directory
    let trial = canonical.join(".brewrs/service-trials/colima");
    fs::create_dir_all(&trial).unwrap();
    let err = colima_plan(&canonical, &installed, &[]).err().unwrap();
    assert!(err.to_string().contains("already exists"));
    fs::remove_dir_all(canonical.join(".brewrs")).unwrap();

    // Symlinked service-trials parent
    fs::create_dir_all(canonical.join(".brewrs")).unwrap();
    let outside = temp.path().join("outside");
    fs::create_dir_all(&outside).unwrap();
    std::os::unix::fs::symlink(&outside, canonical.join(".brewrs/service-trials")).unwrap();
    let err = colima_plan(&canonical, &installed, &[]).err().unwrap();
    assert!(err.to_string().contains("not a managed directory"));
}

#[test]
fn colima_plan_rejects_colon_in_prefix() {
    let temp = tempfile::tempdir().unwrap();
    let prefix = temp.path().join("bad:prefix");
    fs::create_dir_all(&prefix).unwrap();
    let canonical = prefix.canonicalize().unwrap();
    let (_, _, installed) = seed_colima_prefix(&canonical);

    let err = colima_plan(&canonical, &installed, &[]).err().unwrap();
    assert!(err.to_string().contains("colons"));
}

#[test]
fn general_preview_pins_opt_executable_and_expands_bounded_literals() {
    let temp = tempfile::tempdir().unwrap();
    let prefix = temp.path().join("Application Support/brewrs");
    fs::create_dir_all(&prefix).unwrap();
    let canonical = prefix.canonicalize().unwrap();
    let executable = seed_keg(&canonical, "zellij", "0.46.0", "zellij");

    let plan =
        ServicePreview::from_metadata(&canonical, "zellij", "0.46.0", &metadata(zellij_service()))
            .unwrap();
    assert_eq!(plan.executable, executable);
    assert!(plan.used_opt_alias);
    assert_eq!(
        plan.declared_argv,
        [
            "$HOMEBREW_PREFIX/opt/zellij/bin/zellij".to_string(),
            "web".to_string()
        ]
    );
    assert_eq!(
        plan.expanded_argv[0],
        canonical.join("opt/zellij/bin/zellij").to_str().unwrap()
    );
    assert_eq!(plan.expanded_argv[1], "web");
    assert_eq!(
        plan.environment,
        [(
            "PATH".to_string(),
            format!(
                "{}/bin:{}/sbin:/usr/bin:/bin:/usr/sbin:/sbin",
                canonical.display(),
                canonical.display()
            )
        )]
    );
    assert_eq!(plan.unmodelled, ["keep_alive"]);
    assert!(plan.label.starts_with("local.brewrs."));
    assert!(plan.label.ends_with(".zellij"));
    assert_eq!(plan.run_type.as_deref(), Some("immediate"));
    assert_eq!(plan.log_path, Some(canonical.join("var/log/zellij.log")));
}

#[test]
fn general_preview_rejects_a_mutable_opt_alias_and_outside_executables() {
    let temp = tempfile::tempdir().unwrap();
    let prefix = temp.path().join("prefix");
    fs::create_dir_all(&prefix).unwrap();
    let canonical = prefix.canonicalize().unwrap();
    seed_keg(&canonical, "zellij", "0.46.0", "zellij");
    fs::remove_file(canonical.join("opt/zellij")).unwrap();
    std::os::unix::fs::symlink("../Cellar/zellij/0.1.0", canonical.join("opt/zellij")).unwrap();
    let err =
        ServicePreview::from_metadata(&canonical, "zellij", "0.46.0", &metadata(zellij_service()))
            .err()
            .unwrap();
    assert!(err.to_string().contains("pinned keg"), "{err}");

    fs::remove_file(canonical.join("opt/zellij")).unwrap();
    let mut outside = zellij_service();
    outside["run"] = json!(["$HOMEBREW_PREFIX/bin/zellij", "web"]);
    let err = ServicePreview::from_metadata(&canonical, "zellij", "0.46.0", &metadata(outside))
        .err()
        .unwrap();
    assert!(err.to_string().contains("outside the pinned keg"), "{err}");
}

#[test]
fn general_preview_rejects_symlinked_ancestors_and_bad_binaries() {
    let temp = tempfile::tempdir().unwrap();
    let prefix = temp.path().join("prefix");
    fs::create_dir_all(&prefix).unwrap();
    let canonical = prefix.canonicalize().unwrap();
    let executable = seed_keg(&canonical, "zellij", "0.46.0", "zellij");

    fs::set_permissions(&executable, fs::Permissions::from_mode(0o644)).unwrap();
    let err =
        ServicePreview::from_metadata(&canonical, "zellij", "0.46.0", &metadata(zellij_service()))
            .err()
            .unwrap();
    assert!(err.to_string().contains("not executable"), "{err}");
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();

    let outside = temp.path().join("outside");
    fs::write(&outside, b"#!/bin/sh\n").unwrap();
    fs::remove_file(&executable).unwrap();
    std::os::unix::fs::symlink(&outside, &executable).unwrap();
    let err =
        ServicePreview::from_metadata(&canonical, "zellij", "0.46.0", &metadata(zellij_service()))
            .err()
            .unwrap();
    assert!(err.to_string().contains("not a symlink"), "{err}");
    fs::remove_file(&executable).unwrap();
    fs::write(&executable, b"#!/bin/sh\n").unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();

    let keg = canonical.join("Cellar/zellij/0.46.0");
    let moved = temp.path().join("moved-keg");
    fs::rename(&keg, &moved).unwrap();
    fs::remove_dir(canonical.join("Cellar/zellij")).unwrap();
    std::os::unix::fs::symlink(&moved, canonical.join("Cellar/zellij")).unwrap();
    let err =
        ServicePreview::from_metadata(&canonical, "zellij", "0.46.0", &metadata(zellij_service()))
            .err()
            .unwrap();
    assert!(err.to_string().contains("not a real directory"), "{err}");
}

#[test]
fn general_preview_rejects_shell_strings_and_linux_only_run_declarations() {
    let temp = tempfile::tempdir().unwrap();
    let canonical = temp.path().canonicalize().unwrap();
    let mut shell = zellij_service();
    shell["run"] = json!("zellij web");
    let err = ServicePreview::from_metadata(&canonical, "zellij", "0.46.0", &metadata(shell))
        .err()
        .unwrap();
    assert!(err.to_string().contains("shell command string"), "{err}");

    let mut linux = zellij_service();
    linux["run"] = json!({ "linux": ["/usr/bin/zellij", "web"] });
    let err = ServicePreview::from_metadata(&canonical, "zellij", "0.46.0", &metadata(linux))
        .err()
        .unwrap();
    assert!(err.to_string().contains("no macOS entry"), "{err}");
}

#[test]
fn general_preview_refuses_unknown_placeholders_and_root_services() {
    let temp = tempfile::tempdir().unwrap();
    let canonical = temp.path().canonicalize().unwrap();
    fs::create_dir_all(&canonical).unwrap();
    seed_keg(&canonical, "zellij", "0.46.0", "zellij");
    let mut home = zellij_service();
    home["run"] = json!(["$HOME/bin/zellij", "web"]);
    let err = ServicePreview::from_metadata(&canonical, "zellij", "0.46.0", &metadata(home))
        .err()
        .unwrap();
    assert!(err.to_string().contains("does not model"), "{err}");

    let mut inherited = zellij_service();
    inherited["environment_variables"] = json!({ "PATH": "$PATH:/usr/bin" });
    let err = ServicePreview::from_metadata(&canonical, "zellij", "0.46.0", &metadata(inherited))
        .err()
        .unwrap();
    assert!(err.to_string().contains("does not model"), "{err}");

    let mut root = zellij_service();
    root["require_root"] = json!(true);
    let err = ServicePreview::from_metadata(&canonical, "zellij", "0.46.0", &metadata(root))
        .err()
        .unwrap();
    assert!(err.to_string().contains("requires root"), "{err}");
}

#[test]
fn general_preview_notes_every_unmodelled_key() {
    let temp = tempfile::tempdir().unwrap();
    let canonical = temp.path().canonicalize().unwrap();
    seed_keg(&canonical, "zellij", "0.46.0", "zellij");
    let mut service = zellij_service();
    service["process_type"] = json!("Background");
    service["launch_only_once"] = json!(true);
    service["sockets"] = json!({ "Listeners": { "SockPathName": "/tmp/zellij.sock" } });
    let plan =
        ServicePreview::from_metadata(&canonical, "zellij", "0.46.0", &metadata(service)).unwrap();
    assert_eq!(
        plan.unmodelled,
        ["keep_alive", "launch_only_once", "process_type", "sockets"]
    );
}

#[test]
fn general_preview_bounds_arguments_and_environment() {
    let temp = tempfile::tempdir().unwrap();
    let canonical = temp.path().canonicalize().unwrap();
    seed_keg(&canonical, "zellij", "0.46.0", "zellij");

    let mut too_many_args = zellij_service();
    let mut argv = vec![json!("$HOMEBREW_PREFIX/opt/zellij/bin/zellij")];
    argv.extend((0..MAX_SERVICE_ARGS).map(|_| json!("arg")));
    too_many_args["run"] = Value::Array(argv);
    let err =
        ServicePreview::from_metadata(&canonical, "zellij", "0.46.0", &metadata(too_many_args))
            .err()
            .unwrap();
    assert!(err.to_string().contains("outside the supported"), "{err}");

    let mut long_arg = zellij_service();
    long_arg["run"] = json!([
        "$HOMEBREW_PREFIX/opt/zellij/bin/zellij",
        "x".repeat(MAX_SERVICE_LITERAL_LEN + 1)
    ]);
    let err = ServicePreview::from_metadata(&canonical, "zellij", "0.46.0", &metadata(long_arg))
        .err()
        .unwrap();
    assert!(err.to_string().contains("longer than"), "{err}");

    let mut too_many_variables = zellij_service();
    let variables: Map<String, Value> = (0..=MAX_SERVICE_ENV)
        .map(|index| (format!("VAR_{index}"), json!("value")))
        .collect();
    too_many_variables["environment_variables"] = Value::Object(variables);
    let err = ServicePreview::from_metadata(
        &canonical,
        "zellij",
        "0.46.0",
        &metadata(too_many_variables),
    )
    .err()
    .unwrap();
    assert!(err.to_string().contains("environment variables"), "{err}");
}

#[test]
fn general_preview_observes_working_directories_without_creating_them() {
    let temp = tempfile::tempdir().unwrap();
    let canonical = temp.path().canonicalize().unwrap();
    seed_keg(&canonical, "zellij", "0.46.0", "zellij");

    let mut present = zellij_service();
    present["working_dir"] = json!("$HOMEBREW_PREFIX");
    let plan =
        ServicePreview::from_metadata(&canonical, "zellij", "0.46.0", &metadata(present)).unwrap();
    assert_eq!(
        plan.working_directory,
        Some((canonical.clone(), DirectoryObservation::Present))
    );

    let mut absent = zellij_service();
    absent["working_dir"] = json!("$HOMEBREW_PREFIX/var");
    let plan =
        ServicePreview::from_metadata(&canonical, "zellij", "0.46.0", &metadata(absent)).unwrap();
    assert_eq!(
        plan.working_directory,
        Some((canonical.join("var"), DirectoryObservation::Absent))
    );
    assert!(!canonical.join("var").exists());

    let mut outside = zellij_service();
    outside["working_dir"] = json!("/tmp");
    let plan =
        ServicePreview::from_metadata(&canonical, "zellij", "0.46.0", &metadata(outside)).unwrap();
    assert_eq!(
        plan.working_directory,
        Some((PathBuf::from("/tmp"), DirectoryObservation::OutsidePrefix))
    );

    let mut escaping = zellij_service();
    escaping["working_dir"] = json!("$HOMEBREW_PREFIX/../escape");
    let err = ServicePreview::from_metadata(&canonical, "zellij", "0.46.0", &metadata(escaping))
        .err()
        .unwrap();
    assert!(err.to_string().contains("must not contain"), "{err}");
}

#[test]
fn general_preview_reads_os_mapped_service_names_but_never_registers_them() {
    let temp = tempfile::tempdir().unwrap();
    let canonical = temp.path().canonicalize().unwrap();
    seed_keg(&canonical, "zellij", "0.46.0", "zellij");

    let mut mapped = zellij_service();
    mapped["name"] = json!({ "macos": "homebrew.mxcl.zellij" });
    let plan =
        ServicePreview::from_metadata(&canonical, "zellij", "0.46.0", &metadata(mapped)).unwrap();
    assert_eq!(plan.service_name.as_deref(), Some("homebrew.mxcl.zellij"));

    let mut linux_name = zellij_service();
    linux_name["name"] = json!({ "linux": "zellij.service" });
    let plan = ServicePreview::from_metadata(&canonical, "zellij", "0.46.0", &metadata(linux_name))
        .unwrap();
    assert_eq!(plan.service_name, None);

    let mut invalid = zellij_service();
    invalid["name"] = json!("bad name");
    let err = ServicePreview::from_metadata(&canonical, "zellij", "0.46.0", &metadata(invalid))
        .err()
        .unwrap();
    assert!(err.to_string().contains("launchd label"), "{err}");
}

#[test]
fn general_preview_requires_the_exact_relative_opt_link() {
    let temp = tempfile::tempdir().unwrap();
    let prefix = temp.path().join("prefix");
    fs::create_dir_all(&prefix).unwrap();
    let canonical = prefix.canonicalize().unwrap();
    seed_keg(&canonical, "zellij", "0.46.0", "zellij");
    let alias = canonical.join("opt/zellij");
    let keg = canonical.join("Cellar/zellij/0.46.0");

    // An absolute target reads as the same keg but is not the reviewed
    // relative link brewrs writes.
    fs::remove_file(&alias).unwrap();
    std::os::unix::fs::symlink(&keg, &alias).unwrap();
    let err =
        ServicePreview::from_metadata(&canonical, "zellij", "0.46.0", &metadata(zellij_service()))
            .err()
            .unwrap();
    assert!(err.to_string().contains("exact pinned keg link"), "{err}");

    // Extra `..` segments normalize to the same keg lexically, but the raw
    // target is still not the exact reviewed link.
    fs::remove_file(&alias).unwrap();
    std::os::unix::fs::symlink("../Cellar/zellij/0.46.0/../0.46.0", &alias).unwrap();
    let err =
        ServicePreview::from_metadata(&canonical, "zellij", "0.46.0", &metadata(zellij_service()))
            .err()
            .unwrap();
    assert!(err.to_string().contains("exact pinned keg link"), "{err}");

    // A symlinked intermediate could resolve to the keg too; the raw link
    // format must still be rejected rather than resolved.
    std::os::unix::fs::symlink("zellij", canonical.join("Cellar/current")).unwrap();
    fs::remove_file(&alias).unwrap();
    std::os::unix::fs::symlink("../Cellar/current/0.46.0", &alias).unwrap();
    let err =
        ServicePreview::from_metadata(&canonical, "zellij", "0.46.0", &metadata(zellij_service()))
            .err()
            .unwrap();
    assert!(err.to_string().contains("exact pinned keg link"), "{err}");
}

#[test]
fn placeholder_expansion_is_single_pass_and_token_bounded() {
    let temp = tempfile::tempdir().unwrap();
    // A prefix that itself contains a placeholder-looking string proves
    // inserted text is not rescanned.
    let prefix = temp.path().join("$HOMEBREW_CELLAR/prefix");
    let prefix_text = prefix.to_str().unwrap();
    assert_eq!(
        expand_literal(&prefix, "$HOMEBREW_PREFIX/bin", "run argument 0").unwrap(),
        prefix.join("bin").to_str().unwrap()
    );
    assert_eq!(
        expand_literal(&prefix, "${HOMEBREW_PREFIX}/bin", "run argument 0").unwrap(),
        prefix.join("bin").to_str().unwrap()
    );
    assert_eq!(
        expand_literal(&prefix, "$HOMEBREW_CELLAR/lib", "run argument 0").unwrap(),
        prefix.join("Cellar/lib").to_str().unwrap()
    );

    // An unbraced name must end at a token boundary.
    let err = expand_literal(&prefix, "$HOMEBREW_PREFIX_UNKNOWN", "run argument 0")
        .err()
        .unwrap();
    assert!(err.to_string().contains("does not model"), "{err}");
    assert!(expand_literal(&prefix, "$HOME/bin", "run argument 0").is_err());
    assert!(expand_literal(&prefix, "${HOMEBREW_REPOSITORY}", "run argument 0").is_err());
    assert!(expand_literal(&prefix, "trailing $", "run argument 0").is_err());

    // Expansion is bounded even when the literal itself is within bounds.
    let long_prefix = Path::new("/tmp").join("x".repeat(2048));
    let value = "$HOMEBREW_PREFIX/".repeat(20);
    let err = expand_literal(&long_prefix, &value, "run argument 0")
        .err()
        .unwrap();
    assert!(err.to_string().contains("expands beyond"), "{err}");
    assert!(prefix_text.contains("$HOMEBREW_CELLAR"));
}

#[test]
fn general_preview_rejects_unbounded_or_control_service_keys() {
    let temp = tempfile::tempdir().unwrap();
    let canonical = temp.path().canonicalize().unwrap();
    for key in [
        "bad key".to_string(),
        "run\n".to_string(),
        "k".repeat(MAX_SERVICE_KEY_LEN + 1),
    ] {
        let mut service = zellij_service();
        service[key.as_str()] = json!(1);
        let err = ServicePreview::from_metadata(&canonical, "zellij", "0.46.0", &metadata(service))
            .err()
            .unwrap();
        assert!(
            err.to_string().contains("unsupported key"),
            "{key:?}: {err}"
        );
    }
}

#[test]
fn state_roots_are_observed_without_following_symlinks() {
    let temp = tempfile::tempdir().unwrap();
    let prefix = temp.path().join("prefix");
    let observed = observe_state(&prefix, true).unwrap();
    assert!(observed.entries.is_empty());
    assert!(
        observed
            .roots
            .iter()
            .all(|root| root.inspected && !root.present)
    );

    fs::create_dir_all(prefix.join(SERVICE_STATE_ROOT).join("alpha")).unwrap();
    fs::create_dir_all(prefix.join(SERVICE_TRIAL_ROOT)).unwrap();
    fs::write(prefix.join(SERVICE_TRIAL_ROOT).join("colima"), b"x").unwrap();
    let observed = observe_state(&prefix, true).unwrap();
    assert!(observed.roots.iter().all(|root| root.present));
    assert_eq!(
        observed
            .entries
            .iter()
            .map(|entry| (entry.root, entry.name.as_str(), entry.kind))
            .collect::<Vec<_>>(),
        [
            ("service-state", "alpha", "directory"),
            ("service-trial", "colima", "file")
        ]
    );

    let outside = temp.path().join("outside");
    fs::create_dir_all(&outside).unwrap();
    fs::remove_dir_all(prefix.join(SERVICE_STATE_ROOT)).unwrap();
    std::os::unix::fs::symlink(&outside, prefix.join(SERVICE_STATE_ROOT)).unwrap();
    let err = observe_state(&prefix, true).err().unwrap();
    assert!(err.to_string().contains("symlink"), "{err}");

    let unowned = observe_state(&prefix, false).unwrap();
    assert!(unowned.entries.is_empty());
    assert!(unowned.roots.iter().all(|root| !root.inspected));
}

#[test]
fn state_roots_are_bounded() {
    let temp = tempfile::tempdir().unwrap();
    let prefix = temp.path().join("prefix");
    let root = prefix.join(SERVICE_STATE_ROOT);
    fs::create_dir_all(&root).unwrap();
    for index in 0..=MAX_SERVICE_LIST_ENTRIES {
        fs::create_dir(root.join(format!("entry-{index:03}"))).unwrap();
    }
    let err = observe_state(&prefix, true).err().unwrap();
    assert!(err.to_string().contains("more than"), "{err}");
}

#[tokio::test]
async fn preview_rejects_packages_that_are_not_installed_without_network() {
    let temp = tempfile::tempdir().unwrap();
    let prefix = temp.path().join("empty");
    fs::create_dir_all(&prefix).unwrap();
    let err = preview(&Config::default(), Some(&prefix), "zellij")
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("initialized brewrs prefix"),
        "{err}"
    );
}
