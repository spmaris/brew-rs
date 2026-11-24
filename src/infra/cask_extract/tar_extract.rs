//! `.tar.gz` streaming extraction and `bsdtar` preflight/extraction.

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, Read};
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use flate2::bufread::GzDecoder;
use tar::EntryType;

use super::limits::{MAX_ARCHIVE_BYTES, MAX_ENTRIES, MAX_UNPACKED_BYTES};
use super::tree::{
    normalize_relative_entry_path, validate_archive_entry_path, validate_relative_symlink_lexically,
};

const IO_BUFFER_BYTES: usize = 64 * 1024;

/// Bounded tar extraction for `.tar.gz` casks (no fixed `<name>/<version>` wrapper).
pub(super) fn extract_tar_gz(archive_path: &Path, unpack_dir: &Path) -> Result<()> {
    let file = File::open(archive_path)
        .with_context(|| format!("failed to open tarball '{}'", archive_path.display()))?;
    let decoder = GzDecoder::new(BufReader::new(file));
    let mut tar = tar::Archive::new(BoundedReader {
        inner: decoder,
        remaining: MAX_UNPACKED_BYTES,
    });

    let mut count = 0_usize;
    let mut symlinks = Vec::new();
    for entry in tar.entries().context("failed to read cask tarball")? {
        let mut entry = entry.context("failed to read cask tarball entry")?;
        count += 1;
        if count > MAX_ENTRIES {
            bail!("cask tarball exceeds {MAX_ENTRIES} entries");
        }

        let entry_path = entry
            .path()
            .context("cask tarball entry has a non-UTF-8 or invalid path")?;
        let rel = normalize_relative_entry_path(&entry_path)?;
        if rel.as_os_str().is_empty() {
            continue;
        }
        let dest = unpack_dir.join(&rel);
        let entry_type = entry.header().entry_type();
        let mode = entry.header().mode().unwrap_or(0o644) & 0o777;

        match entry_type {
            EntryType::Directory => {
                fs::create_dir_all(&dest)
                    .with_context(|| format!("failed to create directory '{}'", dest.display()))?;
            }
            EntryType::Regular | EntryType::Continuous => {
                let size = entry.header().size().unwrap_or(0);
                if size > MAX_ARCHIVE_BYTES {
                    bail!("cask tarball entry '{}' exceeds size limit", rel.display());
                }
                if let Some(parent) = dest.parent() {
                    fs::create_dir_all(parent)?;
                }
                let mut out_file = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&dest)
                    .with_context(|| format!("failed to create file '{}'", dest.display()))?;
                io::copy(&mut entry, &mut out_file)
                    .with_context(|| format!("failed to extract '{}'", rel.display()))?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let safe_mode = if mode & 0o111 != 0 { 0o755 } else { 0o644 };
                    fs::set_permissions(&dest, fs::Permissions::from_mode(safe_mode))?;
                }
            }
            EntryType::Symlink => {
                let target = entry
                    .link_name()
                    .context("failed to read tar symlink target")?
                    .context("tar symlink missing link target")?;
                let target_str = target
                    .to_str()
                    .context("tar symlink target is not UTF-8")?
                    .to_string();
                symlinks.push((rel, dest, target_str));
            }
            EntryType::XHeader
            | EntryType::XGlobalHeader
            | EntryType::GNULongName
            | EntryType::GNULongLink => {
                continue;
            }
            other => bail!(
                "cask tarball entry '{}' uses unsupported entry type {:?}",
                rel.display(),
                other
            ),
        }
    }

    // Drain remainder to check gzip CRC & trailer cleanly.
    let mut reader = tar.into_inner();
    let mut buf = [0_u8; IO_BUFFER_BYTES];
    while reader.read(&mut buf)? != 0 {}

    #[cfg(unix)]
    for (rel, dest, target) in symlinks {
        validate_relative_symlink_lexically(&rel, &target)?;
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }
        std::os::unix::fs::symlink(&target, &dest).with_context(|| {
            format!(
                "failed to create staged symlink '{}' -> '{target}'",
                dest.display()
            )
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

pub(super) fn preflight_bsdtar(path: &Path) -> Result<()> {
    let output = Command::new("/usr/bin/tar")
        .arg("-tf")
        .arg(path)
        .stdin(Stdio::null())
        .output()
        .context("failed to list tarball contents with /usr/bin/tar")?;
    if !output.status.success() {
        bail!("/usr/bin/tar -tf failed ({})", output.status);
    }
    let listing =
        std::str::from_utf8(&output.stdout).context("tarball contains non-UTF-8 entry paths")?;
    let mut count = 0_usize;
    for line in listing.lines() {
        count += 1;
        if count > MAX_ENTRIES {
            bail!("tarball exceeds {MAX_ENTRIES} entries");
        }
        validate_archive_entry_path(line, true)?;
    }
    Ok(())
}

pub(super) fn run_bsdtar_extract(path: &Path, unpack_dir: &Path) -> Result<()> {
    let status = Command::new("/usr/bin/tar")
        .arg("--no-same-owner")
        .arg("--no-same-permissions")
        .arg("-xf")
        .arg(path)
        .arg("-C")
        .arg(unpack_dir)
        .stdin(Stdio::null())
        .status()
        .context("failed to extract tarball with /usr/bin/tar")?;
    if !status.success() {
        bail!("/usr/bin/tar extraction failed ({status})");
    }
    Ok(())
}
