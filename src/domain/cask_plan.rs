//! Native install planning for `homebrew/cask`.
//!
//! [`resolve`] fetches official cask metadata from
//! `https://formulae.brew.sh/api/cask/<token>.json` (falling back on HTTP 404 to
//! the cached cask catalog for unique `old_tokens` renames, except during
//! read-only `migrate --check` audits) without running `brew` or evaluating Ruby.
//!
//! Only a clean, verified subset of macOS casks is eligible for native
//! installation:
//!
//! * exact 64-character lowercase SHA-256 checksums (`sha256: no_check` is blocked);
//! * public HTTPS download URLs with no `url_specs`, `rename`, or nested containers;
//! * `app`, `binary`, `command_wrapper` (`executable` + optional `args`), and static
//!   `manpage` / shell completion artifacts whose destinations stay inside the
//!   configured appdir or `$HOMEBREW_PREFIX`;
//! * recognized `depends_on` (`macos` `>=`/`==` and 64-bit `arch`) and
//!   non-destructive `uninstall` metadata (`quit`/`signal`/`login_item`/`launchctl`).
//!
//! All other casks remain plannable for diagnostics and `migrate --check`, with
//! explicit blockers explaining why native installation refuses them.

mod artifact_parse;
mod artifacts;
mod error;
mod identifier;
mod model;
mod requirements;
mod resolve;
mod version_order;

pub(crate) use error::IdentifierError;
pub(crate) use identifier::{parse_cask_target, validate_cask_identifier};
pub(crate) use model::{CaskSourcePath, ContainerHint, PlannedCask, PlannedCaskArtifact};
pub(crate) use resolve::{CaskSource, resolve_with};
pub(crate) use version_order::{CaskVersionOrder, compare_cask_versions};

#[cfg(test)]
mod tests;
