//! Safe format detection, preflighted extraction, and payload staging for casks.
//!
//! Supported formats:
//! * Naked binaries (Mach-O executables or `#!` scripts),
//! * ZIP archives (preflighted in pure Rust across Central Directory headers
//!   before invoking `/usr/bin/ditto -x -k` so Apple signatures and resource
//!   forks remain intact),
//! * `.tar.gz` archives (streamed through `flate2` + `tar` with strict path and
//!   deferred relative-symlink containment checks),
//! * `.tar.bz2` / `.tar.xz` archives (entry list preflighted before `/usr/bin/tar -xf`
//!   plus post-unpack containment walk),
//! * `.dmg` disk images (mounted read-only via `/usr/bin/hdiutil attach -mountrandom`
//!   with RAII `detach`, copying only declared artifact sources via `/usr/bin/ditto`).
//!
//! Every extracted tree is walked without following symlinks to reject special
//! files and escaping links before artifacts are staged for [`super::cask_store`].

use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, Read, Seek, SeekFrom};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use flate2::bufread::GzDecoder;
use sha2::{Digest, Sha256};
use tar::EntryType;

use super::cask_plan::{CaskSourcePath, ContainerHint, PlannedCask, PlannedCaskArtifact};
use super::download::verify_file_digest;

/// Largest single extracted file or naked payload (4 GiB).
const MAX_ARCHIVE_BYTES: u64 = 4 * 1024 * 1024 * 1024;
/// Largest total uncompressed bytes allowed for an unpacked cask (8 GiB).
const MAX_UNPACKED_BYTES: u64 = 8 * 1024 * 1024 * 1024;
/// Maximum archive/tree entries allowed for one cask.
const MAX_ENTRIES: usize = 65_536;
/// Maximum directory depth allowed during extraction and tree validation.
const MAX_DEPTH: usize = 64;
/// Maximum symlink hops resolved inside a staged `.app` bundle.
const MAX_SYMLINK_HOPS: usize = 8;
/// Maximum EOCD search tail in bytes (22-byte header + 64 KiB max comment).
const MAX_EOCD_TAIL_BYTES: usize = 22 + u16::MAX as usize;
const IO_BUFFER_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PayloadFormat {
    Naked,
    Zip,
    TarGz,
    TarBz2,
    TarXz,
    Dmg,
}

/// Output of staging a single verified cask download into a private stage dir.
#[derive(Debug)]
pub(super) struct StagedCaskPayload {
    pub app_bundles: Vec<StagedAppBundle>,
    pub has_caskroom_files: bool,
    pub caskroom_files: Vec<String>,
    pub symlinks: Vec<StagedCaskLink>,
    pub wrappers: Vec<StagedWrapper>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct StagedAppBundle {
    pub bundle_name: String,
    pub staged_dir: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct StagedCaskLink {
    pub relative_dest: String,
    pub target: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct StagedWrapper {
    pub relative_dest: String,
    pub staged_file: PathBuf,
    pub sha256: String,
}

/// Stage a verified cask download inside `stage_dir` and prepare all declared
/// app bundles, Caskroom files, symlinks, and command wrappers.
pub(super) fn stage(
    download_path: &Path,
    stage_dir: &Path,
    planned: &PlannedCask,
    prefix: &Path,
    appdir: &Path,
) -> Result<StagedCaskPayload> {
    verify_file_digest(download_path, &planned.sha256).with_context(|| {
        format!(
            "SHA-256 verification failed before staging cask {} {}",
            planned.token, planned.version
        )
    })?;

    let stage_meta = fs::symlink_metadata(stage_dir)
        .with_context(|| format!("failed to inspect stage '{}'", stage_dir.display()))?;
    if stage_meta.file_type().is_symlink() || !stage_meta.is_dir() {
        bail!(
            "cask stage '{}' is not a real directory",
            stage_dir.display()
        );
    }

    let format = detect_format(download_path, &planned.url, planned.container)?;
    let unpack_dir = stage_dir.join("unpack");
    fs::create_dir(&unpack_dir).with_context(|| {
        format!(
            "failed to create unpack directory '{}'",
            unpack_dir.display()
        )
    })?;

    match format {
        PayloadFormat::Naked => stage_naked_payload(download_path, &unpack_dir, planned)?,
        PayloadFormat::Zip => {
            preflight_zip(download_path)?;
            run_ditto_extract(download_path, &unpack_dir)?;
            validate_extracted_tree(&unpack_dir)?;
        }
        PayloadFormat::TarGz => {
            extract_tar_gz(download_path, &unpack_dir)?;
            validate_extracted_tree(&unpack_dir)?;
        }
        PayloadFormat::TarBz2 | PayloadFormat::TarXz => {
            preflight_bsdtar(download_path)?;
            run_bsdtar_extract(download_path, &unpack_dir)?;
            validate_extracted_tree(&unpack_dir)?;
        }
        PayloadFormat::Dmg => {
            extract_dmg(download_path, stage_dir, &unpack_dir, planned)?;
            validate_extracted_tree(&unpack_dir)?;
        }
    }

    assemble_stage(stage_dir, &unpack_dir, planned, prefix, appdir)
}

/// Detect the payload container format from magic bytes, explicit hints, or URL.
pub(super) fn detect_format(path: &Path, url: &str, hint: ContainerHint) -> Result<PayloadFormat> {
    match hint {
        ContainerHint::Naked => return Ok(PayloadFormat::Naked),
        ContainerHint::Zip => return Ok(PayloadFormat::Zip),
        ContainerHint::Dmg => return Ok(PayloadFormat::Dmg),
        ContainerHint::Auto => {}
    }

    let mut file = File::open(path)
        .with_context(|| format!("failed to open downloaded cask '{}'", path.display()))?;
    let meta = file
        .metadata()
        .with_context(|| format!("failed to inspect downloaded cask '{}'", path.display()))?;
    if !meta.is_file() || meta.len() == 0 {
        bail!(
            "downloaded cask '{}' is empty or not a regular file",
            path.display()
        );
    }

    let mut head = [0_u8; 16];
    let head_len = read_prefix(&mut file, &mut head)?;
    let head = &head[..head_len];

    if head.starts_with(b"PK\x03\x04") || head.starts_with(b"PK\x05\x06") {
        return Ok(PayloadFormat::Zip);
    }
    if head.starts_with(&[0x1f, 0x8b]) {
        return Ok(PayloadFormat::TarGz);
    }
    if head.starts_with(b"BZh") {
        return Ok(PayloadFormat::TarBz2);
    }
    if head.starts_with(&[0xfd, 0x37, 0x7a, 0x58, 0x5a, 0x00]) {
        return Ok(PayloadFormat::TarXz);
    }

    // UDIF DMG images end with a 512-byte `koly` trailer.
    if meta.len() >= 512 {
        file.seek(SeekFrom::End(-512))
            .context("failed to seek to DMG trailer")?;
        let mut trailer = [0_u8; 4];
        file.read_exact(&mut trailer)
            .context("failed to read DMG trailer")?;
        if &trailer == b"koly" {
            return Ok(PayloadFormat::Dmg);
        }
    }

    if is_macho_or_script_magic(head) {
        return Ok(PayloadFormat::Naked);
    }

    let url_path = url.split(['?', '#']).next().unwrap_or(url);
    let lower_path = url_path.to_ascii_lowercase();
    if lower_path.ends_with(".dmg") {
        return Ok(PayloadFormat::Dmg);
    }
    if lower_path.ends_with(".zip") {
        return Ok(PayloadFormat::Zip);
    }
    if lower_path.ends_with(".tar.gz") || lower_path.ends_with(".tgz") {
        return Ok(PayloadFormat::TarGz);
    }

    bail!("unrecognized cask payload format for '{url_path}'")
}

fn read_prefix(file: &mut File, buf: &mut [u8]) -> Result<usize> {
    let mut read_total = 0;
    while read_total < buf.len() {
        let n = file
            .read(&mut buf[read_total..])
            .context("failed to read cask header")?;
        if n == 0 {
            break;
        }
        read_total += n;
    }
    Ok(read_total)
}

fn is_macho_or_script_magic(head: &[u8]) -> bool {
    if head.starts_with(b"#!") {
        return true;
    }
    if head.len() < 4 {
        return false;
    }
    matches!(
        &head[..4],
        [0xCF, 0xFA, 0xED, 0xFE]
            | [0xFE, 0xED, 0xFA, 0xCF]
            | [0xCE, 0xFA, 0xED, 0xFE]
            | [0xFE, 0xED, 0xFA, 0xCE]
            | [0xCA, 0xFE, 0xBA, 0xBE]
            | [0xBE, 0xBA, 0xFE, 0xCA]
    )
}

fn stage_naked_payload(
    download_path: &Path,
    unpack_dir: &Path,
    planned: &PlannedCask,
) -> Result<()> {
    let mut payload_paths = HashSet::new();
    for artifact in &planned.artifacts {
        match artifact {
            PlannedCaskArtifact::App { bundle_name, .. } => {
                bail!(
                    "cask declares app bundle '{bundle_name}' but downloaded payload is a naked file"
                );
            }
            PlannedCaskArtifact::Binary {
                source: CaskSourcePath::Payload(rel),
                ..
            }
            | PlannedCaskArtifact::CommandWrapper {
                executable: CaskSourcePath::Payload(rel),
                ..
            }
            | PlannedCaskArtifact::PrefixLink {
                source: CaskSourcePath::Payload(rel),
                ..
            } => {
                payload_paths.insert(rel.as_str());
            }
            PlannedCaskArtifact::Binary {
                source: CaskSourcePath::AppBundle { .. },
                ..
            }
            | PlannedCaskArtifact::CommandWrapper {
                executable: CaskSourcePath::AppBundle { .. },
                ..
            }
            | PlannedCaskArtifact::PrefixLink {
                source: CaskSourcePath::AppBundle { .. },
                ..
            } => {
                bail!("cask references $APPDIR bundle in a naked file payload");
            }
        }
    }
    if payload_paths.len() != 1 {
        bail!(
            "naked cask payload must map to exactly one payload file (got {})",
            payload_paths.len()
        );
    }
    let rel = payload_paths.into_iter().next().expect("len == 1");
    let dest = unpack_dir.join(rel);
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::copy(download_path, &dest)
        .with_context(|| format!("failed to stage naked cask binary to '{}'", dest.display()))?;
    set_executable(&dest)?;
    Ok(())
}

/// Preflight a ZIP archive by scanning its Central Directory in pure Rust before
/// delegating extraction to `/usr/bin/ditto`.
fn preflight_zip(path: &Path) -> Result<()> {
    let mut file =
        File::open(path).with_context(|| format!("failed to open ZIP '{}'", path.display()))?;
    let file_len = file
        .metadata()
        .with_context(|| format!("failed to inspect ZIP '{}'", path.display()))?
        .len();
    if !(22..=MAX_ARCHIVE_BYTES).contains(&file_len) {
        bail!(
            "ZIP archive '{}' has invalid size {file_len}",
            path.display()
        );
    }

    let tail_len = (file_len as usize).min(MAX_EOCD_TAIL_BYTES);
    let tail_start = file_len - tail_len as u64;
    file.seek(SeekFrom::Start(tail_start))
        .context("failed to seek to ZIP EOCD")?;
    let mut tail = vec![0_u8; tail_len];
    file.read_exact(&mut tail)
        .context("failed to read ZIP EOCD")?;

    let eocd_rel = find_eocd(&tail).context("ZIP archive is missing a valid EOCD record")?;
    let eocd = &tail[eocd_rel..];
    let eocd_offset = tail_start + eocd_rel as u64;

    let disk_num = read_u16_le(eocd, 4)?;
    let cd_disk = read_u16_le(eocd, 6)?;
    let entries_32 = read_u16_le(eocd, 10)?;
    let cd_size_32 = read_u32_le(eocd, 12)?;
    let cd_offset_32 = read_u32_le(eocd, 16)?;

    let (entries_count, cd_size, cd_offset) =
        if entries_32 == u16::MAX || cd_size_32 == u32::MAX || cd_offset_32 == u32::MAX {
            parse_zip64_eocd(&mut file, eocd_offset)?
        } else {
            if disk_num != 0 || cd_disk != 0 {
                bail!("multi-disk ZIP archives are not supported");
            }
            (
                u64::from(entries_32),
                u64::from(cd_size_32),
                u64::from(cd_offset_32),
            )
        };

    if entries_count > MAX_ENTRIES as u64 {
        bail!("ZIP archive contains {entries_count} entries, above the {MAX_ENTRIES} limit");
    }
    let cd_end = cd_offset
        .checked_add(cd_size)
        .context("ZIP central directory size overflow")?;
    if cd_end > eocd_offset || cd_end > file_len {
        bail!("ZIP central directory extends beyond archive bounds");
    }

    file.seek(SeekFrom::Start(cd_offset))
        .context("failed to seek to ZIP central directory")?;
    let mut cd = vec![0_u8; usize::try_from(cd_size).context("ZIP central directory too large")?];
    file.read_exact(&mut cd)
        .context("failed to read ZIP central directory")?;

    let mut pos = 0_usize;
    let mut total_uncompressed = 0_u64;
    for _ in 0..entries_count {
        if pos + 46 > cd.len() {
            bail!("truncated ZIP central directory entry");
        }
        let header = &cd[pos..pos + 46];
        if &header[0..4] != b"PK\x01\x02" {
            bail!("invalid ZIP central directory entry signature");
        }
        let comp_size_32 = read_u32_le(header, 20)?;
        let uncomp_size_32 = read_u32_le(header, 24)?;
        let fname_len = usize::from(read_u16_le(header, 28)?);
        let extra_len = usize::from(read_u16_le(header, 30)?);
        let comment_len = usize::from(read_u16_le(header, 32)?);

        let entry_end = pos + 46 + fname_len + extra_len + comment_len;
        if entry_end > cd.len() {
            bail!("truncated ZIP entry metadata");
        }
        let fname_bytes = &cd[pos + 46..pos + 46 + fname_len];
        let extra = &cd[pos + 46 + fname_len..pos + 46 + fname_len + extra_len];

        let fname = std::str::from_utf8(fname_bytes)
            .context("ZIP archive contains a non-UTF-8 entry name")?;
        validate_archive_entry_path(fname, true)?;

        let uncomp_size = if uncomp_size_32 == u32::MAX {
            parse_zip64_uncompressed_size(extra, comp_size_32 == u32::MAX)
                .context("ZIP64 entry is missing uncompressed size in extra field")?
        } else {
            u64::from(uncomp_size_32)
        };
        if uncomp_size > MAX_ARCHIVE_BYTES {
            bail!("ZIP entry '{fname}' exceeds the {MAX_ARCHIVE_BYTES} byte limit");
        }
        total_uncompressed = total_uncompressed
            .checked_add(uncomp_size)
            .context("ZIP total uncompressed size overflowed")?;
        if total_uncompressed > MAX_UNPACKED_BYTES {
            bail!("ZIP archive uncompressed size exceeds {MAX_UNPACKED_BYTES} bytes");
        }
        pos = entry_end;
    }

    Ok(())
}

fn find_eocd(tail: &[u8]) -> Option<usize> {
    if tail.len() < 22 {
        return None;
    }
    let mut i = tail.len() - 22;
    loop {
        if &tail[i..i + 4] == b"PK\x05\x06" {
            let comment_len = usize::from(u16::from_le_bytes([tail[i + 20], tail[i + 21]]));
            if i + 22 + comment_len == tail.len() {
                return Some(i);
            }
        }
        if i == 0 {
            break;
        }
        i -= 1;
    }
    None
}

fn parse_zip64_eocd(file: &mut File, eocd_offset: u64) -> Result<(u64, u64, u64)> {
    if eocd_offset < 20 {
        bail!("invalid ZIP64 EOCD locator offset");
    }
    file.seek(SeekFrom::Start(eocd_offset - 20))
        .context("failed to seek to ZIP64 EOCD locator")?;
    let mut locator = [0_u8; 20];
    file.read_exact(&mut locator)
        .context("failed to read ZIP64 EOCD locator")?;
    if &locator[0..4] != b"PK\x06\x07" {
        bail!("missing ZIP64 EOCD locator");
    }
    let z64_offset = u64::from_le_bytes(locator[8..16].try_into().unwrap());
    if z64_offset + 56 > eocd_offset {
        bail!("ZIP64 EOCD offset is out of bounds");
    }
    file.seek(SeekFrom::Start(z64_offset))
        .context("failed to seek to ZIP64 EOCD")?;
    let mut z64 = [0_u8; 56];
    file.read_exact(&mut z64)
        .context("failed to read ZIP64 EOCD")?;
    if &z64[0..4] != b"PK\x06\x06" {
        bail!("invalid ZIP64 EOCD signature");
    }
    let entries = u64::from_le_bytes(z64[32..40].try_into().unwrap());
    let cd_size = u64::from_le_bytes(z64[40..48].try_into().unwrap());
    let cd_offset = u64::from_le_bytes(z64[48..56].try_into().unwrap());
    Ok((entries, cd_size, cd_offset))
}

fn parse_zip64_uncompressed_size(extra: &[u8], _needs_comp: bool) -> Option<u64> {
    let mut pos = 0;
    while pos + 4 <= extra.len() {
        let id = u16::from_le_bytes([extra[pos], extra[pos + 1]]);
        let len = usize::from(u16::from_le_bytes([extra[pos + 2], extra[pos + 3]]));
        pos += 4;
        if pos + len > extra.len() {
            return None;
        }
        if id == 0x0001 && len >= 8 {
            return Some(u64::from_le_bytes(extra[pos..pos + 8].try_into().ok()?));
        }
        pos += len;
    }
    None
}

fn read_u16_le(slice: &[u8], offset: usize) -> Result<u16> {
    let bytes: [u8; 2] = slice
        .get(offset..offset + 2)
        .context("unexpected EOF reading u16")?
        .try_into()
        .unwrap();
    Ok(u16::from_le_bytes(bytes))
}

fn read_u32_le(slice: &[u8], offset: usize) -> Result<u32> {
    let bytes: [u8; 4] = slice
        .get(offset..offset + 4)
        .context("unexpected EOF reading u32")?
        .try_into()
        .unwrap();
    Ok(u32::from_le_bytes(bytes))
}

fn run_ditto_extract(zip_path: &Path, unpack_dir: &Path) -> Result<()> {
    let status = Command::new("/usr/bin/ditto")
        .arg("-x")
        .arg("-k")
        .arg(zip_path)
        .arg(unpack_dir)
        .stdin(Stdio::null())
        .status()
        .context("failed to execute /usr/bin/ditto")?;
    if !status.success() {
        bail!(
            "/usr/bin/ditto failed to extract '{}' ({status})",
            zip_path.display()
        );
    }
    Ok(())
}

/// Bounded tar extraction for `.tar.gz` casks (no fixed `<name>/<version>` wrapper).
fn extract_tar_gz(archive_path: &Path, unpack_dir: &Path) -> Result<()> {
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

fn preflight_bsdtar(path: &Path) -> Result<()> {
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

fn run_bsdtar_extract(path: &Path, unpack_dir: &Path) -> Result<()> {
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

/// Mount a DMG read-only with `/usr/bin/hdiutil`, copy only declared artifacts,
/// and guarantee detach on exit via RAII.
fn extract_dmg(
    dmg_path: &Path,
    stage_dir: &Path,
    unpack_dir: &Path,
    planned: &PlannedCask,
) -> Result<()> {
    let sources = collect_dmg_sources(planned)?;
    let mount_parent = tempfile::Builder::new()
        .prefix("dmg-mount-")
        .tempdir_in(stage_dir)
        .context("failed to create DMG mount parent directory")?;
    let mounted = MountedDmg::attach(dmg_path, mount_parent.path())?;

    for rel_source in sources {
        copy_from_dmg(mounted.mount_point(), unpack_dir, &rel_source)?;
    }

    mounted.detach()?;
    Ok(())
}

fn collect_dmg_sources(planned: &PlannedCask) -> Result<Vec<String>> {
    let mut sources = Vec::new();
    let mut seen = HashSet::new();
    for artifact in &planned.artifacts {
        match artifact {
            PlannedCaskArtifact::App { source, .. } => {
                if seen.insert(source.clone()) {
                    sources.push(source.clone());
                }
            }
            PlannedCaskArtifact::Binary {
                source: CaskSourcePath::Payload(rel),
                ..
            }
            | PlannedCaskArtifact::CommandWrapper {
                executable: CaskSourcePath::Payload(rel),
                ..
            }
            | PlannedCaskArtifact::PrefixLink {
                source: CaskSourcePath::Payload(rel),
                ..
            } => {
                if seen.insert(rel.clone()) {
                    sources.push(rel.clone());
                }
            }
            PlannedCaskArtifact::Binary {
                source: CaskSourcePath::AppBundle { .. },
                ..
            }
            | PlannedCaskArtifact::CommandWrapper {
                executable: CaskSourcePath::AppBundle { .. },
                ..
            }
            | PlannedCaskArtifact::PrefixLink {
                source: CaskSourcePath::AppBundle { .. },
                ..
            } => {}
        }
    }
    if sources.is_empty() {
        bail!(
            "cask '{}' has no extractable DMG source artifacts",
            planned.token
        );
    }
    Ok(sources)
}

fn copy_from_dmg(mount_point: &Path, unpack_dir: &Path, rel_source: &str) -> Result<()> {
    validate_archive_entry_path(rel_source, false)?;
    // Verify no path component inside the DMG up to and including `rel_source`
    // is a symlink (e.g. a DMG `/Applications` drop symlink).
    let mut current = mount_point.to_path_buf();
    for segment in rel_source.split('/') {
        current.push(segment);
        let meta = fs::symlink_metadata(&current).with_context(|| {
            format!(
                "DMG artifact '{}' is missing at '{}'",
                rel_source,
                current.display()
            )
        })?;
        if meta.file_type().is_symlink() {
            bail!(
                "refusing DMG artifact '{}': path component '{}' is a symlink",
                rel_source,
                current.display()
            );
        }
    }

    let src_path = mount_point.join(rel_source);
    let dst_path = unpack_dir.join(rel_source);
    if let Some(parent) = dst_path.parent() {
        fs::create_dir_all(parent)?;
    }

    let status = Command::new("/usr/bin/ditto")
        .arg(&src_path)
        .arg(&dst_path)
        .stdin(Stdio::null())
        .status()
        .context("failed to execute /usr/bin/ditto for DMG copy")?;
    if !status.success() {
        bail!(
            "/usr/bin/ditto failed to copy DMG artifact '{}' ({status})",
            rel_source
        );
    }
    Ok(())
}

struct MountedDmg {
    mount_point: PathBuf,
    detached: bool,
}

impl MountedDmg {
    fn attach(dmg_path: &Path, mount_parent: &Path) -> Result<Self> {
        let output = Command::new("/usr/bin/hdiutil")
            .args([
                "attach",
                "-nobrowse",
                "-readonly",
                "-noautoopen",
                "-mountrandom",
            ])
            .arg(mount_parent)
            .arg("-plist")
            .arg(dmg_path)
            .stdin(Stdio::null())
            .output()
            .context("failed to execute /usr/bin/hdiutil attach")?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!(
                "/usr/bin/hdiutil attach failed for '{}' ({}): {}",
                dmg_path.display(),
                output.status,
                stderr.trim()
            );
        }
        let plist = std::str::from_utf8(&output.stdout)
            .context("hdiutil attach -plist returned non-UTF-8 output")?;
        let mount_point = parse_hdiutil_mount_point(plist)?;

        let canonical_parent =
            fs::canonicalize(mount_parent).context("failed to canonicalize DMG mount parent")?;
        let canonical_mount = fs::canonicalize(&mount_point).with_context(|| {
            format!(
                "failed to canonicalize DMG mount '{}'",
                mount_point.display()
            )
        })?;
        let meta = fs::symlink_metadata(&canonical_mount).with_context(|| {
            format!(
                "failed to inspect DMG mount '{}'",
                canonical_mount.display()
            )
        })?;
        if !meta.is_dir()
            || !canonical_mount.starts_with(&canonical_parent)
            || canonical_mount == canonical_parent
        {
            let _ = detach_mount(&mount_point);
            bail!(
                "refusing DMG mount point '{}' outside '{}'",
                mount_point.display(),
                canonical_parent.display()
            );
        }

        Ok(Self {
            mount_point: canonical_mount,
            detached: false,
        })
    }

    fn mount_point(&self) -> &Path {
        &self.mount_point
    }

    fn detach(mut self) -> Result<()> {
        self.detached = true;
        detach_mount(&self.mount_point)
    }
}

impl Drop for MountedDmg {
    fn drop(&mut self) {
        if !self.detached {
            let _ = detach_mount(&self.mount_point);
        }
    }
}

fn detach_mount(mount_point: &Path) -> Result<()> {
    let status = Command::new("/usr/bin/hdiutil")
        .args(["detach", "-quiet"])
        .arg(mount_point)
        .stdin(Stdio::null())
        .status();
    match status {
        Ok(s) if s.success() => Ok(()),
        _ => {
            let force_status = Command::new("/usr/bin/hdiutil")
                .args(["detach", "-force", "-quiet"])
                .arg(mount_point)
                .stdin(Stdio::null())
                .status()
                .context("failed to run /usr/bin/hdiutil detach -force")?;
            if force_status.success() {
                Ok(())
            } else {
                bail!("failed to detach DMG at '{}'", mount_point.display())
            }
        }
    }
}

/// Extract the `<key>mount-point</key><string>...</string>` path from
/// `hdiutil attach -plist` XML output.
pub(super) fn parse_hdiutil_mount_point(plist: &str) -> Result<PathBuf> {
    let key = "<key>mount-point</key>";
    let mut search_from = 0;
    while let Some(idx) = plist[search_from..].find(key) {
        let after_key = &plist[search_from + idx + key.len()..];
        let trimmed = after_key.trim_start();
        if let Some(rest) = trimmed.strip_prefix("<string>") {
            if let Some(end) = rest.find("</string>") {
                let decoded = decode_xml_entities(&rest[..end])?;
                let path = PathBuf::from(&decoded);
                if path.is_absolute() && !decoded.contains('\0') {
                    return Ok(path);
                }
                bail!("invalid mount-point path in hdiutil plist: '{decoded}'");
            }
        }
        search_from += idx + key.len();
    }
    bail!("hdiutil attach output did not include a <key>mount-point</key>")
}

fn decode_xml_entities(input: &str) -> Result<String> {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let after = &rest[amp + 1..];
        let Some(semi) = after.find(';') else {
            bail!("unterminated XML entity in hdiutil plist");
        };
        let entity = &after[..semi];
        match entity {
            "amp" => out.push('&'),
            "lt" => out.push('<'),
            "gt" => out.push('>'),
            "apos" => out.push('\''),
            "quot" => out.push('"'),
            hex if hex.starts_with("#x") || hex.starts_with("#X") => {
                let code = u32::from_str_radix(&hex[2..], 16)
                    .context("invalid hex XML entity in hdiutil plist")?;
                let ch = char::from_u32(code).context("invalid Unicode scalar in XML entity")?;
                out.push(ch);
            }
            dec if dec.starts_with('#') => {
                let code: u32 = dec[1..]
                    .parse()
                    .context("invalid decimal XML entity in hdiutil plist")?;
                let ch = char::from_u32(code).context("invalid Unicode scalar in XML entity")?;
                out.push(ch);
            }
            other => bail!("unsupported XML entity '&{other};' in hdiutil plist"),
        }
        rest = &after[semi + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

/// Walk `root` without following symlinks, enforcing entry/depth/size bounds and
/// confirming every symlink is relative and lexically stays inside `root`.
fn validate_extracted_tree(root: &Path) -> Result<()> {
    let mut entries = 0_usize;
    let mut total_bytes = 0_u64;
    let mut stack = vec![(root.to_path_buf(), PathBuf::new(), 0_usize)];

    while let Some((abs, rel, depth)) = stack.pop() {
        if depth > MAX_DEPTH {
            bail!("extracted cask exceeds maximum directory depth ({MAX_DEPTH})");
        }
        for child in fs::read_dir(&abs)
            .with_context(|| format!("failed to read extracted directory '{}'", abs.display()))?
        {
            let child = child?;
            entries += 1;
            if entries > MAX_ENTRIES {
                bail!("extracted cask exceeds {MAX_ENTRIES} entries");
            }
            let child_name = child.file_name();
            let child_abs = child.path();
            let child_rel = rel.join(&child_name);
            let meta = fs::symlink_metadata(&child_abs)
                .with_context(|| format!("failed to inspect '{}'", child_abs.display()))?;
            let ftype = meta.file_type();

            if ftype.is_symlink() {
                let target = fs::read_link(&child_abs)
                    .with_context(|| format!("failed to read symlink '{}'", child_abs.display()))?;
                let target_str = target.to_str().with_context(|| {
                    format!("non-UTF-8 symlink target at '{}'", child_rel.display())
                })?;
                validate_relative_symlink_lexically(&child_rel, target_str)?;
            } else if ftype.is_dir() {
                stack.push((child_abs, child_rel, depth + 1));
            } else if ftype.is_file() {
                if meta.len() > MAX_ARCHIVE_BYTES {
                    bail!(
                        "extracted file '{}' exceeds size limit",
                        child_rel.display()
                    );
                }
                total_bytes = total_bytes
                    .checked_add(meta.len())
                    .context("extracted cask byte size overflowed")?;
                if total_bytes > MAX_UNPACKED_BYTES {
                    bail!("extracted cask exceeds {MAX_UNPACKED_BYTES} bytes");
                }
            } else {
                bail!(
                    "refusing extracted special file at '{}'",
                    child_rel.display()
                );
            }
        }
    }
    Ok(())
}

fn validate_relative_symlink_lexically(rel_link: &Path, target: &str) -> Result<()> {
    if target.is_empty() || target.contains('\0') {
        bail!("invalid symlink target at '{}'", rel_link.display());
    }
    if target.starts_with('/') {
        bail!(
            "refusing absolute symlink '{}' -> '{target}'",
            rel_link.display()
        );
    }
    let parent = rel_link.parent().unwrap_or_else(|| Path::new(""));
    let mut depth = parent
        .components()
        .filter(|comp| matches!(comp, Component::Normal(_)))
        .count();

    for component in Path::new(target).components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if depth == 0 {
                    bail!(
                        "refusing escaping symlink '{}' -> '{target}'",
                        rel_link.display()
                    );
                }
                depth -= 1;
            }
            Component::Normal(_) => depth += 1,
            Component::RootDir | Component::Prefix(_) => {
                bail!(
                    "refusing non-relative symlink '{}' -> '{target}'",
                    rel_link.display()
                );
            }
        }
    }
    Ok(())
}

fn validate_archive_entry_path(raw: &str, allow_trailing_slash: bool) -> Result<()> {
    let trimmed = if allow_trailing_slash {
        raw.trim_end_matches('/')
    } else {
        raw
    };
    if trimmed.is_empty() || trimmed == "." {
        return if allow_trailing_slash && (raw == "./" || raw == ".") {
            Ok(())
        } else {
            bail!("archive contains empty entry path")
        };
    }
    if trimmed.contains('\0') || trimmed.contains('\\') || trimmed.starts_with('/') {
        bail!("archive contains unsafe entry path '{raw}'");
    }
    if let Some(first_byte) = trimmed.as_bytes().get(1) {
        if *first_byte == b':' && trimmed.as_bytes()[0].is_ascii_alphabetic() {
            bail!("archive contains Windows drive path '{raw}'");
        }
    }
    let mut depth = 0_usize;
    for comp in Path::new(trimmed).components() {
        match comp {
            Component::CurDir => {}
            Component::Normal(_) => depth += 1,
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                bail!("archive contains disallowed path component in '{raw}'");
            }
        }
    }
    if depth > MAX_DEPTH {
        bail!("archive entry '{raw}' exceeds maximum depth {MAX_DEPTH}");
    }
    Ok(())
}

fn normalize_relative_entry_path(path: &Path) -> Result<PathBuf> {
    let raw = path
        .to_str()
        .context("archive entry path contains non-UTF-8 characters")?;
    validate_archive_entry_path(raw, true)?;
    let mut normalized = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::CurDir => {}
            Component::Normal(part) => normalized.push(part),
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                bail!("archive contains disallowed path '{raw}'");
            }
        }
    }
    Ok(normalized)
}

/// Arrange unpacked artifacts into `<stage>/apps`, `<stage>/caskroom`, and
/// `<stage>/wrappers`, returning metadata needed by `cask_store::commit`.
fn assemble_stage(
    stage_dir: &Path,
    unpack_dir: &Path,
    planned: &PlannedCask,
    prefix: &Path,
    appdir: &Path,
) -> Result<StagedCaskPayload> {
    let apps_stage = stage_dir.join("apps");
    let caskroom_stage = stage_dir.join("caskroom");
    let wrappers_stage = stage_dir.join("wrappers");

    let mut app_bundles = Vec::new();
    let mut staged_apps_by_name: HashMap<String, PathBuf> = HashMap::new();

    for artifact in &planned.artifacts {
        if let PlannedCaskArtifact::App {
            source,
            bundle_name,
        } = artifact
        {
            fs::create_dir_all(&apps_stage)?;
            let src = resolve_unpack_dir_path(unpack_dir, source)?;
            let meta = fs::symlink_metadata(&src).with_context(|| {
                format!(
                    "cask '{}' declared app '{}' which was not found in the archive",
                    planned.token, source
                )
            })?;
            if meta.file_type().is_symlink() || !meta.is_dir() {
                bail!(
                    "cask '{}' app '{source}' is not a regular .app directory",
                    planned.token
                );
            }
            if !src.join("Contents").is_dir() {
                bail!(
                    "cask '{}' app '{source}' is missing a Contents directory",
                    planned.token
                );
            }
            let dst = apps_stage.join(bundle_name);
            fs::rename(&src, &dst).with_context(|| {
                format!(
                    "failed to stage app bundle '{}' into '{}'",
                    bundle_name,
                    dst.display()
                )
            })?;
            staged_apps_by_name.insert(bundle_name.clone(), dst.clone());
            app_bundles.push(StagedAppBundle {
                bundle_name: bundle_name.clone(),
                staged_dir: dst,
            });
        }
    }

    let mut caskroom_files = Vec::new();
    let mut copied_payload_paths: HashMap<String, String> = HashMap::new();
    let mut symlinks = Vec::new();
    let mut wrappers = Vec::new();

    for artifact in &planned.artifacts {
        match artifact {
            PlannedCaskArtifact::App { .. } => {}
            PlannedCaskArtifact::Binary { source, link_name } => {
                let target = stage_source_and_compute_target(
                    source,
                    unpack_dir,
                    &caskroom_stage,
                    &staged_apps_by_name,
                    prefix,
                    appdir,
                    &planned.token,
                    &planned.version,
                    true,
                    &mut caskroom_files,
                    &mut copied_payload_paths,
                    "../",
                )?;
                symlinks.push(StagedCaskLink {
                    relative_dest: format!("bin/{link_name}"),
                    target,
                });
            }
            PlannedCaskArtifact::PrefixLink {
                source,
                relative_dest,
            } => {
                let depth = relative_dest.matches('/').count();
                let up = "../".repeat(depth);
                let target = stage_source_and_compute_target(
                    source,
                    unpack_dir,
                    &caskroom_stage,
                    &staged_apps_by_name,
                    prefix,
                    appdir,
                    &planned.token,
                    &planned.version,
                    false,
                    &mut caskroom_files,
                    &mut copied_payload_paths,
                    &up,
                )?;
                symlinks.push(StagedCaskLink {
                    relative_dest: relative_dest.clone(),
                    target,
                });
            }
            PlannedCaskArtifact::CommandWrapper {
                executable,
                link_name,
                args,
            } => {
                let exec_abs = stage_source_for_wrapper_exec(
                    executable,
                    unpack_dir,
                    &caskroom_stage,
                    &staged_apps_by_name,
                    prefix,
                    appdir,
                    &planned.token,
                    &planned.version,
                    &mut caskroom_files,
                    &mut copied_payload_paths,
                )?;
                fs::create_dir_all(&wrappers_stage)?;
                let script = build_command_wrapper_script(&exec_abs, args);
                let wrapper_path = wrappers_stage.join(link_name);
                fs::write(&wrapper_path, script.as_bytes()).with_context(|| {
                    format!(
                        "failed to write command wrapper '{}'",
                        wrapper_path.display()
                    )
                })?;
                set_executable(&wrapper_path)?;
                let digest = hex::encode(Sha256::digest(script.as_bytes()));
                wrappers.push(StagedWrapper {
                    relative_dest: format!("bin/{link_name}"),
                    staged_file: wrapper_path,
                    sha256: digest,
                });
            }
        }
    }

    caskroom_files.sort_unstable();
    let has_caskroom_files = !caskroom_files.is_empty();
    let _ = fs::remove_dir_all(unpack_dir);

    Ok(StagedCaskPayload {
        app_bundles,
        has_caskroom_files,
        caskroom_files,
        symlinks,
        wrappers,
    })
}

#[allow(clippy::too_many_arguments)]
fn stage_source_and_compute_target(
    source: &CaskSourcePath,
    unpack_dir: &Path,
    caskroom_stage: &Path,
    staged_apps: &HashMap<String, PathBuf>,
    prefix: &Path,
    appdir: &Path,
    token: &str,
    version: &str,
    chmod_exec: bool,
    caskroom_files: &mut Vec<String>,
    copied_payload_paths: &mut HashMap<String, String>,
    relative_up_to_prefix: &str,
) -> Result<String> {
    match source {
        CaskSourcePath::Payload(rel) => {
            ensure_payload_file_staged(
                unpack_dir,
                caskroom_stage,
                rel,
                chmod_exec,
                caskroom_files,
                copied_payload_paths,
            )?;
            Ok(format!(
                "{relative_up_to_prefix}Caskroom/{token}/{version}/{rel}"
            ))
        }
        CaskSourcePath::AppBundle {
            bundle_name,
            inner_path,
        } => {
            let bundle_dir = staged_apps
                .get(bundle_name)
                .with_context(|| format!("referenced app bundle '{bundle_name}' was not staged"))?;
            verify_regular_file_in_bundle(bundle_dir, inner_path)?;
            if appdir == prefix.join("Applications") {
                Ok(format!(
                    "{relative_up_to_prefix}Applications/{bundle_name}/{inner_path}"
                ))
            } else {
                Ok(appdir
                    .join(bundle_name)
                    .join(inner_path)
                    .to_string_lossy()
                    .into_owned())
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn stage_source_for_wrapper_exec(
    executable: &CaskSourcePath,
    unpack_dir: &Path,
    caskroom_stage: &Path,
    staged_apps: &HashMap<String, PathBuf>,
    prefix: &Path,
    appdir: &Path,
    token: &str,
    version: &str,
    caskroom_files: &mut Vec<String>,
    copied_payload_paths: &mut HashMap<String, String>,
) -> Result<PathBuf> {
    match executable {
        CaskSourcePath::Payload(rel) => {
            ensure_payload_file_staged(
                unpack_dir,
                caskroom_stage,
                rel,
                true,
                caskroom_files,
                copied_payload_paths,
            )?;
            Ok(prefix.join("Caskroom").join(token).join(version).join(rel))
        }
        CaskSourcePath::AppBundle {
            bundle_name,
            inner_path,
        } => {
            let bundle_dir = staged_apps
                .get(bundle_name)
                .with_context(|| format!("referenced app bundle '{bundle_name}' was not staged"))?;
            verify_regular_file_in_bundle(bundle_dir, inner_path)?;
            Ok(appdir.join(bundle_name).join(inner_path))
        }
    }
}

fn ensure_payload_file_staged(
    unpack_dir: &Path,
    caskroom_stage: &Path,
    rel: &str,
    chmod_exec: bool,
    caskroom_files: &mut Vec<String>,
    copied_payload_paths: &mut HashMap<String, String>,
) -> Result<()> {
    if copied_payload_paths.contains_key(rel) {
        if chmod_exec {
            set_executable(&caskroom_stage.join(rel))?;
        }
        return Ok(());
    }
    let src = resolve_unpack_dir_path(unpack_dir, rel)?;
    let meta = fs::symlink_metadata(&src)
        .with_context(|| format!("missing declared payload artifact '{rel}'"))?;
    if meta.file_type().is_symlink() || !meta.is_file() {
        bail!("declared payload artifact '{rel}' is not a regular file");
    }
    let dst = caskroom_stage.join(rel);
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::rename(&src, &dst)
        .or_else(|_| fs::copy(&src, &dst).map(|_| ()))
        .with_context(|| format!("failed to stage Caskroom file '{rel}'"))?;
    if chmod_exec {
        set_executable(&dst)?;
    }
    copied_payload_paths.insert(rel.to_string(), rel.to_string());
    caskroom_files.push(rel.to_string());
    Ok(())
}

fn resolve_unpack_dir_path(unpack_dir: &Path, rel: &str) -> Result<PathBuf> {
    let mut cur = unpack_dir.to_path_buf();
    for segment in rel.split('/') {
        cur.push(segment);
        let meta = fs::symlink_metadata(&cur)
            .with_context(|| format!("extracted artifact '{rel}' not found in archive"))?;
        if meta.file_type().is_symlink() {
            bail!("refusing symlink along declared artifact path '{rel}'");
        }
    }
    Ok(cur)
}

/// Resolve `inner_path` inside `bundle_dir` without escaping `bundle_dir`,
/// allowing bounded relative symlinks within the bundle.
fn verify_regular_file_in_bundle(bundle_dir: &Path, inner_path: &str) -> Result<()> {
    let mut current_rel = PathBuf::new();
    let mut remaining: Vec<String> = inner_path.split('/').map(str::to_string).collect();
    remaining.reverse();
    let mut hops = 0_usize;

    while let Some(seg) = remaining.pop() {
        match seg.as_str() {
            "" | "." => continue,
            ".." => {
                if !current_rel.pop() {
                    bail!(
                        "path '{inner_path}' escapes bundle '{}'",
                        bundle_dir.display()
                    );
                }
                continue;
            }
            _ => {}
        }
        current_rel.push(&seg);
        let abs = bundle_dir.join(&current_rel);
        let meta = fs::symlink_metadata(&abs).with_context(|| {
            format!(
                "missing file '{inner_path}' inside bundle '{}'",
                bundle_dir.display()
            )
        })?;
        if meta.file_type().is_symlink() {
            hops += 1;
            if hops > MAX_SYMLINK_HOPS {
                bail!("symlink loop resolving '{inner_path}' in bundle");
            }
            let target = fs::read_link(&abs)?;
            let target_str = target
                .to_str()
                .context("non-UTF-8 symlink inside .app bundle")?;
            validate_relative_symlink_lexically(&current_rel, target_str)?;
            current_rel.pop();
            let mut replacement: Vec<String> = target_str.split('/').map(str::to_string).collect();
            replacement.reverse();
            remaining.extend(replacement);
        } else if !remaining.is_empty() && !meta.is_dir() {
            bail!(
                "non-directory component along '{inner_path}' in '{}'",
                bundle_dir.display()
            );
        }
    }

    let final_meta = fs::symlink_metadata(bundle_dir.join(&current_rel)).with_context(|| {
        format!(
            "missing target '{inner_path}' in bundle '{}'",
            bundle_dir.display()
        )
    })?;
    if !final_meta.is_file() {
        bail!(
            "target '{inner_path}' inside '{}' is not a regular file",
            bundle_dir.display()
        );
    }
    Ok(())
}

pub(super) fn build_command_wrapper_script(executable: &Path, args: &[String]) -> String {
    let mut script = String::from("#!/bin/sh\nexec ");
    script.push_str(&shell_quote(&executable.to_string_lossy()));
    for arg in args {
        script.push(' ');
        script.push_str(&shell_quote(arg));
    }
    script.push_str(" \"$@\"\n");
    script
}

fn shell_quote(value: &str) -> String {
    let mut quoted = String::with_capacity(value.len() + 2);
    quoted.push('\'');
    for ch in value.chars() {
        if ch == '\'' {
            quoted.push_str("'\\''");
        } else {
            quoted.push(ch);
        }
    }
    quoted.push('\'');
    quoted
}

fn set_executable(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o755))
            .with_context(|| format!("failed to chmod 755 '{}'", path.display()))?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::Compression;
    use flate2::write::GzEncoder;
    use tempfile::tempdir;

    fn sha256_hex(bytes: &[u8]) -> String {
        hex::encode(Sha256::digest(bytes))
    }

    #[test]
    fn detects_container_formats_and_stages_naked_binary() {
        let temp = tempdir().unwrap();
        let prefix = temp.path().join("prefix");
        let appdir = prefix.join("Applications");

        let binary_bytes = b"#!/bin/sh\necho hi\n";
        let digest = sha256_hex(binary_bytes);
        let download = temp.path().join("claude.download");
        fs::write(&download, binary_bytes).unwrap();
        assert_eq!(
            detect_format(&download, "https://example.com/claude", ContainerHint::Auto).unwrap(),
            PayloadFormat::Naked
        );

        let macho = temp.path().join("macho.download");
        fs::write(&macho, [0xCF, 0xFA, 0xED, 0xFE, 0, 0, 0, 0]).unwrap();
        assert_eq!(
            detect_format(&macho, "https://example.com/bin", ContainerHint::Auto).unwrap(),
            PayloadFormat::Naked
        );

        let stage_dir = temp.path().join("stage");
        fs::create_dir(&stage_dir).unwrap();
        let planned = PlannedCask {
            token: "claude-code@latest".into(),
            name: vec!["Claude Code".into()],
            version: "2.1.330".into(),
            auto_updates: false,
            url: "https://example.com/claude".into(),
            sha256: digest,
            container: ContainerHint::Auto,
            artifacts: vec![PlannedCaskArtifact::Binary {
                source: CaskSourcePath::Payload("claude".into()),
                link_name: "claude".into(),
            }],
            conflicts_with_casks: Vec::new(),
            conflicts_with_formulae: Vec::new(),
            install_blockers: Vec::new(),
        };

        let staged = stage(&download, &stage_dir, &planned, &prefix, &appdir).unwrap();
        assert!(staged.has_caskroom_files);
        assert_eq!(staged.caskroom_files, vec!["claude"]);
        assert!(stage_dir.join("caskroom/claude").is_file());
        assert_eq!(staged.symlinks.len(), 1);
        assert_eq!(staged.symlinks[0].relative_dest, "bin/claude");
        assert_eq!(
            staged.symlinks[0].target,
            "../Caskroom/claude-code@latest/2.1.330/claude"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn preflights_zip_with_ditto_preserving_bundle_symlinks_and_builds_wrapper() {
        use std::os::unix::fs::symlink;

        let temp = tempdir().unwrap();
        let src_root = temp.path().join("zip-src");
        let app = src_root.join("Demo App.app/Contents");
        fs::create_dir_all(app.join("MacOS")).unwrap();
        fs::create_dir_all(app.join("Frameworks/Sub.framework/Versions/A")).unwrap();
        fs::write(app.join("Info.plist"), b"<plist/>").unwrap();
        fs::write(app.join("MacOS/demo"), b"#!/bin/sh\necho ok\n").unwrap();
        set_executable(&app.join("MacOS/demo")).unwrap();
        fs::write(app.join("Frameworks/Sub.framework/Versions/A/Sub"), b"lib").unwrap();
        symlink("A", app.join("Frameworks/Sub.framework/Versions/Current")).unwrap();
        symlink(
            "Versions/Current/Sub",
            app.join("Frameworks/Sub.framework/Sub"),
        )
        .unwrap();

        let zip_path = temp.path().join("demo.zip");
        let status = Command::new("/usr/bin/ditto")
            .args(["-c", "-k", "--keepParent"])
            .arg(src_root.join("Demo App.app"))
            .arg(&zip_path)
            .status()
            .unwrap();
        assert!(status.success());

        let zip_bytes = fs::read(&zip_path).unwrap();
        let digest = sha256_hex(&zip_bytes);
        let prefix = temp.path().join("prefix");
        let appdir = prefix.join("Applications");
        let stage_dir = temp.path().join("stage");
        fs::create_dir(&stage_dir).unwrap();

        let planned = PlannedCask {
            token: "demo".into(),
            name: vec!["Demo App".into()],
            version: "1.0".into(),
            auto_updates: true,
            url: "https://example.com/demo.zip".into(),
            sha256: digest,
            container: ContainerHint::Zip,
            artifacts: vec![
                PlannedCaskArtifact::App {
                    source: "Demo App.app".into(),
                    bundle_name: "Demo App.app".into(),
                },
                PlannedCaskArtifact::CommandWrapper {
                    executable: CaskSourcePath::AppBundle {
                        bundle_name: "Demo App.app".into(),
                        inner_path: "Contents/MacOS/demo".into(),
                    },
                    link_name: "demo-cli".into(),
                    args: vec!["--flag=a'b".into()],
                },
            ],
            conflicts_with_casks: Vec::new(),
            conflicts_with_formulae: Vec::new(),
            install_blockers: Vec::new(),
        };

        let staged = stage(&zip_path, &stage_dir, &planned, &prefix, &appdir).unwrap();
        assert!(!staged.has_caskroom_files);
        assert_eq!(staged.app_bundles.len(), 1);
        assert!(
            staged.app_bundles[0]
                .staged_dir
                .join("Contents/Frameworks/Sub.framework/Sub")
                .is_symlink()
        );
        assert_eq!(staged.wrappers.len(), 1);
        let wrapper_contents = fs::read_to_string(&staged.wrappers[0].staged_file).unwrap();
        assert!(wrapper_contents.starts_with("#!/bin/sh\nexec '"));
        assert!(wrapper_contents.contains("'--flag=a'\\''b' \"$@\"\n"));
        assert_eq!(
            staged.wrappers[0].sha256,
            sha256_hex(wrapper_contents.as_bytes())
        );
    }

    #[test]
    fn rejects_zip_slip_central_directory_and_escaping_symlinks() {
        // Minimal hand-crafted ZIP with `../evil` in Central Directory.
        let fname = b"../evil";
        let mut cd = Vec::new();
        cd.extend_from_slice(b"PK\x01\x02");
        cd.extend_from_slice(&[20, 0, 20, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        cd.extend_from_slice(&0_u32.to_le_bytes()); // crc
        cd.extend_from_slice(&0_u32.to_le_bytes()); // comp
        cd.extend_from_slice(&4_u32.to_le_bytes()); // uncomp
        cd.extend_from_slice(&(fname.len() as u16).to_le_bytes());
        cd.extend_from_slice(&0_u16.to_le_bytes());
        cd.extend_from_slice(&0_u16.to_le_bytes());
        cd.extend_from_slice(&[0; 8]);
        cd.extend_from_slice(&0_u32.to_le_bytes());
        cd.extend_from_slice(fname);

        let mut zip = Vec::new();
        let cd_offset = zip.len() as u32;
        let cd_size = cd.len() as u32;
        zip.extend_from_slice(&cd);
        zip.extend_from_slice(b"PK\x05\x06");
        zip.extend_from_slice(&[0; 4]);
        zip.extend_from_slice(&1_u16.to_le_bytes());
        zip.extend_from_slice(&1_u16.to_le_bytes());
        zip.extend_from_slice(&cd_size.to_le_bytes());
        zip.extend_from_slice(&cd_offset.to_le_bytes());
        zip.extend_from_slice(&0_u16.to_le_bytes());

        let temp = tempdir().unwrap();
        let zip_path = temp.path().join("slip.zip");
        fs::write(&zip_path, &zip).unwrap();
        let err = preflight_zip(&zip_path).unwrap_err();
        assert!(format!("{err:#}").contains("disallowed path"), "{err:#}");

        let tree = temp.path().join("tree");
        fs::create_dir(&tree).unwrap();
        std::os::unix::fs::symlink("../../etc/passwd", tree.join("escape")).unwrap();
        let err = validate_extracted_tree(&tree).unwrap_err();
        assert!(format!("{err:#}").contains("escaping symlink"), "{err:#}");
    }

    #[test]
    fn stages_tar_gz_cask_payload() {
        let temp = tempdir().unwrap();
        let tgz_path = temp.path().join("tool.tar.gz");
        {
            let file = File::create(&tgz_path).unwrap();
            let enc = GzEncoder::new(file, Compression::default());
            let mut builder = tar::Builder::new(enc);
            let bin_data = b"#!/bin/sh\necho tool\n";
            let mut header = tar::Header::new_gnu();
            header.set_size(bin_data.len() as u64);
            header.set_mode(0o755);
            header.set_cksum();
            builder
                .append_data(&mut header, "bin/mytool", &bin_data[..])
                .unwrap();
            builder.into_inner().unwrap().finish().unwrap();
        }
        let digest = sha256_hex(&fs::read(&tgz_path).unwrap());
        let prefix = temp.path().join("prefix");
        let appdir = prefix.join("Applications");
        let stage_dir = temp.path().join("stage");
        fs::create_dir(&stage_dir).unwrap();

        let planned = PlannedCask {
            token: "mytool".into(),
            name: vec![],
            version: "1.2".into(),
            auto_updates: false,
            url: "https://example.com/tool.tar.gz".into(),
            sha256: digest,
            container: ContainerHint::Auto,
            artifacts: vec![PlannedCaskArtifact::Binary {
                source: CaskSourcePath::Payload("bin/mytool".into()),
                link_name: "mytool".into(),
            }],
            conflicts_with_casks: Vec::new(),
            conflicts_with_formulae: Vec::new(),
            install_blockers: Vec::new(),
        };

        let staged = stage(&tgz_path, &stage_dir, &planned, &prefix, &appdir).unwrap();
        assert_eq!(staged.caskroom_files, vec!["bin/mytool"]);
        assert_eq!(
            staged.symlinks[0].target,
            "../Caskroom/mytool/1.2/bin/mytool"
        );
    }

    #[test]
    fn parses_hdiutil_plist_mount_points_with_spaces_and_entities() {
        let plist = r#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0">
<dict>
  <key>system-entities</key>
  <array>
    <dict>
      <key>content-hint</key>
      <string>GUID_partition_scheme</string>
    </dict>
    <dict>
      <key>content-hint</key>
      <string>Apple_HFS</string>
      <key>mount-point</key>
      <string>/tmp/smoke/dmg.abc123/App &amp; Tools</string>
    </dict>
  </array>
</dict>
</plist>"#;
        let path = parse_hdiutil_mount_point(plist).unwrap();
        assert_eq!(path, PathBuf::from("/tmp/smoke/dmg.abc123/App & Tools"));
        assert!(parse_hdiutil_mount_point("<plist><dict></dict></plist>").is_err());
    }
}
