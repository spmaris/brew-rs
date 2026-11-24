//! Bounded extraction of a verified Homebrew bottle archive.
//!
//! A bottle is a gzipped tar whose real layout is
//! `<formula>/<version>/...`, with the formula's Ruby definition at
//! `<formula>/<version>/.brew/<formula>.rb`. [`unpack`] verifies the caller's
//! SHA-256 against a private snapshot of the exact bytes it decompresses,
//! and extracts regular files, directories, and accepted relative symlinks
//! into the caller-owned, empty staging directory. The keg
//! contents are written at the root of `out`; the `<formula>/<version>/`
//! prefix is required and stripped.
//!
//! `.brew` metadata is skipped rather than extracted. Relative symlinks are
//! accepted only when their target (and any symlink hops along it, up to a
//! bounded depth) resolves lexically inside the keg and names a regular file or
//! directory staged in the archive. Link creation is deferred until every
//! regular file and directory is written and the gzip stream has been fully
//! verified, so extraction never traverses an archive link. Absolute, escaping,
//! cyclic, over-deep, and broken links are rejected,
//! as are hardlinks, special files, duplicate paths, unsafe path components,
//! and archives that exceed the count, depth, path, or size budgets.
//! Ownership, set-id bits, timestamps, group/world write permission, and
//! symlink modes are never applied; executable bits are preserved.
//!
//! [`unpack`] only fills the keg. Keg-internal links such as
//! `lib/libfoo.dylib -> libfoo.1.dylib` are supported, but the prefix-level
//! links Homebrew installs in `bin`, `sbin`, or `opt` (for example
//! `<prefix>/bin/hello -> ../Cellar/hello/<version>/bin/hello`) leave the keg
//! and are rejected here. A caller that wants them must create them outside
//! this verifier, only for regular executables it inspected after extraction;
//! it must not copy an archive link out of the keg. This module exposes no
//! option that relaxes containment, and no staged file is ever executed.

use std::fs;
use std::io::{BufRead, BufReader, Read};
use std::path::Path;

use flate2::bufread::GzDecoder;

mod error;
mod extract;
mod limited_reader;
mod limits;
mod paths;
mod snapshot;
mod symlinks;
mod validate;

pub(crate) use error::ArchiveError;
use extract::extract;
use limited_reader::LimitedReader;
use limits::{IO_BUFFER_BYTES, MAX_DECOMPRESSED_BYTES};
use snapshot::verified_snapshot;
use symlinks::materialize_symlinks;
use validate::{validate_digest, validate_name, validate_version};

/// Extract the keg contents of a verified bottle archive into `out`.
///
/// `out` must be an existing, empty directory owned by the caller. It is
/// filled with the contents of `<name>/<version>/`; `.brew` is skipped and
/// contained relative symlinks are created last. On error the caller should
/// discard `out`. `sha256` must be exactly 64 lowercase hexadecimal characters
/// and match the compressed archive bytes.
pub(crate) fn unpack(
    archive: &Path,
    out: &Path,
    name: &str,
    version: &str,
    sha256: &str,
) -> Result<(), ArchiveError> {
    validate_digest(sha256)?;
    validate_name(name)?;
    validate_version(version)?;
    unpack_layout(archive, out, name, Some(version), sha256)
}

/// Extract a verified standalone release rooted at one exact directory.
pub(crate) fn unpack_release(
    archive: &Path,
    out: &Path,
    root: &str,
    sha256: &str,
) -> Result<(), ArchiveError> {
    validate_digest(sha256)?;
    validate_name(root)?;
    unpack_layout(archive, out, root, None, sha256)
}

fn unpack_layout(
    archive: &Path,
    out: &Path,
    name: &str,
    version: Option<&str>,
    sha256: &str,
) -> Result<(), ArchiveError> {
    let io_error = |action: &'static str| {
        move |source: std::io::Error| ArchiveError::Io {
            action,
            path: out.to_path_buf(),
            source,
        }
    };
    let out_metadata = fs::symlink_metadata(out).map_err(io_error("inspect staging directory"))?;
    if !out_metadata.file_type().is_dir() {
        return Err(ArchiveError::StagingNotDirectory {
            path: out.to_path_buf(),
        });
    }
    let mut entries = fs::read_dir(out).map_err(io_error("read staging directory"))?;
    if entries.next().is_some() {
        return Err(ArchiveError::StagingNotEmpty {
            path: out.to_path_buf(),
        });
    }

    let snapshot = verified_snapshot(archive, out, sha256)?;
    let decoder = GzDecoder::new(BufReader::new(snapshot));
    let mut tar = tar::Archive::new(LimitedReader::new(decoder, MAX_DECOMPRESSED_BYTES));
    let symlinks = extract(&mut tar, out, name, version)?;

    // The tar parser stops at the first end marker. Consume the zero padding
    // to enforce the total expansion limit and force gzip checksum validation.
    let mut tail = tar.into_inner();
    let mut buffer = [0_u8; IO_BUFFER_BYTES];
    loop {
        let read = tail
            .read(&mut buffer)
            .map_err(|source| ArchiveError::Stream {
                action: "finish the gzip stream",
                source,
            })?;
        if read == 0 {
            break;
        }
        if buffer[..read].iter().any(|byte| *byte != 0) {
            return Err(ArchiveError::DataAfterTarEnd);
        }
    }
    if !tail
        .inner
        .into_inner()
        .fill_buf()
        .map_err(ArchiveError::Read)?
        .is_empty()
    {
        return Err(ArchiveError::BytesAfterGzip);
    }

    // Only now, with every regular entry written and the archive verified, are
    // the validated links materialized. Nothing in `out` follows a link before
    // this point.
    materialize_symlinks(out, &symlinks)
}

#[cfg(test)]
mod tests;
