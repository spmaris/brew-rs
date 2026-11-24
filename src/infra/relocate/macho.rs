//! Bounded relocation of Homebrew placeholders in thin 64-bit Mach-O images.
//!
//! Homebrew relocatable bottles name their dependencies with
//! `@@HOMEBREW_PREFIX@@` and `@@HOMEBREW_CELLAR@@`. [`relocate`] parses the
//! staged image, asks the caller's mapper for every load path, `LC_ID_DYLIB`
//! name, and `LC_RPATH` entry, and then rewrites exactly those strings with
//! Apple's `/usr/bin/install_name_tool`. It re-parses the result and checks
//! the load-command identities, order, types, and values before ad-hoc signing
//! with `/usr/bin/codesign --force --sign -` and verifying that signature with
//! `--verify --strict`. Images that arrived signed keep their entitlements,
//! explicit requirements, code-signing flags, and hardened-runtime bit through
//! `--preserve-metadata=entitlements,requirements,flags,runtime`; images that
//! arrived unsigned are signed without that flag.
//!
//! The parser accepts little-endian thin 64-bit arm64 (including arm64e) and
//! x86_64 executables, dylibs, and bundles. FAT and 32-bit images are
//! recognized by [`is_macho`] but explicitly unsupported by [`relocate`].
//! This first lane deliberately fails closed: unknown load commands are never
//! rewritten, a raw `@@HOMEBREW_` marker outside a supported load-command
//! string is rejected, and a marker that the mapper leaves unresolved is an
//! error. After the rewrite no raw marker may remain anywhere in the file.
//!
//! Nothing in the staged image is executed. The only child processes are the
//! two absolute Apple tools named above, invoked with argument vectors (never
//! a shell). If any post-rewrite check or signature verification fails, the
//! original bytes are restored on a best-effort basis and the error is
//! returned.
//!
//! The caller owns the path mapping policy, the file walk, and the
//! transaction. This module only knows that the caller's mapper returns
//! `None` to retain a path and `Some(new)` to replace it.

use std::fs;
use std::path::Path;

use super::error::RelocateError;

mod apply;
mod constants;
mod error;
mod fat;
mod model;
mod parse;
mod rewrite_plan;

use apply::{apply, ensure_regular_file};
use parse::{parse, validate_relocatable};
use rewrite_plan::{check_marker_plan, plan, validate_targets};

pub(super) use error::MachoError;
#[cfg(test)]
pub(super) use fat::is_macho;
pub(super) use fat::{MACHO_PREFIX_BYTES, is_macho_prefix};
pub(super) use model::PathKind;

/// Rewrite the Homebrew placeholders in one staged thin 64-bit Mach-O image.
///
/// `path` must be the regular staged file whose exact bytes are `bytes`; the
/// two are compared again immediately before mutation. `map` is called once
/// for every supported load-command path, `LC_ID_DYLIB` name, and `LC_RPATH`
/// entry along with its [`PathKind`] and the image's full `LC_RPATH` list.
/// `None` retains the current value; `Some(new)` replaces it. The mapper
/// must return consistent results for repeated `(kind, path)` inputs and must
/// not introduce new paths: the rewritten command sequence and every command
/// type are checked against the plan.
///
/// On success the image is ad-hoc signed and its signature is verified. On any
/// failure after mutation begins, the original bytes are written back on a
/// best-effort basis before the error is returned.
pub(super) fn relocate(
    path: &Path,
    bytes: &[u8],
    mut map: impl FnMut(PathKind, &str, &[&str]) -> Result<Option<String>, RelocateError>,
) -> Result<(), MachoError> {
    let parsed = parse(bytes)?;
    validate_relocatable(&parsed)?;
    let plan = plan(&parsed, &mut map)?;
    validate_targets(&parsed, &plan.expected)?;
    check_marker_plan(bytes, &parsed, &plan.expected)?;

    if plan.changes.is_empty() {
        return Ok(());
    }

    ensure_regular_file(path)?;
    let current = fs::read(path).map_err(|source| MachoError::ReReadImage {
        path: path.to_path_buf(),
        source,
    })?;
    if current != bytes {
        return Err(MachoError::ImageChanged {
            path: path.to_path_buf(),
        });
    }

    if !cfg!(target_os = "macos") {
        return Err(MachoError::MacosRequired);
    }

    if let Err(error) = apply(path, &parsed, &plan) {
        return match fs::write(path, bytes) {
            Ok(()) => Err(MachoError::Restored {
                path: path.to_path_buf(),
                source: Box::new(error),
            }),
            Err(restore_error) => Err(MachoError::RestoreFailed {
                path: path.to_path_buf(),
                restore_error,
                source: Box::new(error),
            }),
        };
    }
    Ok(())
}

#[cfg(test)]
mod tests;
