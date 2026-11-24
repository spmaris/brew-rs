//! `.tar.gz` streaming extraction and `bsdtar` preflight/extraction.

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, Read};
use std::path::Path;
use std::process::{Command, Stdio};

use flate2::bufread::GzDecoder;
use tar::EntryType;

use super::error::CaskExtractError;
use super::limits::{MAX_ARCHIVE_BYTES, MAX_ENTRIES, MAX_UNPACKED_BYTES};
use super::tree::{
    normalize_relative_entry_path, validate_archive_entry_path, validate_relative_symlink_lexically,
};

const IO_BUFFER_BYTES: usize = 64 * 1024;

/// Bounded tar extraction for `.tar.gz` casks (no fixed `<name>/<version>` wrapper).
pub(super) fn extract_tar_gz(
    archive_path: &Path,
    unpack_dir: &Path,
) -> Result<(), CaskExtractError> {
    let file = File::open(archive_path).map_err(|source| CaskExtractError::Io {
        action: "open tarball",
        path: archive_path.to_path_buf(),
        source,
    })?;
    let decoder = GzDecoder::new(BufReader::new(file));
    let mut tar = tar::Archive::new(BoundedReader {
        inner: decoder,
        remaining: MAX_UNPACKED_BYTES,
    });

    let mut count = 0_usize;
    let mut symlinks = Vec::new();
    let entries = tar.entries().map_err(|source| CaskExtractError::Stream {
        action: "read cask tarball",
        source,
    })?;
    for entry in entries {
        let mut entry = entry.map_err(|source| CaskExtractError::Stream {
            action: "read cask tarball entry",
            source,
        })?;
        count += 1;
        if count > MAX_ENTRIES {
            return Err(CaskExtractError::TarballTooManyEntries { limit: MAX_ENTRIES });
        }

        let entry_path = entry
            .path()
            .map_err(|source| CaskExtractError::TarballEntryPath { source })?;
        let rel = normalize_relative_entry_path(&entry_path)?;
        if rel.as_os_str().is_empty() {
            continue;
        }
        let dest = unpack_dir.join(&rel);
        let entry_type = entry.header().entry_type();
        let mode = entry.header().mode().unwrap_or(0o644) & 0o777;

        match entry_type {
            EntryType::Directory => {
                fs::create_dir_all(&dest).map_err(|source| CaskExtractError::Io {
                    action: "create directory",
                    path: dest.clone(),
                    source,
                })?;
            }
            EntryType::Regular | EntryType::Continuous => {
                let size = entry.header().size().unwrap_or(0);
                if size > MAX_ARCHIVE_BYTES {
                    return Err(CaskExtractError::TarballEntryTooLarge { path: rel });
                }
                if let Some(parent) = dest.parent() {
                    fs::create_dir_all(parent)
                        .map_err(CaskExtractError::io("create directory", parent))?;
                }
                let mut out_file = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&dest)
                    .map_err(|source| CaskExtractError::Io {
                        action: "create file",
                        path: dest.clone(),
                        source,
                    })?;
                io::copy(&mut entry, &mut out_file).map_err(|source| CaskExtractError::Io {
                    action: "extract",
                    path: rel.clone(),
                    source,
                })?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let safe_mode = if mode & 0o111 != 0 { 0o755 } else { 0o644 };
                    fs::set_permissions(&dest, fs::Permissions::from_mode(safe_mode))
                        .map_err(CaskExtractError::io("set permissions on", &dest))?;
                }
            }
            EntryType::Symlink => {
                let target = entry
                    .link_name()
                    .map_err(|source| CaskExtractError::Stream {
                        action: "read tar symlink target",
                        source,
                    })?
                    .ok_or(CaskExtractError::TarSymlinkNoTarget)?;
                let target_str = target
                    .to_str()
                    .ok_or(CaskExtractError::TarSymlinkTargetNotUtf8)?
                    .to_string();
                symlinks.push((rel, dest, target_str));
            }
            EntryType::XHeader
            | EntryType::XGlobalHeader
            | EntryType::GNULongName
            | EntryType::GNULongLink => {
                continue;
            }
            entry_type => {
                return Err(CaskExtractError::TarballEntryType {
                    path: rel,
                    entry_type,
                });
            }
        }
    }

    // Drain remainder to check gzip CRC & trailer cleanly.
    let mut reader = tar.into_inner();
    let mut buf = [0_u8; IO_BUFFER_BYTES];
    while reader
        .read(&mut buf)
        .map_err(CaskExtractError::io("finish reading tarball", archive_path))?
        != 0
    {}

    #[cfg(unix)]
    for (rel, dest, target) in symlinks {
        validate_relative_symlink_lexically(&rel, &target)?;
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent).map_err(CaskExtractError::io("create directory", parent))?;
        }
        std::os::unix::fs::symlink(&target, &dest).map_err(|source| {
            CaskExtractError::SymlinkCreate {
                path: dest.clone(),
                target: target.clone(),
                source,
            }
        })?;
    }

    Ok(())
}

struct BoundedReader<R> {
    inner: R,
    remaining: u64,
}

impl<R: Read> Read for BoundedReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let cap = buf
            .len()
            .min(usize::try_from(self.remaining).unwrap_or(usize::MAX));
        if cap == 0 {
            let mut probe = [0_u8; 1];
            if self.inner.read(&mut probe)? > 0 {
                return Err(io::Error::other(format!(
                    "compressed cask exceeded {MAX_UNPACKED_BYTES} uncompressed bytes"
                )));
            }
            return Ok(0);
        }
        let n = self.inner.read(&mut buf[..cap])?;
        self.remaining -= n as u64;
        Ok(n)
    }
}

pub(super) fn preflight_bsdtar(path: &Path) -> Result<(), CaskExtractError> {
    let output = Command::new("/usr/bin/tar")
        .arg("-tf")
        .arg(path)
        .stdin(Stdio::null())
        .output()
        .map_err(|source| CaskExtractError::Stream {
            action: "list tarball contents with /usr/bin/tar",
            source,
        })?;
    if !output.status.success() {
        return Err(CaskExtractError::TarListFailed {
            status: output.status,
        });
    }
    let listing = std::str::from_utf8(&output.stdout)
        .map_err(|source| CaskExtractError::TarListingNotUtf8 { source })?;
    let mut count = 0_usize;
    for line in listing.lines() {
        count += 1;
        if count > MAX_ENTRIES {
            return Err(CaskExtractError::TarTooManyEntries { limit: MAX_ENTRIES });
        }
        validate_archive_entry_path(line, true)?;
    }
    Ok(())
}

pub(super) fn run_bsdtar_extract(path: &Path, unpack_dir: &Path) -> Result<(), CaskExtractError> {
    let status = Command::new("/usr/bin/tar")
        .arg("--no-same-owner")
        .arg("--no-same-permissions")
        .arg("-xf")
        .arg(path)
        .arg("-C")
        .arg(unpack_dir)
        .stdin(Stdio::null())
        .status()
        .map_err(|source| CaskExtractError::Stream {
            action: "extract tarball with /usr/bin/tar",
            source,
        })?;
    if !status.success() {
        return Err(CaskExtractError::TarExtractFailed { status });
    }
    Ok(())
}
