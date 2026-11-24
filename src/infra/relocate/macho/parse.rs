//! Bounded parsing and validation of thin 64-bit Mach-O load commands.

use std::collections::HashMap;
use std::ops::Range;

use anyhow::{Context, Result, bail};

use super::constants::*;
use super::fat::{FatFamily, has_magic};
use super::model::{Header, Kind, LoadCommand, Parsed};

/// Size of `mach_header_64`.
pub(super) const MACH_HEADER_64_BYTES: usize = 32;

/// Largest number of load commands accepted. Real images have far fewer.
pub(super) const MAX_LOAD_COMMANDS: u32 = 4096;
/// Longest load-command path accepted, in bytes.
const MAX_NAME_BYTES: usize = 8192;

/// Parse and bound-check the header and every load command in `bytes`.
pub(super) fn parse(bytes: &[u8]) -> Result<Parsed> {
    let Some(magic) = bytes.get(..4) else {
        bail!("File is too small to be a Mach-O image");
    };
    if FatFamily::from_magic(magic).is_some() {
        bail!("FAT Mach-O images are unsupported; only thin 64-bit images can be relocated");
    }
    if !has_magic(magic, MH_MAGIC_64_BYTES) {
        if has_magic(magic, MH_CIGAM_64_BYTES) {
            bail!("Big-endian 64-bit Mach-O images are unsupported");
        }
        if has_magic(magic, MH_MAGIC_32_BYTES) || has_magic(magic, MH_CIGAM_32_BYTES) {
            bail!("32-bit Mach-O images are unsupported");
        }
        bail!("File is not a little-endian 64-bit Mach-O image");
    }
    if bytes.len() < MACH_HEADER_64_BYTES {
        bail!("Truncated 64-bit Mach-O header");
    }

    let cputype = read_u32(bytes, 4)?;
    let cpusubtype = read_u32(bytes, 8)?;
    let filetype = read_u32(bytes, 12)?;
    let command_count = read_u32(bytes, 16)?;
    let command_bytes = read_u32(bytes, 20)? as usize;

    if !matches!(cputype, CPU_TYPE_ARM64 | CPU_TYPE_X86_64) {
        bail!("Unsupported Mach-O CPU type {cputype:#x}; expected arm64 or x86_64");
    }
    if command_count == 0 || command_count > MAX_LOAD_COMMANDS {
        bail!("Unsupported Mach-O load-command count {command_count}");
    }
    let command_count = command_count as usize;
    let Some(commands_end) = MACH_HEADER_64_BYTES.checked_add(command_bytes) else {
        bail!("Mach-O load-command table size overflows");
    };
    if commands_end > bytes.len() {
        bail!(
            "Mach-O load-command table ({} bytes) extends past the end of the file",
            command_bytes
        );
    }
    if command_bytes < command_count * 8 {
        bail!("Mach-O load-command table is too small for {command_count} commands");
    }

    let mut commands = Vec::with_capacity(command_count);
    let mut offset = MACH_HEADER_64_BYTES;
    let mut signature_commands = 0usize;
    let mut signature_data_size = 0usize;
    for index in 0..command_count {
        let Some(command_end_probe) = offset.checked_add(8) else {
            bail!("Mach-O load command {index} offset overflows");
        };
        if command_end_probe > commands_end {
            bail!("Truncated Mach-O load command {index}");
        }
        let command = read_u32(bytes, offset)?;
        let command_size = read_u32(bytes, offset + 4)? as usize;
        if command_size < 8 || !command_size.is_multiple_of(8) {
            bail!(
                "Mach-O load command {index} has invalid size {command_size}; 64-bit commands are 8-byte aligned"
            );
        }
        let Some(command_end) = offset.checked_add(command_size) else {
            bail!("Mach-O load command {index} size overflows");
        };
        if command_end > commands_end {
            bail!("Mach-O load command {index} extends past the load-command table");
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
                bail!("Mach-O image contains more than one LC_CODE_SIGNATURE command");
            }
            if command_size != 16 {
                bail!("LC_CODE_SIGNATURE has unexpected size {command_size}");
            }
            let data_offset = read_u32(bytes, offset + 8)? as usize;
            let data_size = read_u32(bytes, offset + 12)? as usize;
            signature_data_size = data_size;
            if data_size > 0 {
                let Some(data_end) = data_offset.checked_add(data_size) else {
                    bail!("LC_CODE_SIGNATURE data range overflows");
                };
                if data_offset < commands_end || data_end > bytes.len() {
                    bail!("LC_CODE_SIGNATURE data lies outside the image");
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
        bail!(
            "Mach-O load-command sizes sum to {} bytes, but the header declares {}",
            offset - MACH_HEADER_64_BYTES,
            command_bytes
        );
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
) -> Result<(String, Range<usize>)> {
    let (minimum_offset, minimum_size) = if kind.is_rpath() {
        (12usize, 12usize)
    } else {
        (24usize, 24usize)
    };
    if command_size < minimum_size {
        bail!(
            "{} command is too small ({command_size} bytes)",
            kind.label()
        );
    }
    let string_offset = read_u32(bytes, command_offset + 8)? as usize;
    if string_offset < minimum_offset || string_offset >= command_size {
        bail!(
            "{} has invalid path offset {string_offset} in a {command_size}-byte command",
            kind.label()
        );
    }
    let string_start = command_offset + string_offset;
    let command_end = command_offset + command_size;
    if bytes[command_offset + minimum_offset..string_start]
        .iter()
        .any(|byte| *byte != 0)
    {
        bail!(
            "{} has non-zero bytes between its struct and path",
            kind.label()
        );
    }
    let Some(terminator) = bytes[string_start..command_end]
        .iter()
        .position(|byte| *byte == 0)
    else {
        bail!("{} path is not NUL-terminated", kind.label());
    };
    let string_end = string_start + terminator;
    if string_end == string_start {
        bail!("{} has an empty path", kind.label());
    }
    if bytes[string_end + 1..command_end]
        .iter()
        .any(|byte| *byte != 0)
    {
        bail!("{} path is followed by non-zero padding", kind.label());
    }
    let raw = &bytes[string_start..string_end];
    if raw.len() > MAX_NAME_BYTES {
        bail!(
            "{} path is {} bytes, over the {MAX_NAME_BYTES}-byte limit",
            kind.label(),
            raw.len()
        );
    }
    let name = std::str::from_utf8(raw)
        .with_context(|| format!("{} path is not valid UTF-8", kind.label()))?;
    validate_name(name, kind, false)?;
    Ok((name.to_owned(), string_start..string_end))
}

/// Reject path text that cannot be passed safely to the rewriter.
///
/// Control characters, leading `-`, empty strings, and oversized strings are
/// refused for both parsed and mapped names. Mapped names must additionally be
/// free of unresolved markers.
pub(super) fn validate_name(name: &str, kind: Kind, mapped: bool) -> Result<()> {
    if name.is_empty() {
        bail!("{} path is empty", kind.label());
    }
    if name.len() > MAX_NAME_BYTES {
        bail!(
            "{} path is {} bytes, over the {MAX_NAME_BYTES}-byte limit",
            kind.label(),
            name.len()
        );
    }
    if name.starts_with('-') {
        bail!(
            "{} path {name:?} starts with '-' and cannot be passed to install_name_tool",
            kind.label()
        );
    }
    if name.chars().any(char::is_control) {
        bail!(
            "{} path {name:?} contains a control character",
            kind.label()
        );
    }
    if mapped && name.contains(MARKER_TEXT) {
        bail!(
            "{} replacement path {name:?} still contains a Homebrew placeholder",
            kind.label()
        );
    }
    Ok(())
}

/// Reject command sets that cannot be relocated unambiguously.
pub(super) fn validate_relocatable(parsed: &Parsed) -> Result<()> {
    if !matches!(parsed.header.filetype, MH_EXECUTE | MH_DYLIB | MH_BUNDLE) {
        bail!(
            "Unsupported Mach-O file type {:#x}; only executables, dylibs, and bundles are supported",
            parsed.header.filetype
        );
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
            .with_context(|| format!("Load command {index} is missing its path"))?;
        match kind {
            Kind::IdDylib => {
                if let Some(previous) = id_command.replace(index) {
                    bail!(
                        "Mach-O image has more than one LC_ID_DYLIB (commands {previous} and {index})"
                    );
                }
                if parsed.header.filetype == MH_EXECUTE {
                    bail!("Mach-O executable unexpectedly declares LC_ID_DYLIB at command {index}");
                }
            }
            Kind::Rpath => {
                if let Some(previous) = rpaths.insert(name, index) {
                    bail!("Duplicate LC_RPATH path {name:?} at commands {previous} and {index}");
                }
            }
            _ => {
                if let Some(previous) = dylibs.insert(name, index) {
                    bail!(
                        "Duplicate dependent dylib path {name:?} at commands {previous} and {index}"
                    );
                }
            }
        }
    }
    Ok(())
}

pub(super) fn read_u32(bytes: &[u8], offset: usize) -> Result<u32> {
    let Some(end) = offset.checked_add(4) else {
        bail!("Mach-O field offset overflows");
    };
    let raw = bytes
        .get(offset..end)
        .with_context(|| format!("Truncated Mach-O field at offset {offset}"))?;
    Ok(u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]))
}
