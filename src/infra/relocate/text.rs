//! Bounded relocation of Homebrew placeholders in supported UTF-8 text files.
//!
//! Homebrew relocatable bottles can carry `@@HOMEBREW_CELLAR@@` and
//! `@@HOMEBREW_PREFIX@@` placeholders in text as well as in Mach-O load
//! commands. [`rewrite`] supports:
//!
//! * `bin/pcre2-config`: preserves the exact audited handler for upstream
//!   `pcre2-config` (single-quoted variables and inert `*SunOS*`/`*BSD*` `libR`
//!   branches).
//! * `lib/pkgconfig/*.pc` and `share/pkgconfig/*.pc`: pkg-config assignments,
//!   path flags (`Libs*`, `Cflags*` including `-L`/`-I` space quoting), and
//!   comments carrying pinned Homebrew placeholders.
//! * Any other valid UTF-8 text file (without NUL or unsupported control
//!   bytes, such as CMake targets, `*-config` scripts, and `.inc` files):
//!   bounded line-by-line placeholder rewriting backed by [`Paths`].
//!
//! Nothing is rewritten by blanket substring replacement: every `@@HOMEBREW_`
//! occurrence must resolve to the managed prefix, the package itself, or a
//! pinned direct dependency receipt, with no `..` traversal and no residual
//! markers. A file with no `@@HOMEBREW_` marker returns `Ok(None)`.

use std::io::{self, Read};
use std::path::Path;

mod error;
mod general;
mod mapping;
mod pcre2_config;
mod pkg_config;

pub(crate) use error::TextError;
use general::rewrite_general_text;
use pcre2_config::rewrite_pcre2_config;
use pkg_config::{pc_name_byte, rewrite_pkg_config};

/// Every Homebrew text placeholder starts with this prefix.
const MARKER: &str = "@@HOMEBREW_";
/// Bare managed-prefix placeholder.
const PREFIX_MARKER: &str = "@@HOMEBREW_PREFIX@@";
/// Marker for a complete `Cellar/<name>/<version>` path and its members.
const CELLAR_MARKER: &str = "@@HOMEBREW_CELLAR@@/";
/// Marker for an `opt/<name>` path and its members.
const OPT_MARKER: &str = "@@HOMEBREW_PREFIX@@/opt/";
/// Fixed Cellar path used by upstream `pcre2-config` in non-macOS branches.
const FIXED_CELLAR: &str = "/opt/homebrew/Cellar/";
/// Every byte pattern whose presence makes [`rewrite`] act on a file. Both the
/// in-memory rewrite and [`stream_has_needle`] use this list so they agree.
/// The other path constants only matter in files that already carry [`MARKER`].
const NEEDLES: &[&[u8]] = &[MARKER.as_bytes()];
/// Largest text file this module will inspect.
const MAX_TEXT_BYTES: usize = 4 * 1024 * 1024;

/// The three supported text relocation lanes.
enum Kind {
    PkgConfig,
    Pcre2Config,
    GeneralText,
}

impl Kind {
    fn classify(relative: &Path) -> Result<Self, TextError> {
        let Some(text) = relative.to_str() else {
            return Err(TextError::NonUtf8Path);
        };
        if text == "bin/pcre2-config" {
            return Ok(Self::Pcre2Config);
        }
        if let Some(name) = text
            .strip_prefix("lib/pkgconfig/")
            .or_else(|| text.strip_prefix("share/pkgconfig/"))
        {
            if !name.is_empty()
                && name.len() > ".pc".len()
                && name.ends_with(".pc")
                && !name.contains('/')
                && !name.contains("..")
                && name.bytes().all(pc_name_byte)
            {
                return Ok(Self::PkgConfig);
            }
            return Err(TextError::UnsupportedPkgConfigName {
                name: name.to_owned(),
            });
        }
        Ok(Self::GeneralText)
    }

    fn require_trailing_newline(&self) -> bool {
        matches!(self, Self::PkgConfig | Self::Pcre2Config)
    }
}

/// Rewrite one staged text file, or return `None` when it carries no marker.
///
/// `relative` is the path below the keg root (for example
/// `lib/pkgconfig/libpcre2-8.pc`). The returned bytes are a complete
/// replacement file; the caller owns staging, verification, and writing.
pub(super) fn rewrite(
    relative: &std::path::Path,
    bytes: &[u8],
    paths: &super::paths::Paths<'_>,
) -> Result<Option<Vec<u8>>, TextError> {
    if !has_needle(bytes) {
        return Ok(None);
    }
    if bytes.len() > MAX_TEXT_BYTES {
        return Err(TextError::TooLarge {
            path: relative.to_path_buf(),
            limit: MAX_TEXT_BYTES,
        });
    }
    let kind = Kind::classify(relative)?;
    let text = decode(bytes, relative, kind.require_trailing_newline())?;
    let rewritten = match kind {
        Kind::PkgConfig => rewrite_pkg_config(text, paths),
        Kind::Pcre2Config => rewrite_pcre2_config(text, paths),
        Kind::GeneralText => rewrite_general_text(text, paths),
    }
    .map_err(|source| TextError::Rewrite {
        path: relative.to_path_buf(),
        source: Box::new(source),
    })?;
    if rewritten.contains(MARKER) {
        return Err(TextError::LeftMarker {
            path: relative.to_path_buf(),
        });
    }
    Ok(Some(rewritten.into_bytes()))
}

/// Decode a supported text file with a bounded UTF-8 byte grammar.
fn decode<'a>(
    bytes: &'a [u8],
    relative: &Path,
    require_trailing_newline: bool,
) -> Result<&'a str, TextError> {
    let text = std::str::from_utf8(bytes).map_err(|source| TextError::NotUtf8 {
        path: relative.to_path_buf(),
        source,
    })?;
    if require_trailing_newline && !text.ends_with('\n') {
        return Err(TextError::MissingNewline {
            path: relative.to_path_buf(),
        });
    }
    if text.bytes().any(|byte| {
        byte == b'\r' || (byte < 0x20 && byte != b'\n' && byte != b'\t') || byte == 0x7f
    }) {
        return Err(TextError::ControlBytes {
            path: relative.to_path_buf(),
        });
    }
    Ok(text)
}

/// Whether `bytes` contains any pattern in [`NEEDLES`].
fn has_needle(bytes: &[u8]) -> bool {
    NEEDLES.iter().any(|needle| contains(bytes, needle))
}

/// Whether a stream contains any pattern in [`NEEDLES`], read `chunk` bytes at
/// a time. Consecutive windows overlap by one byte less than the longest
/// needle, so a pattern straddling a chunk boundary is still found.
pub(super) fn stream_has_needle(mut reader: impl Read, chunk: usize) -> io::Result<bool> {
    let keep = NEEDLES.iter().map(|n| n.len()).max().unwrap_or(1) - 1;
    let mut window = Vec::with_capacity(chunk + keep);
    loop {
        let read = reader
            .by_ref()
            .take(chunk as u64)
            .read_to_end(&mut window)?;
        if read == 0 {
            return Ok(false);
        }
        if has_needle(&window) {
            return Ok(true);
        }
        let excess = window.len().saturating_sub(keep);
        window.drain(..excess);
    }
}

/// Whether a byte slice contains `needle`.
fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty()
        && haystack
            .windows(needle.len())
            .any(|window| window == needle)
}

#[cfg(test)]
mod tests;
