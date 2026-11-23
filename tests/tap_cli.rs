//! Offline integration tests for the explicit third-party tap lane.
//!
//! Every test runs the real `brewrs` binary with a sentinel `brew` at the front
//! of `PATH`, an isolated `HOME`, and an isolated brewrs prefix. No test uses
//! the network, Homebrew, or host package state.

#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use tempfile::TempDir;

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
exit 97
"#;

const AGENT_TUI_MANIFEST: &str = r#"schema = 1
tap = "pproenca/tap"

[[packages]]
token = "agent-tui"
version = "1.1.0"
bin = "agent-tui"

[packages.artifacts.arm64]
url = "https://github.com/pproenca/agent-tui/releases/download/v1.1.0/agent-tui-darwin-arm64"
sha256 = "0daed495a2d90acce7a720cd5ef94c7613c54b1277bffc7466b998f3edf023b5"

[packages.artifacts.x86_64]
url = "https://github.com/pproenca/agent-tui/releases/download/v1.1.0/agent-tui-darwin-x64"
sha256 = "81c071afb4df3c688059ebe6336f543cfe5e79082147d3571274c2508ea798cb"
"#;

struct Sandbox {
    _root: TempDir,
    bin: PathBuf,
    home: PathBuf,
    config: PathBuf,
    prefix: PathBuf,
    log: PathBuf,
    manifest: PathBuf,
}

impl Sandbox {
    fn new() -> Self {
        let root = tempfile::tempdir().expect("create sandbox");
        let bin = root.path().join("bin");
        let home = root.path().join("home");
        let tmp = home.join("tmp");
        for directory in [&bin, &home, &tmp] {
            fs::create_dir_all(directory).expect("create sandbox directory");
        }

        let sentinel = bin.join("brew");
        fs::write(&sentinel, SENTINEL_BREW).expect("write sentinel brew");
        let mut permissions = fs::metadata(&sentinel)
            .expect("stat sentinel")
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&sentinel, permissions).expect("chmod sentinel");

        let prefix = root.path().join("prefix");
        seed_prefix(&prefix);

        let manifest = home.join("agent-tui-tap.toml");
        fs::write(&manifest, AGENT_TUI_MANIFEST).expect("write tap manifest");
        let config = root.path().join("config").join("config.toml");
        let log = root.path().join("brew.log");

        Self {
            _root: root,
            bin,
            home,
            config,
            prefix,
            log,
            manifest,
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
        let prefix = self.prefix.to_str().expect("prefix path");
        let mut command = self.command();
        command.args(["--no-color", "--prefix", prefix]);
        command.args(args);
        command.output().expect("failed to run brewrs")
    }

    fn brew_invocations(&self) -> Vec<Vec<String>> {
        let log = fs::read_to_string(&self.log).unwrap_or_default();
        let mut invocations = Vec::new();
        for block in log.split("BEGIN\n").skip(1) {
            let Some(body) = block.split("END\n").next() else {
                continue;
            };
            invocations.push(
                body.lines()
                    .filter_map(|line| line.strip_prefix("arg\u{1f}"))
                    .map(str::to_owned)
                    .collect(),
            );
        }
        invocations
    }

    fn tap_dir(&self) -> PathBuf {
        self.prefix.join(".brewrs/state/taps/pproenca/tap")
    }
}

fn seed_prefix(prefix: &Path) {
    for directory in [
        prefix.join(".brewrs/state/receipts"),
        prefix.join(".brewrs/stages"),
        prefix.join("Cellar"),
        prefix.join("bin"),
        prefix.join("opt"),
    ] {
        fs::create_dir_all(directory).expect("create prefix directory");
    }
    fs::write(prefix.join(".brewrs-root"), "brewrs native store v1\n")
        .expect("write prefix marker");
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "command failed\nstdout:\n{}\nstderr:\n{}",
        stdout(output),
        stderr(output)
    );
}

fn assert_failure(output: &Output, needle: &str) {
    assert!(
        !output.status.success(),
        "command unexpectedly succeeded\nstdout:\n{}\nstderr:\n{}",
        stdout(output),
        stderr(output)
    );
    let message = stderr(output);
    assert!(
        message.contains(needle),
        "stderr did not contain {needle:?}:\n{message}"
    );
}

#[test]
fn tap_add_list_plan_and_untap_are_offline_and_read_only() {
    let sandbox = Sandbox::new();
    let manifest = sandbox.manifest.to_str().unwrap();

    let add = sandbox.run(&["tap", "add", "pproenca/tap", "--manifest", manifest]);
    assert_success(&add);
    assert!(sandbox.tap_dir().join("tap.json").is_file());
    assert!(sandbox.tap_dir().join("manifest.toml").is_file());
    assert!(stdout(&add).contains("No artifact was downloaded or executed"));
    assert!(sandbox.brew_invocations().is_empty());

    let list = sandbox.run(&["tap", "list"]);
    assert_success(&list);
    let listing = stdout(&list);
    assert!(listing.contains("pproenca/tap"));
    assert!(listing.contains("agent-tui"));
    assert!(listing.contains("1.1.0"));
    assert!(listing.contains("no"));
    assert!(sandbox.brew_invocations().is_empty());

    let plan = sandbox.run(&["tap", "plan", "pproenca/tap/agent-tui"]);
    assert_success(&plan);
    let planned = stdout(&plan);
    assert!(planned.contains("agent-tui-darwin"));
    assert!(
        planned.contains("0daed495a2d90acce7a720cd5ef94c7613c54b1277bffc7466b998f3edf023b5")
            || planned.contains("81c071afb4df3c688059ebe6336f543cfe5e79082147d3571274c2508ea798cb")
    );
    assert!(planned.contains("Nothing was downloaded or installed"));
    assert!(sandbox.brew_invocations().is_empty());

    let dry_run = sandbox.run(&["tap", "install", "pproenca/tap/agent-tui", "--dry-run"]);
    assert_success(&dry_run);
    assert!(stdout(&dry_run).contains("1 change planned. No packages changed."));
    assert!(
        !sandbox.home.join(".cache/brew-rs/native-casks").exists(),
        "a dry run must not create the download cache"
    );

    let untap = sandbox.run(&["untap", "pproenca/tap"]);
    assert_success(&untap);
    assert!(!sandbox.tap_dir().exists());
    assert!(!sandbox.prefix.join(".brewrs/state/taps/pproenca").exists());
    assert!(sandbox.brew_invocations().is_empty());
}

#[test]
fn tap_rejects_bare_targets_mismatched_manifests_and_duplicate_taps() {
    let sandbox = Sandbox::new();

    assert_failure(
        &sandbox.run(&["tap", "plan", "agent-tui"]),
        "fully qualified",
    );

    let mismatched = sandbox.home.join("mismatched.toml");
    fs::write(
        &mismatched,
        AGENT_TUI_MANIFEST.replace("pproenca/tap", "other/tap"),
    )
    .unwrap();
    assert_failure(
        &sandbox.run(&[
            "tap",
            "add",
            "pproenca/tap",
            "--manifest",
            mismatched.to_str().unwrap(),
        ]),
        "declares tap",
    );

    let manifest = sandbox.manifest.to_str().unwrap();
    assert_success(&sandbox.run(&["tap", "add", "pproenca/tap", "--manifest", manifest]));
    assert_failure(
        &sandbox.run(&["tap", "add", "pproenca/tap", "--manifest", manifest]),
        "already registered",
    );
    assert_failure(
        &sandbox.run(&["tap", "plan", "pproenca/tap/missing"]),
        "does not declare package",
    );
    assert_failure(
        &sandbox.run(&["tap", "uninstall", "pproenca/tap/agent-tui", "--yes"]),
        "is not installed",
    );
    assert_success(&sandbox.run(&["untap", "pproenca/tap"]));
    assert!(sandbox.brew_invocations().is_empty());
}

#[test]
fn tap_install_refuses_existing_destinations_before_downloading() {
    let sandbox = Sandbox::new();
    let manifest = sandbox.manifest.to_str().unwrap();
    assert_success(&sandbox.run(&["tap", "add", "pproenca/tap", "--manifest", manifest]));
    fs::write(sandbox.prefix.join("bin/agent-tui"), b"foreign executable").unwrap();

    let install = sandbox.run(&["tap", "install", "pproenca/tap/agent-tui", "--yes"]);
    assert_failure(&install, "already exists");
    assert_eq!(
        fs::read(sandbox.prefix.join("bin/agent-tui")).unwrap(),
        b"foreign executable"
    );
    assert!(
        !sandbox.home.join(".cache/brew-rs/native-casks").exists(),
        "a refused install must not download anything"
    );
    assert!(sandbox.brew_invocations().is_empty());
}

#[test]
fn tap_add_rejects_unsafe_manifests_and_reserved_names() {
    let sandbox = Sandbox::new();
    let unsafe_manifest = sandbox.home.join("unsafe.toml");
    fs::write(
        &unsafe_manifest,
        AGENT_TUI_MANIFEST.replace(
            "https://github.com/pproenca/agent-tui/releases/download/v1.1.0/agent-tui-darwin-arm64",
            "http://example.com/agent-tui",
        ),
    )
    .unwrap();
    assert_failure(
        &sandbox.run(&[
            "tap",
            "add",
            "pproenca/tap",
            "--manifest",
            unsafe_manifest.to_str().unwrap(),
        ]),
        "unsafe",
    );

    let reserved = sandbox.home.join("reserved.toml");
    fs::write(
        &reserved,
        AGENT_TUI_MANIFEST.replace("token = \"agent-tui\"", "token = \"brew\""),
    )
    .unwrap();
    assert_failure(
        &sandbox.run(&[
            "tap",
            "add",
            "pproenca/tap",
            "--manifest",
            reserved.to_str().unwrap(),
        ]),
        "reserved",
    );
    assert!(!sandbox.tap_dir().exists());
    assert!(sandbox.brew_invocations().is_empty());
}

#[test]
fn tap_add_initializes_an_absent_or_empty_prefix_only_after_validation() {
    let sandbox = Sandbox::new();
    fs::remove_dir_all(&sandbox.prefix).expect("remove seeded prefix");

    let unsafe_manifest = sandbox.home.join("initialization-unsafe.toml");
    fs::write(
        &unsafe_manifest,
        AGENT_TUI_MANIFEST.replace(
            "https://github.com/pproenca/agent-tui/releases/download/v1.1.0/agent-tui-darwin-arm64",
            "http://example.com/agent-tui",
        ),
    )
    .unwrap();
    assert_failure(
        &sandbox.run(&[
            "tap",
            "add",
            "pproenca/tap",
            "--manifest",
            unsafe_manifest.to_str().unwrap(),
        ]),
        "unsafe",
    );
    assert!(
        !sandbox.prefix.exists(),
        "a rejected manifest must not initialize the prefix"
    );

    let manifest = sandbox.manifest.to_str().unwrap();
    assert_success(&sandbox.run(&["tap", "add", "pproenca/tap", "--manifest", manifest]));
    assert!(sandbox.prefix.join(".brewrs-root").is_file());
    assert!(sandbox.prefix.join("Cellar").is_dir());
    assert!(sandbox.tap_dir().join("tap.json").is_file());

    fs::remove_dir_all(&sandbox.prefix).expect("remove initialized prefix");
    fs::create_dir_all(&sandbox.prefix).expect("recreate prefix");
    fs::write(sandbox.prefix.join("foreign"), b"not ours").unwrap();
    assert_failure(
        &sandbox.run(&["tap", "add", "pproenca/tap", "--manifest", manifest]),
        "not empty",
    );
    assert!(!sandbox.prefix.join(".brewrs-root").exists());
    assert_eq!(
        fs::read(sandbox.prefix.join("foreign")).unwrap(),
        b"not ours"
    );
    assert!(sandbox.brew_invocations().is_empty());
}
