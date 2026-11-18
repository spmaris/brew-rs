use std::io::IsTerminal;
use std::process::Stdio;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, de::DeserializeOwned};
use tokio::process::Command;

use crate::config::Config;

/// Homebrew is the source of truth for installed packages and transactions.
/// Invoke it directly so users keep its native progress, prompts and errors.
#[derive(Clone, Copy)]
pub struct Brew {
    no_color: bool,
    verbose: bool,
}

impl Brew {
    pub fn new(config: &Config, verbose: bool) -> Self {
        Self {
            no_color: !config.ui.colored || std::env::var_os("NO_COLOR").is_some(),
            verbose,
        }
    }

    fn command(&self, subcommand: &str, args: &[&str]) -> Command {
        let mut command = Command::new("brew");
        command.arg(subcommand).args(args).kill_on_drop(true);
        if self.no_color {
            command
                .env("HOMEBREW_NO_COLOR", "1")
                .env_remove("HOMEBREW_COLOR");
        }
        if self.verbose {
            command.env("HOMEBREW_VERBOSE", "1");
        }
        command
    }

    pub async fn run(&self, subcommand: &str, args: &[&str], preview: bool) -> Result<()> {
        self.run_command(self.command(subcommand, args), subcommand, preview)
            .await
    }

    pub async fn uninstall(&self, args: &[&str], preview: bool, autoremove: bool) -> Result<()> {
        let mut command = self.command("uninstall", args);
        if autoremove {
            command.env_remove("HOMEBREW_NO_AUTOREMOVE");
        } else {
            command.env("HOMEBREW_NO_AUTOREMOVE", "1");
        }
        self.run_command(command, "uninstall", preview).await
    }

    async fn run_command(
        &self,
        mut command: Command,
        subcommand: &str,
        preview: bool,
    ) -> Result<()> {
        if preview {
            command.env("HOMEBREW_NO_AUTO_UPDATE", "1");
        }
        let status = command
            .status()
            .await
            .with_context(|| launch_hint(subcommand))?;
        if !status.success() {
            bail!("brew {subcommand} failed ({status})");
        }
        Ok(())
    }

    pub(crate) async fn json<T: DeserializeOwned>(
        &self,
        subcommand: &str,
        args: &[&str],
    ) -> Result<T> {
        let output = self
            .command(subcommand, args)
            .env("HOMEBREW_NO_AUTO_UPDATE", "1")
            .stdin(Stdio::null())
            .output()
            .await
            .with_context(|| launch_hint(subcommand))?;
        let details = String::from_utf8_lossy(&output.stderr);
        // Homebrew can emit an empty JSON result and exit zero after a
        // metadata download error. Do not report that as a clean inventory.
        let reported_error = console::strip_ansi_codes(&details)
            .lines()
            .any(|line| line.trim_start().starts_with("Error:"));
        if output.status.success() && reported_error {
            bail!("brew {subcommand} reported an error: {}", details.trim());
        }
        if !output.status.success() {
            if details.trim().is_empty() {
                bail!("brew {subcommand} failed ({})", output.status);
            }
            bail!(
                "brew {subcommand} failed ({}): {}",
                output.status,
                details.trim()
            );
        }
        if !output.stderr.is_empty() {
            eprint!("{}", String::from_utf8_lossy(&output.stderr));
        }
        serde_json::from_slice(&output.stdout)
            .with_context(|| format!("Invalid JSON from brew {subcommand}"))
    }

    pub async fn installed(&self) -> Result<Inventory> {
        self.json("info", &["--json=v2", "--installed"]).await
    }

    pub async fn outdated(&self, greedy: bool) -> Result<Outdated> {
        let mut args = vec!["--json=v2"];
        if greedy {
            args.push("--greedy");
        }
        self.json("outdated", &args).await
    }
}

fn launch_hint(subcommand: &str) -> String {
    format!(
        "Could not launch brew {subcommand}. Make sure Homebrew is installed and brew is on PATH."
    )
}

pub fn require_confirmation_input() -> Result<()> {
    if !std::io::stdin().is_terminal() {
        bail!(
            "Confirmation requires a terminal. Use --assumeyes to proceed or --dry-run to preview."
        );
    }
    Ok(())
}

#[derive(Deserialize)]
pub struct Inventory {
    pub formulae: Vec<InstalledFormula>,
    pub casks: Vec<InstalledCask>,
}

#[derive(Deserialize)]
pub struct InstalledFormula {
    pub name: String,
    pub full_name: String,
    pub tap: Option<String>,
    pub installed: Vec<Keg>,
}

#[derive(Deserialize)]
pub struct Keg {
    pub version: String,
    pub time: Option<i64>,
}

#[derive(Deserialize)]
pub struct InstalledCask {
    pub token: String,
    pub full_token: Option<String>,
    pub tap: Option<String>,
    pub installed: Option<String>,
    pub installed_time: Option<i64>,
}

#[derive(Deserialize)]
pub struct Outdated {
    pub formulae: Vec<OutdatedPackage>,
    pub casks: Vec<OutdatedPackage>,
}

#[derive(Deserialize)]
pub struct OutdatedPackage {
    pub name: String,
    pub installed_versions: Vec<String>,
    pub current_version: String,
    #[serde(default)]
    pub pinned: bool,
}
