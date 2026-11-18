//! Offline integration tests for the `brewrs` command line.
//!
//! Every test runs the real binary with a fake `brew` executable at the front
//! of `PATH`. The fake records its argv and a small allowlist of Homebrew
//! environment variables, and emits canned JSON for `brew info` and
//! `brew outdated`. No Homebrew installation, network access, or real package
//! state is involved.
//!
//! Configuration is redirected to a throwaway path through `BREW_RS_CONFIG`,
//! and `HOME`/XDG directories are isolated as well, so the tests never touch
//! the user's real brew-rs configuration.

#![cfg(unix)]

use std::collections::HashMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

use tempfile::TempDir;

/// Fake Homebrew. It records one block per invocation and answers `info` and
/// `outdated` from fixture files, so assertions can inspect both argv and the
/// environment brew-rs passes down.
const FAKE_BREW: &str = r#"#!/bin/sh
set -u

log=${BREW_FAKE_LOG:?BREW_FAKE_LOG must be set}
fixtures=${BREW_FAKE_DIR:?BREW_FAKE_DIR must be set}

{
    printf 'BEGIN\n'
    for arg in "$@"; do
        printf 'arg\037%s\n' "$arg"
    done
    printf 'env\037HOMEBREW_NO_AUTOREMOVE=%s\n' "${HOMEBREW_NO_AUTOREMOVE-<unset>}"
    printf 'env\037HOMEBREW_NO_AUTO_UPDATE=%s\n' "${HOMEBREW_NO_AUTO_UPDATE-<unset>}"
    printf 'env\037HOMEBREW_NO_COLOR=%s\n' "${HOMEBREW_NO_COLOR-<unset>}"
    printf 'env\037HOMEBREW_COLOR=%s\n' "${HOMEBREW_COLOR-<unset>}"
    printf 'env\037HOMEBREW_VERBOSE=%s\n' "${HOMEBREW_VERBOSE-<unset>}"
    printf 'END\n'
} >> "$log"

case "${1-}" in
    outdated)
        if [ -f "$fixtures/outdated.json" ]; then
            cat "$fixtures/outdated.json"
        else
            printf '{"formulae":[],"casks":[]}'
        fi
        exit "${BREW_FAKE_OUTDATED_EXIT:-0}"
        ;;
    info)
        if [ -f "$fixtures/installed.json" ]; then
            cat "$fixtures/installed.json"
        else
            printf '{"formulae":[],"casks":[]}'
        fi
        exit "${BREW_FAKE_INFO_EXIT:-0}"
        ;;
    *)
        exit "${BREW_FAKE_EXIT:-0}"
        ;;
esac
"#;

const UPDATED_JSON: &str = r#"{
  "formulae": [
    {
      "name": "ripgrep",
      "installed_versions": ["13.0.0"],
      "current_version": "14.1.1",
      "pinned": false
    }
  ],
  "casks": [
    {
      "name": "firefox",
      "installed_versions": ["119.0"],
      "current_version": "120.0"
    }
  ]
}"#;

const NO_UPDATES_JSON: &str = r#"{"formulae":[],"casks":[]}"#;

/// Makes `handle_error` print the anyhow context chain to stderr.
const VERBOSE_ERRORS: &[(&str, &str)] = &[("BREW_RS_VERBOSE", "1")];

const INSTALLED_JSON: &str = r#"{
  "formulae": [
    {
      "name": "ripgrep",
      "full_name": "ripgrep",
      "tap": null,
      "installed": [
        {"version": "14.1.0", "time": 1700000000}
      ]
    }
  ],
  "casks": [
    {
      "token": "firefox",
      "full_token": "firefox",
      "tap": null,
      "installed": "120.0",
      "installed_time": 1600000000
    }
  ]
}"#;

#[derive(Debug, Default)]
struct Invocation {
    args: Vec<String>,
    /// Every tracked variable maps to its value, or `None` when the fake
    /// observed it as unset.
    env: HashMap<String, Option<String>>,
}

impl Invocation {
    fn env(&self, key: &str) -> Option<&str> {
        self.env.get(key).and_then(|value| value.as_deref())
    }
}

struct Sandbox {
    _root: TempDir,
    bin: PathBuf,
    fixtures: PathBuf,
    log: PathBuf,
    config: PathBuf,
    home: PathBuf,
}

impl Sandbox {
    fn new() -> Self {
        let root = tempfile::tempdir().expect("create sandbox directory");
        let bin = root.path().join("bin");
        let fixtures = root.path().join("fixtures");
        let home = root.path().join("home");
        let tmp = home.join("tmp");
        for directory in [&bin, &fixtures, &home, &tmp] {
            fs::create_dir_all(directory).expect("create sandbox directory");
        }

        let fake_brew = bin.join("brew");
        fs::write(&fake_brew, FAKE_BREW).expect("write fake brew");
        let mut permissions = fs::metadata(&fake_brew)
            .expect("stat fake brew")
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&fake_brew, permissions).expect("chmod fake brew");

        let log = root.path().join("brew.log");
        let config = root.path().join("config").join("config.toml");

        Self {
            _root: root,
            bin,
            fixtures,
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
            .env("BREW_FAKE_LOG", &self.log)
            .env("BREW_FAKE_DIR", &self.fixtures)
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

    fn write_fixture(&self, name: &str, contents: &str) {
        fs::write(self.fixtures.join(name), contents).expect("write fixture");
    }

    fn invocations(&self) -> Vec<Invocation> {
        let log = fs::read_to_string(&self.log).unwrap_or_default();
        let mut invocations = Vec::new();
        let mut current: Option<Invocation> = None;

        for line in log.lines() {
            match line {
                "BEGIN" => current = Some(Invocation::default()),
                "END" => {
                    if let Some(invocation) = current.take() {
                        invocations.push(invocation);
                    }
                }
                _ => {
                    let Some(invocation) = current.as_mut() else {
                        continue;
                    };
                    if let Some(arg) = line.strip_prefix("arg\u{1f}") {
                        invocation.args.push(arg.to_string());
                    } else if let Some(environment) = line.strip_prefix("env\u{1f}") {
                        if let Some((key, value)) = environment.split_once('=') {
                            let value = (value != "<unset>").then(|| value.to_string());
                            invocation.env.insert(key.to_string(), value);
                        }
                    }
                }
            }
        }

        invocations
    }

    fn single_invocation(&self) -> Invocation {
        let mut invocations = self.invocations();
        assert_eq!(
            invocations.len(),
            1,
            "expected exactly one brew invocation, observed {invocations:#?}"
        );
        invocations.remove(0)
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

fn assert_outdated_row(output: &str, name: &str, installed: &str, current: &str) {
    assert_row_containing(output, name, &[installed, current]);
}

#[test]
fn install_preview_uses_brew_install_with_dry_run() {
    let sandbox = Sandbox::new();
    let output = sandbox.run(&["--no-color", "install", "--dry-run", "ripgrep"]);
    assert_success(&output);

    let invocation = sandbox.single_invocation();
    assert_eq!(
        invocation.args,
        vec!["install", "--dry-run", "--", "ripgrep"]
    );
    assert_eq!(invocation.env("HOMEBREW_NO_AUTO_UPDATE"), Some("1"));
    assert_eq!(invocation.env("HOMEBREW_NO_COLOR"), Some("1"));
    assert_eq!(invocation.env("HOMEBREW_VERBOSE"), None);
    assert_eq!(invocation.env("HOMEBREW_NO_AUTOREMOVE"), None);

    let output = stdout(&output);
    assert!(output.contains("Transaction summary"), "stdout: {output}");
    assert!(output.contains("Preview"), "stdout: {output}");
}

#[test]
fn upgrade_preview_uses_brew_upgrade_with_dry_run() {
    let sandbox = Sandbox::new();
    let output = sandbox.run(&["--no-color", "-v", "upgrade", "-n", "ripgrep"]);
    assert_success(&output);

    let invocation = sandbox.single_invocation();
    assert_eq!(
        invocation.args,
        vec!["upgrade", "--dry-run", "--", "ripgrep"]
    );
    assert_eq!(invocation.env("HOMEBREW_NO_AUTO_UPDATE"), Some("1"));
    assert_eq!(invocation.env("HOMEBREW_NO_COLOR"), Some("1"));
    assert_eq!(invocation.env("HOMEBREW_VERBOSE"), Some("1"));

    let output = stdout(&output);
    assert!(output.contains("Upgrade"), "stdout: {output}");
    assert!(output.contains("Preview"), "stdout: {output}");
}

#[test]
fn remove_preview_sets_homebrew_no_autoremove_by_default() {
    let sandbox = Sandbox::new();
    let output = sandbox.run(&["--no-color", "remove", "--dry-run", "ripgrep"]);
    assert_success(&output);

    let invocation = sandbox.single_invocation();
    assert_eq!(
        invocation.args,
        vec!["uninstall", "--dry-run", "--", "ripgrep"]
    );
    assert_eq!(invocation.env("HOMEBREW_NO_AUTOREMOVE"), Some("1"));
    assert_eq!(invocation.env("HOMEBREW_NO_AUTO_UPDATE"), Some("1"));

    assert!(stdout(&output).contains("Preview"));
}

#[test]
fn remove_preview_autoremove_opt_in_clears_homebrew_no_autoremove() {
    let sandbox = Sandbox::new();
    let output = sandbox.run(&[
        "--no-color",
        "remove",
        "--dry-run",
        "--autoremove",
        "ripgrep",
    ]);
    assert_success(&output);

    let invocation = sandbox.single_invocation();
    assert_eq!(
        invocation.args,
        vec!["uninstall", "--dry-run", "--", "ripgrep"]
    );
    assert_eq!(invocation.env("HOMEBREW_NO_AUTOREMOVE"), None);
    assert_eq!(invocation.env("HOMEBREW_NO_AUTO_UPDATE"), Some("1"));
}

#[test]
fn remove_with_assumeyes_sets_homebrew_no_autoremove_and_does_not_preview() {
    let sandbox = Sandbox::new();
    let output = sandbox.run(&["--no-color", "remove", "--assumeyes", "ripgrep"]);
    assert_success(&output);

    let invocation = sandbox.single_invocation();
    assert_eq!(invocation.args, vec!["uninstall", "--", "ripgrep"]);
    assert_eq!(invocation.env("HOMEBREW_NO_AUTOREMOVE"), Some("1"));
    assert_eq!(invocation.env("HOMEBREW_NO_AUTO_UPDATE"), None);
}

#[test]
fn non_tty_refuses_install_and_remove_without_confirmation() {
    let sandbox = Sandbox::new();

    for args in [
        vec!["--no-color", "install", "ripgrep"],
        vec!["--no-color", "remove", "ripgrep"],
    ] {
        let output = sandbox.run_with_env(&args, VERBOSE_ERRORS);
        assert_eq!(
            output.status.code(),
            Some(1),
            "expected refusal for {args:?}\nstdout:\n{}\nstderr:\n{}",
            stdout(&output),
            stderr(&output)
        );
        assert!(
            stderr(&output).contains("Confirmation requires a terminal"),
            "stderr for {args:?} did not explain the refusal:\n{}",
            stderr(&output)
        );
        assert!(
            !stdout(&output).contains("Transaction summary"),
            "a refused transaction must not print a summary for {args:?}"
        );
    }

    assert!(
        sandbox.invocations().is_empty(),
        "brew must not be invoked when confirmation is refused"
    );
}

#[test]
fn check_update_reports_structured_outdated_rows_and_exits_100() {
    let sandbox = Sandbox::new();
    sandbox.write_fixture("outdated.json", UPDATED_JSON);

    let output = sandbox.run(&["--no-color", "check-update"]);
    assert_eq!(
        output.status.code(),
        Some(100),
        "check-update must signal updates with exit code 100\nstdout:\n{}\nstderr:\n{}",
        stdout(&output),
        stderr(&output)
    );

    let invocation = sandbox.single_invocation();
    assert_eq!(invocation.args, vec!["outdated", "--json=v2"]);
    assert_eq!(invocation.env("HOMEBREW_NO_AUTO_UPDATE"), Some("1"));

    let output = stdout(&output);
    assert_outdated_row(&output, "ripgrep", "13.0.0", "14.1.1");
    assert_outdated_row(&output, "firefox", "119.0", "120.0");
}

#[test]
fn list_outdated_reports_updates_but_exits_zero() {
    let sandbox = Sandbox::new();
    sandbox.write_fixture("outdated.json", UPDATED_JSON);

    let output = sandbox.run(&["--no-color", "list", "--outdated"]);
    assert_success(&output);

    let invocation = sandbox.single_invocation();
    assert_eq!(invocation.args, vec!["outdated", "--json=v2"]);

    let output = stdout(&output);
    assert_outdated_row(&output, "ripgrep", "13.0.0", "14.1.1");
    assert_outdated_row(&output, "firefox", "119.0", "120.0");
}

#[test]
fn check_update_and_list_outdated_exit_zero_without_updates() {
    let sandbox = Sandbox::new();
    sandbox.write_fixture("outdated.json", NO_UPDATES_JSON);

    for args in [
        vec!["--no-color", "check-update"],
        vec!["--no-color", "list", "--outdated"],
    ] {
        let output = sandbox.run(&args);
        assert_success(&output);
        assert!(
            !stdout(&output).contains("ripgrep"),
            "no outdated package should be reported for {args:?}:\n{}",
            stdout(&output)
        );
    }

    let invocations = sandbox.invocations();
    assert_eq!(invocations.len(), 2);
    for invocation in invocations {
        assert_eq!(invocation.args, vec!["outdated", "--json=v2"]);
    }
}

#[test]
fn check_update_greedy_passes_greedy_to_brew() {
    let sandbox = Sandbox::new();
    sandbox.write_fixture("outdated.json", UPDATED_JSON);

    let output = sandbox.run(&["--no-color", "check-update", "--greedy"]);
    assert_eq!(
        output.status.code(),
        Some(100),
        "stdout:\n{}\nstderr:\n{}",
        stdout(&output),
        stderr(&output)
    );

    let invocation = sandbox.single_invocation();
    assert_eq!(invocation.args, vec!["outdated", "--json=v2", "--greedy"]);
}

#[test]
fn malformed_outdated_json_is_a_failure() {
    let sandbox = Sandbox::new();
    sandbox.write_fixture("outdated.json", "{ this is not valid json");

    let output = sandbox.run_with_env(&["--no-color", "check-update"], VERBOSE_ERRORS);
    assert_eq!(
        output.status.code(),
        Some(1),
        "malformed brew output must fail\nstdout:\n{}\nstderr:\n{}",
        stdout(&output),
        stderr(&output)
    );
    assert!(
        stderr(&output).contains("Invalid JSON"),
        "stderr did not identify the malformed JSON:\n{}",
        stderr(&output)
    );
    assert_eq!(sandbox.invocations().len(), 1);
}

#[test]
fn unexpected_outdated_shape_is_a_failure() {
    let sandbox = Sandbox::new();
    sandbox.write_fixture("outdated.json", r#"{"formulae":"ripgrep","casks":[]}"#);

    let output = sandbox.run_with_env(&["--no-color", "check-update"], VERBOSE_ERRORS);
    assert_eq!(
        output.status.code(),
        Some(1),
        "unexpected brew output must fail\nstdout:\n{}\nstderr:\n{}",
        stdout(&output),
        stderr(&output)
    );
    assert!(
        stderr(&output).contains("Invalid JSON"),
        "stderr did not identify the unexpected JSON shape:\n{}",
        stderr(&output)
    );
}

#[test]
fn list_dates_uses_installed_inventory_and_renders_formula_and_cask_dates() {
    let sandbox = Sandbox::new();
    sandbox.write_fixture("installed.json", INSTALLED_JSON);

    let output = sandbox.run(&["--no-color", "list", "--dates"]);
    assert_success(&output);

    let invocation = sandbox.single_invocation();
    assert_eq!(invocation.args, vec!["info", "--json=v2", "--installed"]);
    assert_eq!(invocation.env("HOMEBREW_NO_AUTO_UPDATE"), Some("1"));
    assert_eq!(invocation.env("HOMEBREW_NO_COLOR"), Some("1"));

    let output = stdout(&output);
    // Formula version and `installed[].time` (2023-11-14 UTC) on one row.
    assert_row_containing(&output, "ripgrep", &["14.1.0", "2023-11-14"]);
    // Cask version and `installed_time` (2020-09-13 UTC) on one row.
    assert_row_containing(&output, "firefox", &["120.0", "2020-09-13"]);
}

#[test]
fn check_update_fails_when_brew_outdated_fails() {
    let sandbox = Sandbox::new();
    sandbox.write_fixture("outdated.json", UPDATED_JSON);

    let output = sandbox.run_with_env(
        &["--no-color", "check-update"],
        &[("BREW_RS_VERBOSE", "1"), ("BREW_FAKE_OUTDATED_EXIT", "1")],
    );
    assert_eq!(
        output.status.code(),
        Some(1),
        "a failing brew outdated must fail the command\nstdout:\n{}\nstderr:\n{}",
        stdout(&output),
        stderr(&output)
    );
    assert!(
        stderr(&output).contains("brew outdated failed"),
        "stderr did not report the brew failure:\n{}",
        stderr(&output)
    );
    assert_eq!(sandbox.invocations().len(), 1);
}

#[test]
fn failed_apply_is_not_reported_as_complete() {
    let sandbox = Sandbox::new();
    let output = sandbox.run_with_env(&["install", "--yes", "ripgrep"], &[("BREW_FAKE_EXIT", "3")]);
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("brew install failed"));
    assert!(!stdout(&output).contains("Complete!"));
    assert_eq!(
        sandbox.single_invocation().args,
        ["install", "--yes", "--", "ripgrep"]
    );
}

#[test]
fn update_only_refreshes_metadata() {
    let sandbox = Sandbox::new();
    assert_success(&sandbox.run(&["update"]));
    assert_eq!(sandbox.single_invocation().args, ["update"]);
}

#[test]
fn autoremove_preview_does_not_apply() {
    let sandbox = Sandbox::new();
    assert_success(&sandbox.run(&["autoremove", "-n"]));
    let invocation = sandbox.single_invocation();
    assert_eq!(invocation.args, ["autoremove", "--dry-run"]);
    assert_eq!(invocation.env("HOMEBREW_NO_AUTO_UPDATE"), Some("1"));
}

#[test]
fn help_names_brewrs_without_creating_config() {
    let sandbox = Sandbox::new();
    let output = sandbox.run(&["--help"]);
    assert_success(&output);
    assert!(stdout(&output).contains("Usage: brewrs"));
    assert!(!sandbox.config.exists());
    assert!(sandbox.invocations().is_empty());
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
    assert!(sandbox.invocations().is_empty());
    assert_success(&sandbox.run(&["config", "reset"]));
    assert_success(&sandbox.run(&["config", "show"]));
}

#[test]
fn offline_catalog_search_does_not_launch_brew() {
    let sandbox = Sandbox::new();
    assert_success(&sandbox.run(&["config", "reset"]));
    let mut config: toml::Value = fs::read_to_string(&sandbox.config)
        .unwrap()
        .parse()
        .unwrap();
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
    assert!(sandbox.invocations().is_empty());
}
