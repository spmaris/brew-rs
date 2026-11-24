//! Native Homebrew cask installation, listing, upgrade, and removal.
//!
//! Operates strictly on the isolated brewrs prefix (defaulting `.app` bundles to
//! `<prefix>/Applications` unless an explicit safe `--appdir` is supplied) and
//! tracks installed casks via `.brewrs/state/casks/<token>.json`.

mod add;
mod labels;
mod list;
mod remove;
mod stage;
mod upgrade;

pub(crate) use self::add::{AuditCaskPlanner, install};
pub(crate) use self::list::list;
pub(super) use self::list::render_cask_list;
pub(crate) use self::remove::remove;
pub(crate) use self::upgrade::{check_updates, upgrade};
