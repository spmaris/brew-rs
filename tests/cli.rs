//! Offline integration tests for the `brewrs` command line.
//!
//! Every test runs the real binary with a sentinel `brew` executable at the
//! front of `PATH`. The sentinel records its argv and exits with a distinctive
//! failure, so a command that unexpectedly shells out to Homebrew is detected
//! instead of touching a real installation. No Homebrew installation, network
//! access, or real package state is involved.
//!
//! Configuration is redirected to a throwaway path through `BREW_RS_CONFIG`,
//! and `HOME`/XDG directories are isolated as well, so the tests never touch
//! the user's real brew-rs configuration.

#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use tempfile::TempDir;

/// Sentinel Homebrew. It records one block per invocation and fails, so tests
/// can prove that no command reaches Homebrew and a regression is loud instead
/// of silently managing the host's packages.
const SENTINEL_BREW: &str = r#"#!/bin/sh
set -u

log=${BREW_SENTINEL_LOG:?BREW_SENTINEL_LOG must be set}

{
    printf 'BEGIN\n'
    for arg in "$@"; do
        printf 'arg\037%s\n' "$arg"
    done
    printf 'END\n'
} >> "$log"

printf 'sentinel brew: brewrs must not invoke Homebrew in this test\n' >&2
exit "${BREW_SENTINEL_EXIT:-97}"
"#;

struct Sandbox {
    _root: TempDir,
    bin: PathBuf,
    log: PathBuf,
    config: PathBuf,
    home: PathBuf,
}

impl Sandbox {
    fn new() -> Self {
        let root = tempfile::tempdir().expect("create sandbox directory");
        let bin = root.path().join("bin");
        let home = root.path().join("home");
        let tmp = home.join("tmp");
        for directory in [&bin, &home, &tmp] {
            fs::create_dir_all(directory).expect("create sandbox directory");
        }

        let sentinel = bin.join("brew");
        fs::write(&sentinel, SENTINEL_BREW).expect("write sentinel brew");
        let mut permissions = fs::metadata(&sentinel)
            .expect("stat sentinel brew")
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&sentinel, permissions).expect("chmod sentinel brew");

        let log = root.path().join("brew.log");
        let config = root.path().join("config").join("config.toml");

        Self {
            _root: root,
            bin,
            log,
            config,
            home,
        }
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_brewrs"));
        command
            .env_clear()
            .env(
                "PATH",
                format!("{}:/usr/bin:/bin:/usr/sbin:/sbin", self.bin.display()),
            )
            .env("HOME", &self.home)
            .env("XDG_CONFIG_HOME", self.home.join(".config"))
            .env("XDG_CACHE_HOME", self.home.join(".cache"))
            .env("XDG_DATA_HOME", self.home.join(".local").join("share"))
            .env("TMPDIR", self.home.join("tmp"))
            .env("BREW_RS_CONFIG", &self.config)
            .env("BREW_RS_LOG", "off")
            .env("BREW_RS_LOG_STYLE", "never")
            .env("BREW_SENTINEL_LOG", &self.log)
            .env("LANG", "C")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    }

    fn run(&self, args: &[&str]) -> Output {
        self.run_with_env(args, &[])
    }

    fn run_with_env(&self, args: &[&str], environment: &[(&str, &str)]) -> Output {
        let mut command = self.command();
        command.args(args);
        for (key, value) in environment {
            command.env(key, value);
        }
        command.output().expect("failed to run brewrs")
    }

    /// Every sentinel `brew` invocation, in order, with the arguments it saw.
    fn brew_invocations(&self) -> Vec<Vec<String>> {
        let log = fs::read_to_string(&self.log).unwrap_or_default();
        let mut invocations = Vec::new();
        let mut current: Option<Vec<String>> = None;

        for line in log.lines() {
            match line {
                "BEGIN" => current = Some(Vec::new()),
                "END" => {
                    if let Some(args) = current.take() {
                        invocations.push(args);
                    }
                }
                _ => {
                    let Some(args) = current.as_mut() else {
                        continue;
                    };
                    if let Some(arg) = line.strip_prefix("arg\u{1f}") {
                        args.push(arg.to_string());
                    }
                }
            }
        }

        invocations
    }

    fn assert_no_brew(&self) {
        let invocations = self.brew_invocations();
        assert!(
            invocations.is_empty(),
            "brewrs must not invoke brew, observed {invocations:#?}"
        );
    }
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn assert_success(output: &Output) {
    assert_eq!(
        output.status.code(),
        Some(0),
        "command failed\nstdout:\n{}\nstderr:\n{}",
        stdout(output),
        stderr(output)
    );
}

/// Assert that a single output row mentions `name` and every expected value,
/// so the values are associated with the package rather than merely present
/// somewhere in the output.
fn assert_row_containing(output: &str, name: &str, expected: &[&str]) {
    let row = output
        .lines()
        .find(|line| line.contains(name))
        .unwrap_or_else(|| panic!("no output row for {name:?} in:\n{output}"));
    for value in expected {
        assert!(
            row.contains(value),
            "row for {name:?} is missing {value:?}: {row:?}"
        );
    }
}

/// Guard the guard: the sentinel must really record argv and fail, so every
/// `assert_no_brew` in this file reflects an executable check rather than an
/// absent binary on `PATH`.
#[test]
fn sentinel_brew_records_invocations_and_fails() {
    let sandbox = Sandbox::new();
    let output = Command::new(sandbox.bin.join("brew"))
        .args(["info", "--json=v2", "--installed"])
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("BREW_SENTINEL_LOG", &sandbox.log)
        .stdin(Stdio::null())
        .output()
        .expect("run the sentinel brew");
    assert_eq!(output.status.code(), Some(97));
    assert_eq!(
        sandbox.brew_invocations(),
        vec![vec![
            "info".to_string(),
            "--json=v2".to_string(),
            "--installed".to_string(),
        ]]
    );
}

#[test]
fn retired_legacy_flag_is_rejected_without_running_brew() {
    let sandbox = Sandbox::new();
    let prefix = sandbox.home.join("native-prefix");
    let path = prefix.to_str().unwrap();

    for args in [
        vec!["--legacy", "list"],
        vec!["list", "--legacy"],
        vec!["--legacy", "install", "--dry-run", "hello"],
        vec!["--legacy", "update"],
        vec!["--prefix", path, "--legacy", "list"],
        vec!["native", "--legacy", "list"],
    ] {
        let output = sandbox.run(&args);
        assert_ne!(
            output.status.code(),
            Some(0),
            "retired --legacy must be rejected for {args:?}\nstdout:\n{}\nstderr:\n{}",
            stdout(&output),
            stderr(&output)
        );
        assert!(
            stderr(&output).to_ascii_lowercase().contains("legacy"),
            "the rejection for {args:?} should name --legacy:\n{}",
            stderr(&output)
        );
    }

    sandbox.assert_no_brew();
    assert!(
        !prefix.exists(),
        "a rejected --legacy command must not create the native prefix"
    );
    assert!(
        !sandbox.config.exists(),
        "a rejected command must not create configuration"
    );
}

#[test]
fn unsupported_upstream_flags_are_rejected_without_running_brew() {
    let sandbox = Sandbox::new();

    for (args, flag) in [
        (vec!["install", "--dry-run", "--force", "hello"], "--force"),
        (vec!["install", "--dry-run", "--head", "hello"], "--head"),
        (
            vec!["install", "--dry-run", "--no-deps", "hello"],
            "--no-deps",
        ),
        (
            vec!["install", "--dry-run", "--appdir", "/tmp", "hello"],
            "--appdir",
        ),
        (
            vec!["remove", "--dry-run", "--cask", "--autoremove", "firefox"],
            "--autoremove",
        ),
        (
            vec!["upgrade", "--dry-run", "--formula", "--greedy", "hello"],
            "--greedy",
        ),
        (vec!["check-update", "--formula", "--greedy"], "--greedy"),
        (vec!["list", "--greedy"], "--greedy"),
        (vec!["list", "--sizes"], "--sizes"),
        (vec!["list", "--dates"], "--dates"),
    ] {
        let output = sandbox.run(&args);
        assert_ne!(
            output.status.code(),
            Some(0),
            "unsupported {flag} must be rejected for {args:?}\nstdout:\n{}\nstderr:\n{}",
            stdout(&output),
            stderr(&output)
        );
        assert!(
            stderr(&output).contains(flag),
            "the rejection for {args:?} should name {flag}:\n{}",
            stderr(&output)
        );
    }

    sandbox.assert_no_brew();
    assert!(
        !sandbox.config.exists(),
        "a rejected upstream flag must not create configuration"
    );
}

#[test]
fn help_names_brewrs_without_creating_config() {
    let sandbox = Sandbox::new();
    let output = sandbox.run(&["--help"]);
    assert_success(&output);
    assert!(stdout(&output).contains("Usage: brewrs"));
    assert!(!sandbox.config.exists());
    sandbox.assert_no_brew();
}

#[test]
fn invalid_config_is_preserved_until_explicit_reset() {
    let sandbox = Sandbox::new();
    fs::create_dir_all(sandbox.config.parent().unwrap()).unwrap();
    let invalid = "not valid TOML";
    fs::write(&sandbox.config, invalid).unwrap();
    let failed = sandbox.run(&["list"]);
    assert_eq!(failed.status.code(), Some(1));
    assert_eq!(fs::read_to_string(&sandbox.config).unwrap(), invalid);
    sandbox.assert_no_brew();
    assert_success(&sandbox.run(&["config", "reset"]));
    assert_success(&sandbox.run(&["config", "show"]));
    sandbox.assert_no_brew();
}

#[test]
fn migration_audit_reads_inventory_without_using_brew_or_writing() {
    let sandbox = Sandbox::new();
    let source = sandbox.home.join("homebrew-source");
    let dest = sandbox.home.join("unused-brewrs-destination");
    let launchd = sandbox.home.join("launchd");
    for path in [
        source.join("Cellar/ripgrep/16.0.0"),
        source.join("Cellar/example-tool/1.3"),
        source.join("Caskroom/example-browser/2.0"),
        source.join("Library/Taps/example/homebrew-tools"),
        launchd.clone(),
    ] {
        fs::create_dir_all(path).unwrap();
    }
    let receipt = source.join("Cellar/ripgrep/16.0.0/INSTALL_RECEIPT.json");
    let original = r#"{"installed_on_request":true,"source":{"tap":"homebrew/core"}}"#;
    fs::write(&receipt, original).unwrap();
    fs::write(
        source.join("Cellar/example-tool/1.3/INSTALL_RECEIPT.json"),
        r#"{"installed_on_request":false,"source":{"tap":"example/tools"}}"#,
    )
    .unwrap();
    fs::write(
        launchd.join("homebrew.mxcl.colima.plist"),
        b"fixture: file existence only",
    )
    .unwrap();
    fs::write(launchd.join("com.example.unrelated.plist"), b"unrelated").unwrap();

    let output = sandbox.run(&[
        "migrate",
        "--brew-prefix",
        source.to_str().unwrap(),
        "--prefix",
        dest.to_str().unwrap(),
        "--service-dir",
        launchd.to_str().unwrap(),
    ]);
    assert_success(&output);
    let report = stdout(&output);
    assert_row_containing(&report, "ripgrep", &["16.0.0", "requested", "Unchecked"]);
    assert_row_containing(&report, "example-tool", &["1.3", "dependency", "Blocked"]);
    assert_row_containing(&report, "example-browser", &["2.0", "Unchecked"]);
    assert!(report.contains("example/tools"));
    assert!(report.contains("homebrew.mxcl.colima"));
    assert!(!report.contains("com.example.unrelated"));
    assert!(report.contains("running state not checked"));
    assert_eq!(fs::read_to_string(receipt).unwrap(), original);
    assert!(!dest.exists() && !sandbox.config.exists());
    assert!(!sandbox.home.join(".cache").exists());
    assert!(!sandbox.home.join("Library/Caches").exists());
    sandbox.assert_no_brew();
}

#[test]
fn migration_audit_rejects_overlapping_prefixes() {
    let sandbox = Sandbox::new();
    let source = sandbox.home.join("homebrew-source");
    let output = sandbox.run(&[
        "migrate",
        "--brew-prefix",
        source.to_str().unwrap(),
        "--prefix",
        source.join("not-separate").to_str().unwrap(),
    ]);
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("must not overlap"));
    assert!(!source.exists());
    sandbox.assert_no_brew();
}

#[test]
fn offline_catalog_search_does_not_launch_brew() {
    let sandbox = Sandbox::new();
    assert_success(&sandbox.run(&["config", "reset"]));
    let mut config: toml::Value =
        toml::from_str(&fs::read_to_string(&sandbox.config).unwrap()).unwrap();
    let cache = sandbox.home.join("catalog-cache");
    config["storage"]["cache_dir"] = toml::Value::String(cache.to_string_lossy().into_owned());
    fs::write(&sandbox.config, toml::to_string(&config).unwrap()).unwrap();
    let catalogs = cache.join("catalogs");
    fs::create_dir_all(&catalogs).unwrap();
    fs::write(
        catalogs.join("casks.json"),
        r#"[{"token":"example-browser@beta","name":["Example Browser Beta"],"version":"2.4","desc":"Browser"}]"#,
    ).unwrap();

    let output = sandbox.run(&["search", "--offline", "--cask", "Example Browser"]);
    assert_success(&output);
    assert_row_containing(
        &stdout(&output),
        "example-browser@beta",
        &["Example Browser Beta", "2.4"],
    );
    sandbox.assert_no_brew();
}

#[test]
fn native_list_does_not_create_a_missing_prefix_or_call_brew() {
    let sandbox = Sandbox::new();
    let prefix = sandbox.home.join("native-prefix");
    let path = prefix.to_str().unwrap();
    for args in [
        ["list", "--prefix", path].as_slice(),
        &["ls", "--prefix", path],
        &["native", "list", "--prefix", path],
    ] {
        let output = sandbox.run(args);
        assert_success(&output);
        assert!(stderr(&output).contains("No packages in"));
    }
    assert!(!prefix.exists());
    assert!(!sandbox.config.exists());
    sandbox.assert_no_brew();
}

#[test]
fn hidden_native_alias_stays_accepted_but_unlisted() {
    let sandbox = Sandbox::new();
    let help = sandbox.run(&["--help"]);
    assert_success(&help);
    assert!(
        !stdout(&help)
            .lines()
            .any(|line| line.starts_with("  native ")),
        "the hidden native alias must not be advertised:\n{}",
        stdout(&help)
    );

    let prefix = sandbox.home.join("native-prefix");
    let output = sandbox.run(&["native", "list", "--prefix", prefix.to_str().unwrap()]);
    assert_success(&output);
    assert!(stderr(&output).contains("No packages in"));
    assert!(!prefix.exists());
    sandbox.assert_no_brew();
}

#[test]
#[cfg(target_os = "macos")]
fn native_install_does_not_fall_back_to_brew_for_casks() {
    let sandbox = Sandbox::new();
    let prefix = sandbox.home.join("native-prefix");
    let output = sandbox.run(&[
        "native",
        "--prefix",
        prefix.to_str().unwrap(),
        "install",
        "--dry-run",
        "homebrew/cask/firefox",
    ]);
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("unsupported tap"));
    assert!(!prefix.exists());
    sandbox.assert_no_brew();
}

#[cfg(target_os = "macos")]
#[test]
fn native_upgrade_without_receipts_is_offline_and_read_only() {
    let sandbox = Sandbox::new();
    let prefix = sandbox.home.join("native-prefix");
    let path = prefix.to_str().unwrap();
    let output = sandbox.run(&["upgrade", "--prefix", path, "--dry-run"]);
    assert_success(&output);
    assert!(stderr(&output).contains("No packages installed yet."));
    let missing = sandbox.run(&["upgrade", "--prefix", path, "--yes", "hello"]);
    assert_eq!(missing.status.code(), Some(1));
    assert!(stderr(&missing).contains("'hello' is not installed"));
    assert!(!prefix.exists());
    assert!(!sandbox.config.exists());
    sandbox.assert_no_brew();
}

#[cfg(target_os = "macos")]
#[test]
fn default_update_queries_never_consult_homebrew_on_an_empty_prefix() {
    let sandbox = Sandbox::new();
    let prefix = sandbox.home.join("native-prefix");
    let path = prefix.to_str().unwrap();
    for args in [
        ["check-update", "--prefix", path].as_slice(),
        &["list", "--outdated", "--prefix", path],
    ] {
        let output = sandbox.run(args);
        assert_success(&output);
        assert!(stderr(&output).contains("No packages installed yet."));
    }
    assert!(!prefix.exists());
    assert!(!sandbox.config.exists());
    sandbox.assert_no_brew();
}

#[test]
fn native_refuses_a_prefix_inside_the_homebrew_prefix() {
    let sandbox = Sandbox::new();

    // A local stand-in for the Homebrew root: `HOMEBREW_PREFIX` is the
    // environment variable the store consults, so this exercises the refusal
    // without ever touching the host's real Homebrew prefix.
    let homebrew = sandbox.home.join("fake-homebrew-prefix");
    let protected = homebrew.join("brewrs-native");
    seed_native_graph(&protected, &[("hello", true, &[])]);
    let receipt = protected.join(".brewrs/state/receipts/hello.json");
    let keg = protected.join("Cellar/hello/1.0");

    let output = sandbox.run_with_env(
        &[
            "remove",
            "--prefix",
            protected.to_str().unwrap(),
            "hello",
            "--yes",
        ],
        &[("HOMEBREW_PREFIX", homebrew.to_str().unwrap())],
    );
    assert_eq!(
        output.status.code(),
        Some(1),
        "a prefix inside the Homebrew root must be refused\nstdout:\n{}\nstderr:\n{}",
        stdout(&output),
        stderr(&output)
    );
    let message = stderr(&output);
    assert!(
        message.contains("inside the Homebrew prefix"),
        "stderr should explain the protected-prefix refusal:\n{message}"
    );
    assert!(
        receipt.is_file() && keg.is_dir(),
        "a refused protected prefix must stay untouched"
    );

    // The same fixture outside the protected root is still manageable, which
    // proves the refusal is about the protected location rather than the
    // fixture shape.
    let safe = sandbox.home.join("safe-native");
    seed_native_graph(&safe, &[("hello", true, &[])]);
    assert_success(&sandbox.run(&[
        "remove",
        "--prefix",
        safe.to_str().unwrap(),
        "hello",
        "--yes",
    ]));
    assert!(!safe.join(".brewrs/state/receipts/hello.json").exists());

    sandbox.assert_no_brew();
}

fn seed_native_graph(prefix: &Path, packages: &[(&str, bool, &[&str])]) {
    let receipts = prefix.join(".brewrs/state/receipts");
    for directory in [
        &receipts,
        &prefix.join(".brewrs/stages"),
        &prefix.join("bin"),
        &prefix.join("Cellar"),
    ] {
        fs::create_dir_all(directory).unwrap();
    }
    fs::write(prefix.join(".brewrs-root"), "brewrs native store v1\n").unwrap();
    let identity =
        |name: &str| serde_json::json!({"name": name, "version": "1.0", "sha256": "a".repeat(64)});
    for &(name, requested, dependencies) in packages {
        let mut receipt = identity(name);
        receipt["requested"] = serde_json::json!(requested);
        receipt["dependencies"] = serde_json::json!(
            dependencies
                .iter()
                .copied()
                .map(identity)
                .collect::<Vec<_>>()
        );
        fs::create_dir_all(prefix.join("Cellar").join(name).join("1.0")).unwrap();
        fs::write(receipts.join(format!("{name}.json")), receipt.to_string()).unwrap();
    }
}

#[test]
fn colima_service_preview_uses_pinned_kegs_without_executing_them() {
    let sandbox = Sandbox::new();
    let prefix = sandbox.home.join("Colima trial prefix");
    seed_native_graph(&prefix, &[]);
    let identity = |name, version| serde_json::json!({"name": name, "version": version, "sha256": "a".repeat(64)});
    let mut colima = identity("colima", "0.10.3");
    colima["requested"] = serde_json::json!(true);
    colima["dependencies"] = serde_json::json!([identity("lima", "2.3.0")]);
    let mut lima = identity("lima", "2.3.0");
    lima["requested"] = serde_json::json!(false);
    let receipts = prefix.join(".brewrs/state/receipts");
    fs::write(receipts.join("colima.json"), colima.to_string()).unwrap();
    fs::write(receipts.join("lima.json"), lima.to_string()).unwrap();

    let colima_bin = prefix.join("Cellar/colima/0.10.3/bin/colima");
    let lima_bin = prefix.join("Cellar/lima/2.3.0/bin/limactl");
    for executable in [&colima_bin, &lima_bin] {
        fs::create_dir_all(executable.parent().unwrap()).unwrap();
        fs::write(
            executable,
            b"#!/bin/sh\nprintf invoked > \"$HOME/service-invoked\"\nexit 98\n",
        )
        .unwrap();
        fs::set_permissions(executable, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let args = [
        "services",
        "plan",
        "colima",
        "--prefix",
        prefix.to_str().unwrap(),
    ];
    let output = sandbox.run(&args);
    assert_success(&output);
    let report = stdout(&output);
    assert!(report.contains("Proposed label"));
    assert!(report.contains("local.brewrs."));
    assert!(report.contains("Cellar/colima/0.10.3/bin/colima"));
    assert!(report.contains("Cellar/lima/2.3.0/bin/limactl"));
    assert!(report.contains("--foreground"));
    assert!(report.contains("--activate=false") && report.contains("--ssh-config=false"));
    assert!(report.contains("--mount=none") && report.contains("--template=false"));
    assert!(report.contains("enforces no sandbox"));
    assert!(report.contains("Working directory"));
    assert!(report.contains("service-trials/colima/home"));
    assert!(report.contains("COLIMA_HOME"));
    assert!(report.contains("COLIMA_CACHE_HOME"));
    assert!(report.contains("COLIMA_PROFILE"));
    assert!(report.contains("brewrs-"));
    assert!(report.contains("LIMA_HOME"));
    assert!(report.contains("PATH"));
    assert!(report.contains("Cellar/lima/2.3.0/bin:/usr/bin:/bin:/usr/sbin:/sbin"));
    assert!(!report.contains("/opt/homebrew"));
    assert!(!sandbox.home.join("service-invoked").exists());
    assert!(!prefix.join(".brewrs/service-trials").exists());
    assert!(!sandbox.config.exists());

    lima["sha256"] = serde_json::json!("b".repeat(64));
    fs::write(receipts.join("lima.json"), lima.to_string()).unwrap();
    let failed = sandbox.run(&args);
    assert_eq!(failed.status.code(), Some(1));
    assert!(stderr(&failed).contains("unsatisfied receipt pins"));
    sandbox.assert_no_brew();
}

#[test]
fn reverse_dependencies_use_only_native_receipts() {
    let sandbox = Sandbox::new();
    let prefix = sandbox.home.join("native-prefix");
    let path = prefix.to_str().unwrap();
    seed_native_graph(
        &prefix,
        &[
            ("leaf", false, &[]),
            ("middle", false, &["leaf"]),
            ("program", true, &["middle"]),
        ],
    );
    let direct = sandbox.run(&["deps", "--prefix", path, "leaf", "--reverse"]);
    assert_success(&direct);
    assert!(stdout(&direct).contains("middle"));
    assert!(!stdout(&direct).contains("program"));
    let tree = sandbox.run(&["deps", "--prefix", path, "leaf", "--reverse", "--tree"]);
    assert_success(&tree);
    assert!(stdout(&tree).contains("middle"));
    assert!(stdout(&tree).contains("program"));
    assert!(!sandbox.config.exists());
    sandbox.assert_no_brew();
}

#[test]
fn reverse_dependencies_report_retained_kegs_without_claiming_active_pins() {
    let sandbox = Sandbox::new();
    let prefix = sandbox.home.join("native-prefix");
    seed_native_graph(
        &prefix,
        &[("leaf", false, &[]), ("program", true, &["leaf"])],
    );
    let active = prefix.join(".brewrs/state/receipts/leaf.json");
    let retained = prefix.join(".brewrs/state/retained/leaf/1.0.json");
    fs::create_dir_all(retained.parent().unwrap()).unwrap();
    fs::copy(&active, retained).unwrap();
    fs::create_dir_all(prefix.join("Cellar/leaf/2.0")).unwrap();
    fs::write(
        active,
        serde_json::json!({
            "name": "leaf", "version": "2.0", "sha256": "b".repeat(64), "requested": false
        })
        .to_string(),
    )
    .unwrap();

    let output = sandbox.run(&[
        "deps",
        "--prefix",
        prefix.to_str().unwrap(),
        "leaf",
        "--reverse",
    ]);
    assert_success(&output);
    let report = stdout(&output);
    let (active_part, retained_part) = report.split_once("Retained keg").unwrap();
    assert!(active_part.contains("Active keg") && active_part.contains("leaf 2.0"));
    assert!(!active_part.contains("program"));
    assert!(retained_part.contains("leaf 1.0") && retained_part.contains("program"));
    sandbox.assert_no_brew();
}

#[test]
fn native_autoremove_previews_and_preserves_live_dependencies() {
    let sandbox = Sandbox::new();
    let prefix = sandbox.home.join("native-prefix");
    let path = prefix.to_str().unwrap();
    let receipts = prefix.join(".brewrs/state/receipts");
    seed_native_graph(
        &prefix,
        &[
            ("app", true, &["shared", "app-only"]),
            ("keeper", true, &["shared"]),
            ("app-only", false, &[]),
            ("shared", false, &[]),
            ("orphan-tool", false, &[]),
        ],
    );
    let preview = sandbox.run(&["autoremove", "--prefix", path, "--dry-run"]);
    assert_success(&preview);
    assert_row_containing(&stdout(&preview), "orphan-tool", &["1.0", "remove"]);
    assert!(!stdout(&preview).contains("app-only"));
    assert!(receipts.join("orphan-tool.json").is_file());

    let remove = sandbox.run(&["remove", "--prefix", path, "app", "--autoremove", "--yes"]);
    assert_success(&remove);
    assert_row_containing(&stdout(&remove), "app-only", &["1.0", "remove"]);
    for name in ["app", "app-only"] {
        assert!(!receipts.join(format!("{name}.json")).exists());
        assert!(!prefix.join("Cellar").join(name).join("1.0").exists());
    }
    for name in ["keeper", "shared", "orphan-tool"] {
        assert!(receipts.join(format!("{name}.json")).is_file());
    }

    assert_success(&sandbox.run(&["autoremove", "--prefix", path, "--yes"]));
    assert!(!receipts.join("orphan-tool.json").exists());
    assert!(receipts.join("shared.json").is_file());
    assert!(receipts.join("keeper.json").is_file());
    assert!(!sandbox.config.exists());
    sandbox.assert_no_brew();
}

#[test]
fn native_remove_previews_and_removes_only_the_owned_package() {
    use std::os::unix::fs::symlink;

    let sandbox = Sandbox::new();
    let prefix = sandbox.home.join("native-prefix");
    let path = prefix.to_str().unwrap();
    let receipts = prefix.join(".brewrs/state/receipts");
    let executable = prefix.join("Cellar/hello/2.12.3/bin/hello");
    let bin = prefix.join("bin");
    for directory in [
        &receipts,
        &prefix.join(".brewrs/stages"),
        &bin,
        &executable.parent().unwrap().to_path_buf(),
    ] {
        fs::create_dir_all(directory).unwrap();
    }
    fs::write(prefix.join(".brewrs-root"), "brewrs native store v1\n").unwrap();
    fs::write(&executable, "an inert executable fixture\n").unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();
    let receipt = receipts.join("hello.json");
    fs::write(&receipt, serde_json::json!({"name":"hello", "version":"2.12.3", "sha256":"a".repeat(64), "requested":true}).to_string()).unwrap();
    let link = bin.join("hello");
    symlink("../Cellar/hello/2.12.3/bin/hello", &link).unwrap();
    let foreign = bin.join("custom-command");
    fs::write(&foreign, b"leave me alone").unwrap();

    let preview = sandbox.run(&["remove", "--prefix", path, "hello", "--dry-run"]);
    assert_success(&preview);
    assert_row_containing(&stdout(&preview), "hello", &["2.12.3", "remove"]);
    assert!(receipt.exists() && executable.exists() && link.is_symlink());

    let missing = sandbox.run(&[
        "native", "remove", "--prefix", path, "hello", "missing", "--yes",
    ]);
    assert_eq!(missing.status.code(), Some(1));
    assert!(stderr(&missing).contains("'missing' is not installed"));
    assert!(receipt.exists() && executable.exists() && link.is_symlink());

    let applied = sandbox.run(&[
        "native",
        "remove",
        "--prefix",
        path,
        "homebrew/core/hello",
        "--yes",
    ]);
    assert_success(&applied);
    assert!(!receipt.exists() && !executable.exists() && fs::symlink_metadata(&link).is_err());
    assert_eq!(fs::read(&foreign).unwrap(), b"leave me alone");
    sandbox.assert_no_brew();
}

#[test]
fn shellenv_prints_exports_without_creating_prefix_or_config() {
    let sandbox = Sandbox::new();
    let prefix = sandbox.home.join("Brew's Space Prefix");
    let path = prefix.to_str().unwrap();

    let posix = sandbox.run(&["shellenv", "--prefix", path, "--shell", "zsh"]);
    assert_success(&posix);
    let out = stdout(&posix);
    assert!(out.contains("export BREWRS_PREFIX="), "{out}");
    assert!(out.contains("Brew'\\''s Space Prefix"), "{out}");
    assert!(out.contains("export PATH="), "{out}");
    assert!(out.contains("/bin:"), "{out}");
    assert!(out.contains("/sbin"), "{out}");

    let fish = sandbox.run_with_env(
        &["shellenv", "--prefix", path],
        &[("SHELL", "/usr/bin/fish")],
    );
    assert_success(&fish);
    let fish_out = stdout(&fish);
    assert!(fish_out.contains("set -gx BREWRS_PREFIX "), "{fish_out}");
    assert!(fish_out.contains("fish_add_path -gP "), "{fish_out}");
    assert!(fish_out.contains("Brew\\'s Space Prefix"), "{fish_out}");

    assert!(!prefix.exists());
    assert!(!sandbox.config.exists());
    sandbox.assert_no_brew();
}

#[cfg(target_os = "macos")]
#[test]
fn native_cask_list_remove_and_upgrade_work_offline_without_brew() {
    use sha2::{Digest, Sha256};
    use std::os::unix::fs::symlink;

    let sandbox = Sandbox::new();
    let prefix = sandbox.home.join("native-cask-prefix");
    let path = prefix.to_str().unwrap();

    // Empty prefix checks for cask subcommands must stay offline and read-only.
    assert_success(&sandbox.run(&["upgrade", "--prefix", path, "--cask", "--dry-run"]));
    assert_success(&sandbox.run(&["check-update", "--prefix", path, "--greedy"]));
    assert!(!prefix.exists());

    // Seed an initialized store with a cask containing an .app, symlinked binary,
    // and command_wrapper script, plus a foreign file in bin/.
    let receipts = prefix.join(".brewrs/state/casks");
    let app_dir = prefix.join("Applications/Demo.app/Contents/MacOS");
    let caskroom_ver = prefix.join("Caskroom/demo-cask/1.2.3");
    let bin_dir = prefix.join("bin");
    for dir in [
        &receipts,
        &prefix.join(".brewrs/state/receipts"),
        &prefix.join(".brewrs/stages"),
        &prefix.join("Cellar"),
        &app_dir,
        &caskroom_ver,
        &bin_dir,
    ] {
        fs::create_dir_all(dir).unwrap();
    }
    fs::write(prefix.join(".brewrs-root"), "brewrs native store v1\n").unwrap();
    fs::write(
        prefix.join("Applications/Demo.app/Contents/Info.plist"),
        b"<plist/>",
    )
    .unwrap();
    fs::write(app_dir.join("demo-gui"), b"gui").unwrap();
    fs::write(caskroom_ver.join("demo-cli"), b"#!/bin/sh\necho cli\n").unwrap();
    fs::set_permissions(
        caskroom_ver.join("demo-cli"),
        fs::Permissions::from_mode(0o755),
    )
    .unwrap();

    let symlink_target = "../Caskroom/demo-cask/1.2.3/demo-cli";
    symlink(symlink_target, bin_dir.join("demo-cli")).unwrap();

    let wrapper_bytes = b"#!/bin/sh\nexec '/tmp/demo' \"$@\"\n";
    let wrapper_sha = hex::encode(Sha256::digest(wrapper_bytes));
    fs::write(bin_dir.join("demo-wrap"), wrapper_bytes).unwrap();
    fs::set_permissions(bin_dir.join("demo-wrap"), fs::Permissions::from_mode(0o755)).unwrap();

    let foreign = bin_dir.join("foreign-tool");
    fs::write(&foreign, b"keep").unwrap();

    let receipt = serde_json::json!({
        "token": "demo-cask",
        "version": "1.2.3",
        "sha256": "a".repeat(64),
        "auto_updates": false,
        "apps": [{"bundle_name": "Demo.app", "path": prefix.join("Applications/Demo.app")}],
        "caskroom_files": ["demo-cli"],
        "links": [{"dest": "bin/demo-cli", "target": symlink_target}],
        "wrappers": [{"dest": "bin/demo-wrap", "sha256": wrapper_sha}],
        "conflicts_with_casks": [],
        "conflicts_with_formulae": []
    });
    fs::write(receipts.join("demo-cask.json"), receipt.to_string()).unwrap();

    let list_all = sandbox.run(&["list", "--prefix", path]);
    assert_success(&list_all);
    assert_row_containing(
        &stdout(&list_all),
        "demo-cask",
        &["cask", "1.2.3", "homebrew", "requested"],
    );

    let list_casks = sandbox.run(&["list", "--prefix", path, "--cask"]);
    assert_success(&list_casks);
    assert_row_containing(&stdout(&list_casks), "demo-cask", &["1.2.3"]);

    let list_formulae = sandbox.run(&["list", "--prefix", path, "--formula"]);
    assert_success(&list_formulae);
    assert!(stderr(&list_formulae).contains("No packages in"));

    let dry_rm = sandbox.run(&[
        "remove",
        "--prefix",
        path,
        "--cask",
        "--dry-run",
        "demo-cask",
    ]);
    assert_success(&dry_rm);
    assert_row_containing(&stdout(&dry_rm), "demo-cask", &["1.2.3", "remove"]);
    assert!(receipts.join("demo-cask.json").is_file());

    let applied_rm = sandbox.run(&["remove", "--prefix", path, "--cask", "--yes", "demo-cask"]);
    assert_success(&applied_rm);
    assert!(
        stderr(&applied_rm).contains("Removed demo-cask\n")
            || stderr(&applied_rm).ends_with("Removed demo-cask"),
        "the result line should name the removed cask:\n{}",
        stderr(&applied_rm)
    );
    assert!(!receipts.join("demo-cask.json").exists());
    assert!(!prefix.join("Applications/Demo.app").exists());
    assert!(fs::symlink_metadata(bin_dir.join("demo-cli")).is_err());
    assert!(!bin_dir.join("demo-wrap").exists());
    assert!(!prefix.join("Caskroom/demo-cask").exists());
    assert_eq!(fs::read(&foreign).unwrap(), b"keep");

    sandbox.assert_no_brew();
}

#[cfg(target_os = "macos")]
#[test]
fn native_cask_rejects_protected_prefix_and_appdir() {
    let sandbox = Sandbox::new();
    let homebrew = sandbox.home.join("fake-homebrew");
    let safe_prefix = sandbox.home.join("safe-prefix");
    let protected_appdir = homebrew.join("Applications");
    fs::create_dir_all(&protected_appdir).unwrap();

    let bad_prefix = sandbox.run_with_env(
        &[
            "install",
            "--prefix",
            homebrew.join("brewrs-sub").to_str().unwrap(),
            "--cask",
            "--dry-run",
            "firefox",
        ],
        &[("HOMEBREW_PREFIX", homebrew.to_str().unwrap())],
    );
    assert_eq!(bad_prefix.status.code(), Some(1));
    assert!(stderr(&bad_prefix).contains("inside the Homebrew prefix"));

    let bad_appdir = sandbox.run_with_env(
        &[
            "install",
            "--prefix",
            safe_prefix.to_str().unwrap(),
            "--cask",
            "--appdir",
            protected_appdir.to_str().unwrap(),
            "--dry-run",
            "firefox",
        ],
        &[("HOMEBREW_PREFIX", homebrew.to_str().unwrap())],
    );
    assert_eq!(bad_appdir.status.code(), Some(1));
    assert!(stderr(&bad_appdir).contains("inside the Homebrew prefix"));
    assert!(!safe_prefix.exists());

    sandbox.assert_no_brew();
}

#[test]
fn install_and_upgrade_refuse_a_protected_prefix_before_planning_or_network() {
    let sandbox = Sandbox::new();
    let homebrew = sandbox.home.join("fake-homebrew");
    let protected = homebrew.join("brewrs-sub");
    let environment = [("HOMEBREW_PREFIX", homebrew.to_str().unwrap())];
    let prefix = protected.to_str().unwrap();

    // Each command would otherwise resolve metadata over the network (or fail
    // with a network error) before reaching the store; the refusal must come
    // first, so the output names the protected prefix and nothing was fetched.
    for args in [
        vec!["install", "--prefix", prefix, "--assumeyes", "jq"],
        vec![
            "install",
            "--prefix",
            prefix,
            "--assumeyes",
            "--cask",
            "alt-tab",
        ],
        vec!["upgrade", "--prefix", prefix, "--assumeyes"],
        vec!["upgrade", "--prefix", prefix, "--assumeyes", "--cask"],
    ] {
        let output = sandbox.run_with_env(&args, &environment);
        assert_eq!(
            output.status.code(),
            Some(1),
            "{args:?}\nstdout:\n{}\nstderr:\n{}",
            stdout(&output),
            stderr(&output)
        );
        let message = stderr(&output);
        assert!(
            message.contains("inside the Homebrew prefix"),
            "{args:?} should refuse the protected prefix first:\n{message}"
        );
        assert!(
            !message.contains("Download") && !stdout(&output).contains("Download"),
            "{args:?} must not download anything:\n{message}"
        );
        assert!(!protected.exists(), "{args:?} must not create the prefix");
    }

    sandbox.assert_no_brew();
}

#[test]
fn services_list_reports_only_observed_state_and_creates_nothing() {
    let sandbox = Sandbox::new();
    let prefix = sandbox.home.join("Service state prefix");
    let path = prefix.to_str().unwrap();

    // An absent prefix is reported as such without creating it or a config.
    let absent = sandbox.run(&["services", "list", "--prefix", path]);
    assert_success(&absent);
    let report = stdout(&absent);
    assert!(report.contains("No local service state or reviewed Colima receipt observed"));
    assert!(report.contains("not inspected (prefix is not an initialized brewrs store)"));
    assert!(!report.to_lowercase().contains("running"));
    assert!(!prefix.exists(), "services list must not create the prefix");
    assert!(!sandbox.config.exists());

    seed_native_graph(&prefix, &[]);
    let receipts = prefix.join(".brewrs/state/receipts");
    let identity = |name: &str, version: &str, sha: &str| serde_json::json!({"name": name, "version": version, "sha256": sha});
    let mut colima = identity("colima", "0.10.3", &"a".repeat(64));
    colima["requested"] = serde_json::json!(true);
    colima["dependencies"] = serde_json::json!([identity("lima", "2.3.0", &"b".repeat(64))]);
    let mut lima = identity("lima", "2.3.0", &"b".repeat(64));
    lima["requested"] = serde_json::json!(false);
    let mut zellij = identity("zellij", "0.46.0", &"c".repeat(64));
    zellij["requested"] = serde_json::json!(true);
    fs::write(receipts.join("colima.json"), colima.to_string()).unwrap();
    fs::write(receipts.join("lima.json"), lima.to_string()).unwrap();
    fs::write(receipts.join("zellij.json"), zellij.to_string()).unwrap();

    // The reviewed Colima lane is reported; an installed formula with no
    // recorded declaration is not turned into a fabricated service row.
    let empty = sandbox.run(&["services", "list", "--prefix", path]);
    assert_success(&empty);
    let report = stdout(&empty);
    assert!(report.contains("colima"));
    assert!(report.contains("0.10.3"));
    assert!(report.contains("reviewed trial (0.10.3)"));
    assert!(report.contains("no brewrs service state"));
    assert!(
        !report.contains("zellij"),
        "services list must not invent a service row for an installed formula whose declaration is not recorded"
    );
    assert!(!report.to_lowercase().contains("running"));

    // Observed state is reported literally, including entries this build did
    // not create, and is never labeled running or installed.
    fs::create_dir_all(prefix.join(".brewrs/services/colima")).unwrap();
    fs::create_dir_all(prefix.join(".brewrs/service-trials/colima")).unwrap();
    fs::write(prefix.join(".brewrs/services/orphan"), b"note").unwrap();
    let observed = sandbox.run(&["services", "list", "--prefix", path]);
    assert_success(&observed);
    let report = stdout(&observed);
    assert!(report.contains(".brewrs/services directory present (runtime not inspected)"));
    assert!(report.contains(".brewrs/service-trials directory present (runtime not inspected)"));
    assert!(report.contains("orphan"));
    assert!(report.contains("file under .brewrs/services (runtime not inspected)"));
    assert!(!report.to_lowercase().contains("running"));
    assert!(!sandbox.config.exists());
    sandbox.assert_no_brew();
}

#[test]
fn services_list_refuses_a_symlinked_state_root() {
    let sandbox = Sandbox::new();
    let prefix = sandbox.home.join("Service symlink prefix");
    seed_native_graph(&prefix, &[]);
    let outside = sandbox.home.join("outside-service-state");
    fs::create_dir_all(&outside).unwrap();
    std::os::unix::fs::symlink(&outside, prefix.join(".brewrs/services")).unwrap();

    let output = sandbox.run(&["services", "list", "--prefix", prefix.to_str().unwrap()]);
    assert_eq!(
        output.status.code(),
        Some(1),
        "stdout:\n{}\nstderr:\n{}",
        stdout(&output),
        stderr(&output)
    );
    assert!(stderr(&output).contains("must not be a symlink"));
    assert_eq!(fs::read_dir(&outside).unwrap().count(), 0);
    sandbox.assert_no_brew();
}

#[test]
fn services_plan_rejects_an_uninstalled_formula_before_any_metadata_fetch() {
    let sandbox = Sandbox::new();
    let prefix = sandbox.home.join("Service plan prefix");
    seed_native_graph(&prefix, &[]);

    let output = sandbox.run(&[
        "services",
        "plan",
        "zellij",
        "--prefix",
        prefix.to_str().unwrap(),
    ]);
    assert_eq!(
        output.status.code(),
        Some(1),
        "stdout:\n{}\nstderr:\n{}",
        stdout(&output),
        stderr(&output)
    );
    assert!(stderr(&output).contains("not installed"));
    assert!(!prefix.join(".brewrs/service-trials").exists());
    assert!(!sandbox.config.exists());
    sandbox.assert_no_brew();
}

#[test]
fn services_help_lists_both_read_only_actions() {
    let sandbox = Sandbox::new();
    let output = sandbox.run(&["services", "--help"]);
    assert_success(&output);
    let help = stdout(&output);
    assert!(help.contains("plan"));
    assert!(help.contains("list"));
    sandbox.assert_no_brew();
}
