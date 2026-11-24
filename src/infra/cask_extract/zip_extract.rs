//! ZIP preflight (Central Directory scan) and `ditto` extraction.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::process::{Command, Stdio};

use super::error::CaskExtractError;
use super::limits::{MAX_ARCHIVE_BYTES, MAX_ENTRIES, MAX_UNPACKED_BYTES};
use super::tree::validate_archive_entry_path;

/// Maximum EOCD search tail in bytes (22-byte header + 64 KiB max comment).
const MAX_EOCD_TAIL_BYTES: usize = 22 + u16::MAX as usize;

/// Preflight a ZIP archive by scanning its Central Directory in pure Rust before
/// delegating extraction to `/usr/bin/ditto`.
pub(super) fn preflight_zip(path: &Path) -> Result<(), CaskExtractError> {
    let io_error = |action: &'static str| {
        move |source: std::io::Error| CaskExtractError::Io {
            action,
            path: path.to_path_buf(),
            source,
        }
    };
    let mut file = File::open(path).map_err(io_error("open ZIP"))?;
    let file_len = file.metadata().map_err(io_error("inspect ZIP"))?.len();
    if !(22..=MAX_ARCHIVE_BYTES).contains(&file_len) {
        return Err(CaskExtractError::ZipInvalidSize {
            path: path.to_path_buf(),
            size: file_len,
        });
    }

    let tail_len = (file_len as usize).min(MAX_EOCD_TAIL_BYTES);
    let tail_start = file_len - tail_len as u64;
    file.seek(SeekFrom::Start(tail_start))
        .map_err(|source| CaskExtractError::Stream {
            action: "seek to ZIP EOCD",
            source,
        })?;
    let mut tail = vec![0_u8; tail_len];
    file.read_exact(&mut tail)
        .map_err(|source| CaskExtractError::Stream {
            action: "read ZIP EOCD",
            source,
        })?;

    let eocd_rel = find_eocd(&tail).ok_or(CaskExtractError::ZipMissingEocd)?;
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
                return Err(CaskExtractError::ZipMultiDisk);
            }
            (
                u64::from(entries_32),
                u64::from(cd_size_32),
                u64::from(cd_offset_32),
            )
        };

    if entries_count > MAX_ENTRIES as u64 {
        return Err(CaskExtractError::ZipTooManyEntries {
            count: entries_count,
            limit: MAX_ENTRIES,
        });
    }
    let cd_end = cd_offset
        .checked_add(cd_size)
        .ok_or(CaskExtractError::ZipDirectoryOverflow)?;
    if cd_end > eocd_offset || cd_end > file_len {
        return Err(CaskExtractError::ZipDirectoryOutOfBounds);
    }

    file.seek(SeekFrom::Start(cd_offset))
        .map_err(|source| CaskExtractError::Stream {
            action: "seek to ZIP central directory",
            source,
        })?;
    let mut cd = vec![
        0_u8;
        usize::try_from(cd_size)
            .map_err(|source| CaskExtractError::ZipDirectoryTooLarge { source })?
    ];
    file.read_exact(&mut cd)
        .map_err(|source| CaskExtractError::Stream {
            action: "read ZIP central directory",
            source,
        })?;

    let mut pos = 0_usize;
    let mut total_uncompressed = 0_u64;
    for _ in 0..entries_count {
        if pos + 46 > cd.len() {
            return Err(CaskExtractError::ZipTruncatedEntry);
        }
        let header = &cd[pos..pos + 46];
        if &header[0..4] != b"PK\x01\x02" {
            return Err(CaskExtractError::ZipInvalidEntrySignature);
        }
        let comp_size_32 = read_u32_le(header, 20)?;
        let uncomp_size_32 = read_u32_le(header, 24)?;
        let fname_len = usize::from(read_u16_le(header, 28)?);
        let extra_len = usize::from(read_u16_le(header, 30)?);
        let comment_len = usize::from(read_u16_le(header, 32)?);

        let entry_end = pos + 46 + fname_len + extra_len + comment_len;
        if entry_end > cd.len() {
            return Err(CaskExtractError::ZipTruncatedMetadata);
        }
        let fname_bytes = &cd[pos + 46..pos + 46 + fname_len];
        let extra = &cd[pos + 46 + fname_len..pos + 46 + fname_len + extra_len];

        let fname = std::str::from_utf8(fname_bytes)
            .map_err(|source| CaskExtractError::ZipNameNotUtf8 { source })?;
        validate_archive_entry_path(fname, true)?;

        let uncomp_size = if uncomp_size_32 == u32::MAX {
            parse_zip64_uncompressed_size(extra, comp_size_32 == u32::MAX)
                .ok_or(CaskExtractError::ZipMissingZip64Size)?
        } else {
            u64::from(uncomp_size_32)
        };
        if uncomp_size > MAX_ARCHIVE_BYTES {
            return Err(CaskExtractError::ZipEntryTooLarge {
                name: fname.to_owned(),
                limit: MAX_ARCHIVE_BYTES,
            });
        }
        total_uncompressed = total_uncompressed
            .checked_add(uncomp_size)
            .ok_or(CaskExtractError::ZipSizeOverflow)?;
        if total_uncompressed > MAX_UNPACKED_BYTES {
            return Err(CaskExtractError::ZipTooLarge {
                limit: MAX_UNPACKED_BYTES,
            });
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

fn parse_zip64_eocd(
    file: &mut File,
    eocd_offset: u64,
) -> Result<(u64, u64, u64), CaskExtractError> {
    let stream = |action: &'static str| move |source| CaskExtractError::Stream { action, source };
    if eocd_offset < 20 {
        return Err(CaskExtractError::Zip64InvalidLocatorOffset);
    }
    file.seek(SeekFrom::Start(eocd_offset - 20))
        .map_err(stream("seek to ZIP64 EOCD locator"))?;
    let mut locator = [0_u8; 20];
    file.read_exact(&mut locator)
        .map_err(stream("read ZIP64 EOCD locator"))?;
    if &locator[0..4] != b"PK\x06\x07" {
        return Err(CaskExtractError::Zip64MissingLocator);
    }
    let z64_offset = read_u64_le(&locator, 8)?;
    if z64_offset + 56 > eocd_offset {
        return Err(CaskExtractError::Zip64OffsetOutOfBounds);
    }
    file.seek(SeekFrom::Start(z64_offset))
        .map_err(stream("seek to ZIP64 EOCD"))?;
    let mut z64 = [0_u8; 56];
    file.read_exact(&mut z64)
        .map_err(stream("read ZIP64 EOCD"))?;
    if &z64[0..4] != b"PK\x06\x06" {
        return Err(CaskExtractError::Zip64InvalidSignature);
    }
    let entries = read_u64_le(&z64, 32)?;
    let cd_size = read_u64_le(&z64, 40)?;
    let cd_offset = read_u64_le(&z64, 48)?;
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

fn read_u16_le(slice: &[u8], offset: usize) -> Result<u16, CaskExtractError> {
    let bytes: [u8; 2] = slice
        .get(offset..offset + 2)
        .and_then(|raw| raw.try_into().ok())
        .ok_or(CaskExtractError::ZipUnexpectedEof { bits: 16 })?;
    Ok(u16::from_le_bytes(bytes))
}

fn read_u32_le(slice: &[u8], offset: usize) -> Result<u32, CaskExtractError> {
    let bytes: [u8; 4] = slice
        .get(offset..offset + 4)
        .and_then(|raw| raw.try_into().ok())
        .ok_or(CaskExtractError::ZipUnexpectedEof { bits: 32 })?;
    Ok(u32::from_le_bytes(bytes))
}

fn read_u64_le(slice: &[u8], offset: usize) -> Result<u64, CaskExtractError> {
    let bytes: [u8; 8] = slice
        .get(offset..offset + 8)
        .and_then(|raw| raw.try_into().ok())
        .ok_or(CaskExtractError::ZipUnexpectedEof { bits: 64 })?;
    Ok(u64::from_le_bytes(bytes))
}

pub(super) fn run_ditto_extract(
    zip_path: &Path,
    unpack_dir: &Path,
) -> Result<(), CaskExtractError> {
    let status = Command::new("/usr/bin/ditto")
        .arg("-x")
        .arg("-k")
        .arg(zip_path)
        .arg(unpack_dir)
        .stdin(Stdio::null())
        .status()
        .map_err(|source| CaskExtractError::Stream {
            action: "execute /usr/bin/ditto",
            source,
        })?;
    if !status.success() {
        return Err(CaskExtractError::DittoExtractFailed {
            path: zip_path.to_path_buf(),
            status,
        });
    }
    Ok(())
}
