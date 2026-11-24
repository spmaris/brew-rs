//! Native install planning for `homebrew/core`.
//!
//! [`resolve_with`] turns a list of requested formula names into a dependency-first
//! plan built exclusively from the official Homebrew formula API:
//! `https://formulae.brew.sh/api/formula/<name>.json`. It never runs `brew`,
//! never evaluates Ruby, never writes a file, and never extracts an archive.
//!
//! The planner deliberately supports a single, well-understood slice of
//! Homebrew:
//!
//! * only `homebrew/core` formulae with a stable version and a bottle for the
//!   requested host tag (or the universal `all` bottle) are accepted;
//! * disabled and source-only formulae are rejected;
//! * the planned version carries Homebrew's `_<revision>` suffix when the
//!   formula revision is non-zero;
//! * runtime dependencies are the host-tag `variations` override when one is
//!   present, otherwise the formula's `dependencies` list, and every one of them
//!   is resolved, planned and recorded on the dependent formula under its
//!   canonical name. Build, test, optional and recommended dependencies are
//!   intentionally not expanded (see the limitations below).
//!
//! The API payload is untrusted data. Every name, version, tag, URL, digest and
//! cellar value is validated before it reaches a [`PlannedFormula`], and the
//! graph is bounded. The digest is only recorded here; it must be verified
//! against the downloaded bytes by the download stage.
//!
//! Known limitations, all consequences of using the static API without running
//! Homebrew's Ruby:
//!
//! * Alias and old formula names fall back to the cached formula catalog
//!   when `formula/<name>.json` returns HTTP 404 (except during read-only
//!   `migrate --check` audits, which never populate or mutate a catalog
//!   cache). Requests and dependencies that resolve to the same canonical
//!   `name` are deduplicated.
//! * `uses_from_macos` is understood in three shapes on supported macOS tags:
//!   1. a plain (untagged) runtime library name from [`SYSTEM_SUPPLIED_LIBRARIES`](uses_from_macos::SYSTEM_SUPPLIED_LIBRARIES)
//!      whose bounds entry is `{}` or `{ "since": "<known_codename>" }` with
//!      host macOS release `>=` the `since` release;
//!   2. a `no_linkage` entry (`{ "<formula>": "no_linkage" }` or `["no_linkage"]`)
//!      whose formula is in [`SYSTEM_SUPPLIED_LIBRARIES`](uses_from_macos::SYSTEM_SUPPLIED_LIBRARIES) with satisfied bounds;
//!   3. a build- and/or test-only entry (`{ "<formula>": "build" }`, `"test"`,
//!      or `["build", "test"]`) with empty or recognized macOS `since`/`before`
//!      bounds, because precompiled bottles do not execute build or test phases.
//!
//!   Every other runtime name, non-build/test tag, `before` bound or unsatisfied
//!   `since` bound, mismatched bounds array, unknown or non-macOS platform, and
//!   unrecognized JSON shape keeps an install blocker.
//! * `requirements` supports `macos` entries (numeric dotted minimum evaluated
//!   against the host bottle tag or `version: null` for any supported macOS
//!   release), `arch` entries (`"arm64"` or `"x86_64"` matched against the host
//!   tag), and build- and/or test-only requirements (`contexts` containing only
//!   `"build"`/`"test"`). Other requirement names, non-build/test contexts,
//!   cask/download requests, additional fields, unusual spec lists, unparseable
//!   versions, unknown host releases and oversize arrays keep an install blocker.
//! * A `service` declaration is metadata only: installing a formula never runs
//!   `brew services`, so this crate never starts a service. Recognized
//!   optional `service` blocks on supported macOS tags do not block
//!   installation (`has_service` is recorded so `install` can note it). Invalid
//!   or unmodelled service blocks, any service on a non-macOS host, and any
//!   `pour_bottle_only_if` value stay blocked.
//! * Only direct runtime `dependencies` are planned. `recommended_dependencies`
//!   are refused with an install blocker because Homebrew would install them by
//!   default.
//! * Dependency metadata comes from the API only. Its variation lists are used
//!   verbatim, so no Ruby-side conditional logic is evaluated.
//! * Deprecated formulae are allowed when they are still bottled; disabled
//!   formulae are rejected.
//! * Metadata is fetched sequentially to keep ordering deterministic and the
//!   request volume bounded.

mod macos;
mod model;
mod node;
mod requirements;
mod resolve;
mod service;
mod source;
mod uses_from_macos;
mod validate;

pub(super) use macos::{arch_for_tag, macos_version_for_tag, parse_macos_version_or_codename};
pub(crate) use macos::{is_supported_macos_tag, macos_tag};
pub(crate) use model::FormulaResponse;
pub use model::{PackageSource, PlannedFormula, SourceFlag};
pub(crate) use resolve::resolve_with;
pub(crate) use service::{InstalledServiceMetadata, verify_installed_service};
pub(crate) use source::{FormulaSource, Memoized};
pub(crate) use validate::{parse_target, validate_formula_name, validate_tag};

#[cfg(test)]
mod tests;
