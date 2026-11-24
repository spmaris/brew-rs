//! ZIP preflight (Central Directory scan) and `ditto` extraction.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

use super::limits::{MAX_ARCHIVE_BYTES, MAX_ENTRIES, MAX_UNPACKED_BYTES};
use super::tree::validate_archive_entry_path;

/// Maximum EOCD search tail in bytes (22-byte header + 64 KiB max comment).
const MAX_EOCD_TAIL_BYTES: usize = 22 + u16::MAX as usize;

/// Preflight a ZIP archive by scanning its Central Directory in pure Rust before
/// delegating extraction to `/usr/bin/ditto`.
pub(super) fn preflight_zip(path: &Path) -> Result<()> {
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

pub(super) fn run_ditto_extract(zip_path: &Path, unpack_dir: &Path) -> Result<()> {
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
