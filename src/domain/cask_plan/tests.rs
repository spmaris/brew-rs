use super::resolve::plan_from_value;
use super::*;
use anyhow::Result;
use serde_json::{Value, json};
use std::collections::HashMap;

const SHA_A: &str = "f7c5093b9dd6560d08af28bc27b123fb55091deae3180ac39df199522da86815";
const SHA_B: &str = "bbed99bb8a39bb3f94389c5c99d3d02cb75f3f983b5d1bc40b1b0d1efca84713";

struct MockCaskSource(HashMap<String, Value>);

impl CaskSource for MockCaskSource {
    async fn fetch_cask(&self, token: &str) -> Result<Value> {
        self.0
            .get(token)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("missing mock cask {token}"))
    }
}

fn plan_json(value: Value, tag: &str) -> PlannedCask {
    plan_from_value(value, "mock", tag).expect("cask metadata should parse")
}

#[test]
fn parses_claude_code_binary_cask_and_applies_variations() {
    let payload = json!({
        "token": "claude-code@latest",
        "tap": "homebrew/cask",
        "name": ["Claude Code"],
        "version": "2.1.330",
        "url": "https://downloads.claude.ai/claude-code-releases/2.1.330/darwin-arm64/claude",
        "sha256": SHA_A,
        "depends_on": {},
        "conflicts_with": {"cask": ["claude-code"]},
        "artifacts": [
            {"binary": ["claude"], "target": "$HOMEBREW_PREFIX/bin/claude"},
            {"zap": [{"trash": ["~/.cache/claude"]}]}
        ],
        "supported_platforms": ["sequoia", "arm64_sequoia"],
        "variations": {
            "sequoia": {
                "url": "https://downloads.claude.ai/claude-code-releases/2.1.330/darwin-x64/claude",
                "sha256": SHA_B
            }
        }
    });

    let arm = plan_json(payload.clone(), "arm64_sequoia");
    assert!(
        arm.install_blockers.is_empty(),
        "{:?}",
        arm.install_blockers
    );
    assert_eq!(arm.sha256, SHA_A);
    assert_eq!(arm.conflicts_with_casks, vec!["claude-code"]);
    assert_eq!(
        arm.artifacts,
        vec![PlannedCaskArtifact::Binary {
            source: CaskSourcePath::Payload("claude".into()),
            link_name: "claude".into(),
        }]
    );

    let intel = plan_json(payload, "sequoia");
    assert!(
        intel.install_blockers.is_empty(),
        "{:?}",
        intel.install_blockers
    );
    assert_eq!(intel.sha256, SHA_B);
    assert!(intel.url.contains("darwin-x64"));
}

#[test]
fn parses_app_binary_command_wrapper_and_completions() {
    let iterm = plan_json(
        json!({
            "token": "iterm2",
            "version": "3.7.9",
            "url": "https://iterm2.com/downloads/stable/iTerm2-3_7_9.zip",
            "sha256": SHA_A,
            "auto_updates": true,
            "depends_on": {"macos": {">=": ["13"]}},
            "artifacts": [
                {"uninstall": [{"quit": "com.googlecode.iterm2"}]},
                {"app": ["iTerm.app"], "target": "/Applications/iTerm.app"},
                {"zap": []}
            ]
        }),
        "arm64_sequoia",
    );
    assert!(
        iterm.install_blockers.is_empty(),
        "{:?}",
        iterm.install_blockers
    );
    assert!(iterm.auto_updates);
    assert_eq!(
        iterm.artifacts,
        vec![PlannedCaskArtifact::App {
            source: "iTerm.app".into(),
            bundle_name: "iTerm.app".into(),
        }]
    );

    let vscode = plan_json(
        json!({
            "token": "visual-studio-code",
            "version": "1.147.0",
            "url": "https://update.code.visualstudio.com/1.147.0/darwin-arm64/stable",
            "sha256": SHA_A,
            "auto_updates": true,
            "depends_on": {"macos": {}},
            "artifacts": [
                {"uninstall": [{"launchctl": "com.microsoft.VSCode.ShipIt", "quit": "com.microsoft.VSCode"}]},
                {"app": ["Visual Studio Code.app"], "target": "/Applications/Visual Studio Code.app"},
                {"binary": ["$APPDIR/Visual Studio Code.app/Contents/Resources/app/bin/code"], "target": "$HOMEBREW_PREFIX/bin/code"}
            ]
        }),
        "arm64_sequoia",
    );
    assert!(
        vscode.install_blockers.is_empty(),
        "{:?}",
        vscode.install_blockers
    );
    assert_eq!(vscode.artifacts.len(), 2);

    let firefox = plan_json(
        json!({
            "token": "firefox",
            "version": "160.0.1",
            "url": "https://download-installer.cdn.mozilla.net/pub/firefox/releases/160.0.1/mac/en-US/Firefox%20160.0.1.dmg",
            "sha256": SHA_A,
            "artifacts": [
                {"uninstall": [{"quit": "org.mozilla.firefox"}]},
                {"app": ["Firefox.app"], "target": "/Applications/Firefox.app"},
                {"command_wrapper": ["firefox", {"executable": "$APPDIR/Firefox.app/Contents/MacOS/firefox", "args": ["--no-remote"]}], "target": "$HOMEBREW_PREFIX/bin/firefox"}
            ]
        }),
        "arm64_sequoia",
    );
    assert!(
        firefox.install_blockers.is_empty(),
        "{:?}",
        firefox.install_blockers
    );
    assert_eq!(
        firefox.artifacts[1],
        PlannedCaskArtifact::CommandWrapper {
            executable: CaskSourcePath::AppBundle {
                bundle_name: "Firefox.app".into(),
                inner_path: "Contents/MacOS/firefox".into(),
            },
            link_name: "firefox".into(),
            args: vec!["--no-remote".into()],
        }
    );

    let ghostty = plan_json(
        json!({
            "token": "ghostty",
            "version": "1.4.0",
            "url": "https://release.files.ghostty.org/1.4.0/Ghostty.dmg",
            "sha256": SHA_A,
            "depends_on": {"macos": {">=": ["13"]}},
            "artifacts": [
                {"app": ["Ghostty.app"], "target": "/Applications/Ghostty.app"},
                {"manpage": ["$APPDIR/Ghostty.app/Contents/Resources/man/man1/ghostty.1"], "target": "$HOMEBREW_PREFIX/share/man/man1/ghostty.1"},
                {"bash_completion": ["$APPDIR/Ghostty.app/Contents/Resources/bash-completion/completions/ghostty.bash"], "target": "$HOMEBREW_PREFIX/etc/bash_completion.d/ghostty"},
                {"zsh_completion": ["$APPDIR/Ghostty.app/Contents/Resources/zsh/site-functions/_ghostty"], "target": "$HOMEBREW_PREFIX/share/zsh/site-functions/_ghostty"},
                {"fish_completion": ["$APPDIR/Ghostty.app/Contents/Resources/fish/vendor_completions.d/ghostty.fish"], "target": "$HOMEBREW_PREFIX/share/fish/vendor_completions.d/ghostty.fish"}
            ]
        }),
        "arm64_sequoia",
    );
    assert!(
        ghostty.install_blockers.is_empty(),
        "{:?}",
        ghostty.install_blockers
    );
    assert_eq!(ghostty.artifacts.len(), 5);
}

#[test]
fn blocks_no_check_pkg_font_home_target_and_generated_completions() {
    let no_check = plan_json(
        json!({
            "token": "google-chrome",
            "version": "158.0",
            "url": "https://dl.google.com/chrome/mac/universal/stable/GGRO/googlechrome.dmg",
            "sha256": "no_check",
            "artifacts": [{"app": ["Google Chrome.app"], "target": "/Applications/Google Chrome.app"}]
        }),
        "arm64_sequoia",
    );
    assert!(
        no_check
            .install_blockers
            .iter()
            .any(|b| b.contains("no_check")),
        "{:?}",
        no_check.install_blockers
    );

    let pkg = plan_json(
        json!({
            "token": "pkg-app",
            "version": "1.0",
            "url": "https://example.com/app.pkg",
            "sha256": SHA_A,
            "artifacts": [{"pkg": ["App.pkg"]}]
        }),
        "arm64_sequoia",
    );
    assert!(pkg.install_blockers.iter().any(|b| b.contains("'pkg'")));

    let font = plan_json(
        json!({
            "token": "font-fira-code",
            "version": "6.2",
            "url": "https://github.com/tonsky/FiraCode/releases/download/6.2/Fira_Code_v6.2.zip",
            "sha256": SHA_A,
            "artifacts": [{"font": ["ttf/FiraCode-Regular.ttf"], "target": "/$HOME/Library/Fonts/FiraCode-Regular.ttf"}]
        }),
        "arm64_sequoia",
    );
    assert!(font.install_blockers.iter().any(|b| b.contains("'font'")));

    let alacritty = plan_json(
        json!({
            "token": "alacritty",
            "version": "0.17.0",
            "url": "https://github.com/alacritty/alacritty/releases/download/v0.17.0/Alacritty-v0.17.0.dmg",
            "sha256": SHA_A,
            "artifacts": [
                {"app": ["Alacritty.app"], "target": "/Applications/Alacritty.app"},
                {"binary": ["$APPDIR/Alacritty.app/Contents/Resources/61/alacritty", {"target": "~/.terminfo/61/alacritty"}], "target": "/$HOME/.terminfo/61/alacritty"}
            ]
        }),
        "arm64_sequoia",
    );
    assert!(
        alacritty
            .install_blockers
            .iter()
            .any(|b| b.contains("outside $HOMEBREW_PREFIX/bin"))
    );

    let op_cli = plan_json(
        json!({
            "token": "1password-cli",
            "version": "2.42.0",
            "url": "https://cache.agilebits.com/dist/1P/op2/pkg/v2.42.0/op_darwin_arm64_v2.42.0.zip",
            "sha256": SHA_A,
            "artifacts": [
                {"binary": ["op"], "target": "$HOMEBREW_PREFIX/bin/op"},
                {"generate_completions_from_executable": ["op", "completion"]}
            ]
        }),
        "arm64_sequoia",
    );
    assert!(
        op_cli
            .install_blockers
            .iter()
            .any(|b| b.contains("generate_completions_from_executable"))
    );
}

#[test]
fn enforces_macos_and_arch_depends_on_and_deduplicates_targets() {
    let arch_arm = plan_json(
        json!({
            "token": "arm-only",
            "version": "1.0",
            "url": "https://example.com/arm.zip",
            "sha256": SHA_A,
            "depends_on": {"arch": [{"type": "arm", "bits": 64}], "macos": {"==": ["14", "15"]}},
            "artifacts": [{"binary": ["arm-only"], "target": "$HOMEBREW_PREFIX/bin/arm-only"}]
        }),
        "arm64_sequoia",
    );
    assert!(
        arch_arm.install_blockers.is_empty(),
        "{:?}",
        arch_arm.install_blockers
    );

    let arch_fail = plan_json(
        json!({
            "token": "arm-only",
            "version": "1.0",
            "url": "https://example.com/arm.zip",
            "sha256": SHA_A,
            "depends_on": {"arch": [{"type": "intel", "bits": 64}], "macos": {">=": ["26"]}},
            "artifacts": [{"binary": ["arm-only"], "target": "$HOMEBREW_PREFIX/bin/arm-only"}]
        }),
        "arm64_sequoia",
    );
    assert_eq!(arch_fail.install_blockers.len(), 2);

    let mut map = HashMap::new();
    map.insert(
        "demo".into(),
        json!({
            "token": "demo",
            "version": "1.0",
            "url": "https://example.com/demo.zip",
            "sha256": SHA_A,
            "artifacts": [{"binary": ["demo"], "target": "$HOMEBREW_PREFIX/bin/demo"}]
        }),
    );
    let source = MockCaskSource(map);
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let resolved = rt
        .block_on(resolve_with(
            &source,
            &["demo".into(), "homebrew/cask/demo".into()],
            "arm64_sequoia",
        ))
        .unwrap();
    assert_eq!(resolved.len(), 1);
}

#[test]
fn compares_cask_versions_with_optional_build_numbers() {
    assert_eq!(
        compare_cask_versions("2.1.330", "2.1.330"),
        CaskVersionOrder::Same
    );
    assert_eq!(
        compare_cask_versions("2.1.331", "2.1.330"),
        CaskVersionOrder::Upgrade
    );
    assert_eq!(
        compare_cask_versions("2.1.329", "2.1.330"),
        CaskVersionOrder::Downgrade
    );
    assert_eq!(
        compare_cask_versions("1.5,200", "1.5,199"),
        CaskVersionOrder::Upgrade
    );
    assert_eq!(
        compare_cask_versions("1.5,198", "1.5,199"),
        CaskVersionOrder::Downgrade
    );
    assert_eq!(
        compare_cask_versions("1.5,199", "1.5"),
        CaskVersionOrder::Upgrade
    );
    assert_eq!(
        compare_cask_versions("1.5", "1.5,199"),
        CaskVersionOrder::Downgrade
    );
    assert_eq!(
        compare_cask_versions("1.2.3-beta2", "1.2.3-beta1"),
        CaskVersionOrder::Upgrade
    );
}
