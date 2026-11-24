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

pub(super) fn confirm(question: &str) -> Result<bool> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() || !io::stderr().is_terminal() {
        bail!("Confirmation requires a visible terminal; pass --assumeyes or --dry-run");
    }
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
