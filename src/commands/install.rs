use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

mod add;
mod all;
mod format;
mod list;
mod pending;
mod remove;
mod shellenv;
mod sources;
mod stage;
mod upgrade;

pub(crate) use self::add::install;
pub(crate) use self::all::{check_all_updates, upgrade_all};
pub(in crate::commands) use self::format::{installed_summary, removed_summary, upgraded_summary};
pub(crate) use self::list::{list, list_all};
pub(crate) use self::remove::{autoremove, remove};
pub(crate) use self::shellenv::shellenv;
pub(in crate::commands) use self::stage::fetch_formula;
pub(crate) use self::upgrade::upgrade;
pub(crate) use crate::domain::plan::SourceFlag;

pub(super) fn prefix_path(prefix: Option<&Path>) -> Result<PathBuf> {
    if let Some(path) = prefix {
        return Ok(path.to_path_buf());
    }
    dirs::data_local_dir()
        .map(|path| path.join("brew-rs").join("native"))
        .context("Cannot determine the native prefix; pass --prefix with an absolute path")
}

/// The prompt is written to stderr and read from stdin, so a piped stdout
/// does not prevent it.
fn can_prompt(stdin_tty: bool, stderr_tty: bool) -> bool {
    stdin_tty && stderr_tty
}

pub(super) fn confirm(question: &str) -> Result<bool> {
    if !can_prompt(io::stdin().is_terminal(), io::stderr().is_terminal()) {
        bail!("Confirmation requires a visible terminal; pass --assumeyes or --dry-run");
    }
    // The plan table goes to stdout; flush it so it precedes the stderr prompt.
    io::stdout().flush()?;
    eprint!("{question} [y/N] ");
    io::stderr().flush()?;
    let mut answer = String::new();
    io::stdin().read_line(&mut answer)?;
    Ok(matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod prompt_tests {
    use super::can_prompt;

    #[test]
    fn prompt_needs_only_stdin_and_stderr_terminals() {
        assert!(can_prompt(true, true));
        assert!(!can_prompt(false, true));
        assert!(!can_prompt(true, false));
        assert!(!can_prompt(false, false));
    }
}
