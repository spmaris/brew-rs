//! Bounded parsing and validation of thin 64-bit Mach-O load commands.

use std::collections::HashMap;
use std::ops::Range;

use super::constants::*;
use super::error::MachoError;
use super::fat::{FatFamily, has_magic};
use super::model::{Header, Kind, LoadCommand, Parsed};

/// Size of `mach_header_64`.
pub(super) const MACH_HEADER_64_BYTES: usize = 32;

/// Largest number of load commands accepted. Real images have far fewer.
pub(super) const MAX_LOAD_COMMANDS: u32 = 4096;
/// Longest load-command path accepted, in bytes.
const MAX_NAME_BYTES: usize = 8192;

/// Parse and bound-check the header and every load command in `bytes`.
pub(super) fn parse(bytes: &[u8]) -> Result<Parsed, MachoError> {
    let Some(magic) = bytes.get(..4) else {
        return Err(MachoError::TooSmall);
    };
    if FatFamily::from_magic(magic).is_some() {
        return Err(MachoError::Fat);
    }
    if !has_magic(magic, MH_MAGIC_64_BYTES) {
        if has_magic(magic, MH_CIGAM_64_BYTES) {
            return Err(MachoError::BigEndian);
        }
        if has_magic(magic, MH_MAGIC_32_BYTES) || has_magic(magic, MH_CIGAM_32_BYTES) {
            return Err(MachoError::Bit32);
        }
        return Err(MachoError::NotMacho);
    }
    if bytes.len() < MACH_HEADER_64_BYTES {
        return Err(MachoError::TruncatedHeader);
    }

    let cputype = read_u32(bytes, 4)?;
    let cpusubtype = read_u32(bytes, 8)?;
    let filetype = read_u32(bytes, 12)?;
    let command_count = read_u32(bytes, 16)?;
    let command_bytes = read_u32(bytes, 20)? as usize;

    if !matches!(cputype, CPU_TYPE_ARM64 | CPU_TYPE_X86_64) {
        return Err(MachoError::UnsupportedCpu { cputype });
    }
    if command_count == 0 || command_count > MAX_LOAD_COMMANDS {
        return Err(MachoError::BadCommandCount {
            count: command_count,
        });
    }
    let command_count = command_count as usize;
    let Some(commands_end) = MACH_HEADER_64_BYTES.checked_add(command_bytes) else {
        return Err(MachoError::TableSizeOverflow);
    };
    if commands_end > bytes.len() {
        return Err(MachoError::TablePastEof {
            bytes: command_bytes,
        });
    }
    if command_bytes < command_count * 8 {
        return Err(MachoError::TableTooSmall {
            count: command_count,
        });
    }

    let mut commands = Vec::with_capacity(command_count);
    let mut offset = MACH_HEADER_64_BYTES;
    let mut signature_commands = 0usize;
    let mut signature_data_size = 0usize;
    for index in 0..command_count {
        let Some(command_end_probe) = offset.checked_add(8) else {
            return Err(MachoError::CommandOffsetOverflow { index });
        };
        if command_end_probe > commands_end {
            return Err(MachoError::TruncatedCommand { index });
        }
        let command = read_u32(bytes, offset)?;
        let command_size = read_u32(bytes, offset + 4)? as usize;
        if command_size < 8 || !command_size.is_multiple_of(8) {
            return Err(MachoError::InvalidCommandSize {
                index,
                size: command_size,
            });
        }
        let Some(command_end) = offset.checked_add(command_size) else {
            return Err(MachoError::CommandSizeOverflow { index });
        };
        if command_end > commands_end {
            return Err(MachoError::CommandPastTable { index });
        }

        let kind = Kind::from_command(command);
        let (name, name_range) = match kind {
            Some(kind) => {
                let (name, range) = parse_name(bytes, offset, command_size, kind)?;
                (Some(name), Some(range))
            }
            None => (None, None),
        };

        if command == LC_CODE_SIGNATURE {
            signature_commands += 1;
            if signature_commands > 1 {
                return Err(MachoError::MultipleSignatures);
            }
            if command_size != 16 {
                return Err(MachoError::SignatureSize { size: command_size });
            }
            let data_offset = read_u32(bytes, offset + 8)? as usize;
            let data_size = read_u32(bytes, offset + 12)? as usize;
            signature_data_size = data_size;
            if data_size > 0 {
                let Some(data_end) = data_offset.checked_add(data_size) else {
                    return Err(MachoError::SignatureRangeOverflow);
                };
                if data_offset < commands_end || data_end > bytes.len() {
                    return Err(MachoError::SignatureOutside);
                }
            }
        }

        commands.push(LoadCommand {
            command,
            kind,
            name,
            name_range,
        });
        offset = command_end;
    }
    if offset != commands_end {
        return Err(MachoError::CommandSizeSum {
            actual: offset - MACH_HEADER_64_BYTES,
            declared: command_bytes,
        });
    }

    Ok(Parsed {
        header: Header {
            cputype,
            cpusubtype,
            filetype,
        },
        commands,
        signed: signature_data_size > 0,
    })
}

/// Parse the path of an `LC_*_DYLIB` or `LC_RPATH` command.
///
/// The string must start at the documented struct offset (or after zero
/// padding), be NUL-terminated and UTF-8, and leave only zero padding through
/// the end of the command. Ambiguous layouts are rejected rather than guessed
/// at.
fn parse_name(
    bytes: &[u8],
    command_offset: usize,
    command_size: usize,
    kind: Kind,
) -> Result<(String, Range<usize>), MachoError> {
    let (minimum_offset, minimum_size) = if kind.is_rpath() {
        (12usize, 12usize)
    } else {
        (24usize, 24usize)
    };
    if command_size < minimum_size {
        return Err(MachoError::CommandTooSmall {
            label: kind.label(),
            size: command_size,
        });
    }
    let string_offset = read_u32(bytes, command_offset + 8)? as usize;
    if string_offset < minimum_offset || string_offset >= command_size {
        return Err(MachoError::InvalidPathOffset {
            label: kind.label(),
            offset: string_offset,
            size: command_size,
        });
    }
    let string_start = command_offset + string_offset;
    let command_end = command_offset + command_size;
    if bytes[command_offset + minimum_offset..string_start]
        .iter()
        .any(|byte| *byte != 0)
    {
        return Err(MachoError::NonZeroBeforePath {
            label: kind.label(),
        });
    }
    let Some(terminator) = bytes[string_start..command_end]
        .iter()
        .position(|byte| *byte == 0)
    else {
        return Err(MachoError::PathNotTerminated {
            label: kind.label(),
        });
    };
    let string_end = string_start + terminator;
    if string_end == string_start {
        return Err(MachoError::EmptyPath {
            label: kind.label(),
        });
    }
    if bytes[string_end + 1..command_end]
        .iter()
        .any(|byte| *byte != 0)
    {
        return Err(MachoError::NonZeroPadding {
            label: kind.label(),
        });
    }
    let raw = &bytes[string_start..string_end];
    if raw.len() > MAX_NAME_BYTES {
        return Err(MachoError::PathTooLong {
            label: kind.label(),
            len: raw.len(),
            max: MAX_NAME_BYTES,
        });
    }
    let name = std::str::from_utf8(raw).map_err(|source| MachoError::PathNotUtf8 {
        label: kind.label(),
        source,
    })?;
    validate_name(name, kind, false)?;
    Ok((name.to_owned(), string_start..string_end))
}

/// Reject path text that cannot be passed safely to the rewriter.
///
/// Control characters, leading `-`, empty strings, and oversized strings are
/// refused for both parsed and mapped names. Mapped names must additionally be
/// free of unresolved markers.
pub(super) fn validate_name(name: &str, kind: Kind, mapped: bool) -> Result<(), MachoError> {
    if name.is_empty() {
        return Err(MachoError::PathIsEmpty {
            label: kind.label(),
        });
    }
    if name.len() > MAX_NAME_BYTES {
        return Err(MachoError::PathTooLong {
            label: kind.label(),
            len: name.len(),
            max: MAX_NAME_BYTES,
        });
    }
    if name.starts_with('-') {
        return Err(MachoError::NameLeadingDash {
            label: kind.label(),
            name: name.to_owned(),
        });
    }
    if name.chars().any(char::is_control) {
        return Err(MachoError::NameControl {
            label: kind.label(),
            name: name.to_owned(),
        });
    }
    if mapped && name.contains(MARKER_TEXT) {
        return Err(MachoError::ReplacementMarker {
            label: kind.label(),
            name: name.to_owned(),
        });
    }
    Ok(())
}

/// Reject command sets that cannot be relocated unambiguously.
pub(super) fn validate_relocatable(parsed: &Parsed) -> Result<(), MachoError> {
    if !matches!(parsed.header.filetype, MH_EXECUTE | MH_DYLIB | MH_BUNDLE) {
        return Err(MachoError::UnsupportedFileType {
            filetype: parsed.header.filetype,
        });
    }

    let mut id_command: Option<usize> = None;
    let mut dylibs: HashMap<&str, usize> = HashMap::new();
    let mut rpaths: HashMap<&str, usize> = HashMap::new();
    for (index, command) in parsed.commands.iter().enumerate() {
        let Some(kind) = command.kind else {
            continue;
        };
        let name = command
            .name
            .as_deref()
            .ok_or(MachoError::CommandMissingPath { index })?;
        match kind {
            Kind::IdDylib => {
                if let Some(previous) = id_command.replace(index) {
                    return Err(MachoError::MultipleIdDylib { previous, index });
                }
                if parsed.header.filetype == MH_EXECUTE {
                    return Err(MachoError::ExecutableIdDylib { index });
                }
            }
            Kind::Rpath => {
                if let Some(previous) = rpaths.insert(name, index) {
                    return Err(MachoError::DuplicateRpath {
                        name: name.to_owned(),
                        previous,
                        index,
                    });
                }
            }
            _ => {
                if let Some(previous) = dylibs.insert(name, index) {
                    return Err(MachoError::DuplicateDylib {
                        name: name.to_owned(),
                        previous,
                        index,
                    });
                }
            }
        }
    }
    Ok(())
}

pub(super) fn read_u32(bytes: &[u8], offset: usize) -> Result<u32, MachoError> {
    let Some(end) = offset.checked_add(4) else {
        return Err(MachoError::FieldOffsetOverflow);
    };
    let raw = bytes
        .get(offset..end)
        .ok_or(MachoError::TruncatedField { offset })?;
    Ok(u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]))
}
