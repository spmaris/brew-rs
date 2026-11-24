//! Explicit, local third-party tap lane.
//!
//! This module manages a deliberately narrow slice of Homebrew-style taps:
//!
//! * a user registers one local declarative TOML manifest with
//!   `brewrs tap add <owner>/<repo> --manifest <path>`;
//! * every package must be requested as a fully qualified
//!   `<owner>/<repo>/<token>` target, so a tap package can never silently
//!   resolve to an official formula or cask;
//! * the selected artifact must be a naked macOS executable for the host
//!   architecture, fetched over validated public HTTPS, and pinned to an exact
//!   SHA-256 digest;
//! * nothing is evaluated: no Ruby, no shell, no source build, and no formula
//!   compatibility claim;
//! * tap receipts live in their own state root, never in the standard formula
//!   or cask receipt directories, so `brewrs upgrade` never treats a tap
//!   package as an official package.
//!
//! Installed state is laid out as:
//!
//! ```text
//! <prefix>/.brewrs/state/taps/<owner>/<repo>/tap.json       registry record
//! <prefix>/.brewrs/state/taps/<owner>/<repo>/manifest.toml  exact manifest copy
//! <prefix>/.brewrs/state/taps/<owner>/<repo>/receipts/<token>.json
//! <prefix>/Taproom/<owner>/<repo>/<token>/<version>/bin/<bin>
//! <prefix>/bin/<bin>                                        relative symlink
//! ```
//!
//! The writer lock is shared with the standard store, so tap installs and
//! formula installs cannot publish colliding `bin/` entries concurrently.

mod commit;
mod fsguard;
mod package;
mod registry;
mod selection;
mod state;
mod verify;

pub(crate) use self::package::{install, plan, uninstall};
pub(crate) use self::registry::{add, list, untap};

#[cfg(test)]
mod tests;
