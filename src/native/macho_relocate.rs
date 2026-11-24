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

use std::collections::HashMap;
use std::fs;
use std::ops::Range;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};

/// Size of `mach_header_64`.
const MACH_HEADER_64_BYTES: usize = 32;
/// Size of `fat_header`.
const FAT_HEADER_BYTES: usize = 8;
/// Size of one `fat_arch`.
const FAT_ARCH_32_BYTES: usize = 20;
/// Size of one `fat_arch_64`.
const FAT_ARCH_64_BYTES: usize = 32;
/// Largest plausible number of slices in a FAT image. Real images have a
/// handful; Java class files begin with the same bytes and carry their version
/// where `nfat_arch` would live, so a tight bound also separates the two.
const MAX_FAT_ARCHES: u32 = 32;
/// Largest number of load commands accepted. Real images have far fewer.
const MAX_LOAD_COMMANDS: u32 = 4096;
/// Longest load-command path accepted, in bytes.
const MAX_NAME_BYTES: usize = 8192;
/// Maximum tool diagnostic copied into an error.
const MAX_TOOL_DIAGNOSTIC_BYTES: usize = 4096;

/// Little-endian 64-bit Mach-O magic, as stored in the file.
const MH_MAGIC_64_BYTES: [u8; 4] = [0xcf, 0xfa, 0xed, 0xfe];
/// Big-endian 64-bit Mach-O magic, as stored in the file.
const MH_CIGAM_64_BYTES: [u8; 4] = [0xfe, 0xed, 0xfa, 0xcf];
/// Little-endian 32-bit Mach-O magic, as stored in the file.
const MH_MAGIC_32_BYTES: [u8; 4] = [0xce, 0xfa, 0xed, 0xfe];
/// Big-endian 32-bit Mach-O magic, as stored in the file.
const MH_CIGAM_32_BYTES: [u8; 4] = [0xfe, 0xed, 0xfa, 0xce];
/// FAT magic (`FAT_MAGIC`), as stored in the file.
const FAT_MAGIC_BYTES: [u8; 4] = [0xca, 0xfe, 0xba, 0xbe];
/// 64-bit FAT magic (`FAT_MAGIC_64`), as stored in the file.
const FAT_MAGIC_64_BYTES: [u8; 4] = [0xca, 0xfe, 0xba, 0xbf];
/// Byte-swapped FAT magic (`FAT_CIGAM`), as stored in the file.
const FAT_CIGAM_BYTES: [u8; 4] = [0xbe, 0xba, 0xfe, 0xca];
/// Byte-swapped 64-bit FAT magic (`FAT_CIGAM_64`), as stored in the file.
const FAT_CIGAM_64_BYTES: [u8; 4] = [0xbf, 0xba, 0xfe, 0xca];

/// `CPU_TYPE_X86_64`.
const CPU_TYPE_X86_64: u32 = 0x0100_0007;
/// `CPU_TYPE_ARM64` (`CPU_TYPE_ARM64_32` is a different, unsupported type).
const CPU_TYPE_ARM64: u32 = 0x0100_000c;

/// `MH_EXECUTE`.
const MH_EXECUTE: u32 = 0x2;
/// `MH_DYLIB`.
const MH_DYLIB: u32 = 0x6;
/// `MH_BUNDLE`.
const MH_BUNDLE: u32 = 0x8;

/// `LC_LOAD_DYLIB`.
const LC_LOAD_DYLIB: u32 = 0x0000_000c;
/// `LC_ID_DYLIB`.
const LC_ID_DYLIB: u32 = 0x0000_000d;
/// `LC_LOAD_WEAK_DYLIB` (`LC_REQ_DYLD | 0x18`).
const LC_LOAD_WEAK_DYLIB: u32 = 0x8000_0018;
/// `LC_RPATH` (`LC_REQ_DYLD | 0x1c`).
const LC_RPATH: u32 = 0x8000_001c;
/// `LC_REEXPORT_DYLIB` (`LC_REQ_DYLD | 0x1f`).
const LC_REEXPORT_DYLIB: u32 = 0x8000_001f;
/// `LC_LAZY_LOAD_DYLIB`.
const LC_LAZY_LOAD_DYLIB: u32 = 0x0000_0020;
/// `LC_LOAD_UPWARD_DYLIB` (`LC_REQ_DYLD | 0x23`).
const LC_LOAD_UPWARD_DYLIB: u32 = 0x8000_0023;
/// `LC_CODE_SIGNATURE`.
const LC_CODE_SIGNATURE: u32 = 0x0000_001d;

/// The marker prefix Homebrew uses for every relocatable path.
const MARKER: &[u8] = b"@@HOMEBREW_";
/// The marker prefix as text, for mapped string checks.
const MARKER_TEXT: &str = "@@HOMEBREW_";

/// Absolute path of Apple's install-name rewriter.
const INSTALL_NAME_TOOL: &str = "/usr/bin/install_name_tool";
/// Absolute path of Apple's code-signing tool.
const CODESIGN: &str = "/usr/bin/codesign";

/// Compare the first four bytes of `magic` with a stored magic constant.
fn has_magic(magic: &[u8], expected: [u8; 4]) -> bool {
    magic == expected.as_slice()
}

/// Return whether `bytes` starts like a Mach-O family image.
///
/// Thin 32-bit and 64-bit images (either byte order) are recognized from their
/// magic alone even though [`relocate`] supports only little-endian 64-bit
/// images. A `0xcafebabe` prefix is recognized as FAT only when the header and
/// its architecture table are plausible; a Java class file starts with the
/// same bytes but does not carry a plausible FAT table, so it is not Mach-O.
/// The caller decides how to report recognized-but-unsupported images.
pub(super) fn is_macho(bytes: &[u8]) -> bool {
    let Some(magic) = bytes.get(..4) else {
        return false;
    };
    if [
        MH_MAGIC_64_BYTES,
        MH_CIGAM_64_BYTES,
        MH_MAGIC_32_BYTES,
        MH_CIGAM_32_BYTES,
    ]
    .into_iter()
    .any(|expected| has_magic(magic, expected))
    {
        return true;
    }
    let Some(family) = FatFamily::from_magic(magic) else {
        return false;
    };
    fat_is_plausible(bytes, family)
}

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
    mut map: impl FnMut(PathKind, &str, &[&str]) -> Result<Option<String>>,
) -> Result<()> {
    let parsed = parse(bytes)?;
    validate_relocatable(&parsed)?;
    let plan = plan(&parsed, &mut map)?;
    validate_targets(&parsed, &plan.expected)?;
    check_marker_plan(bytes, &parsed, &plan.expected)?;

    if plan.changes.is_empty() {
        return Ok(());
    }

    ensure_regular_file(path)?;
    let current = fs::read(path)
        .with_context(|| format!("Could not re-read staged image {}", path.display()))?;
    if current != bytes {
        bail!(
            "Staged image {} changed after it was inspected; refusing to rewrite it",
            path.display()
        );
    }

    if !cfg!(target_os = "macos") {
        bail!("Mach-O relocation requires macOS");
    }

    if let Err(error) = apply(path, &parsed, &plan) {
        return match fs::write(path, bytes) {
            Ok(()) => Err(error.context(format!(
                "macOS relocation failed; the original bytes of {} were restored",
                path.display()
            ))),
            Err(restore_error) => Err(error.context(format!(
                "macOS relocation failed and the original bytes of {} could not be restored: {restore_error:#}",
                path.display()
            ))),
        };
    }
    Ok(())
}

/// A byte-order/slice-width variant of the FAT container header.
#[derive(Clone, Copy)]
enum FatFamily {
    Fat32Big,
    Fat32Little,
    Fat64Big,
    Fat64Little,
}

impl FatFamily {
    fn from_magic(magic: &[u8]) -> Option<Self> {
        if has_magic(magic, FAT_MAGIC_BYTES) {
            Some(Self::Fat32Big)
        } else if has_magic(magic, FAT_CIGAM_BYTES) {
            Some(Self::Fat32Little)
        } else if has_magic(magic, FAT_MAGIC_64_BYTES) {
            Some(Self::Fat64Big)
        } else if has_magic(magic, FAT_CIGAM_64_BYTES) {
            Some(Self::Fat64Little)
        } else {
            None
        }
    }

    fn is_64(self) -> bool {
        matches!(self, Self::Fat64Big | Self::Fat64Little)
    }

    fn is_big_endian(self) -> bool {
        matches!(self, Self::Fat32Big | Self::Fat64Big)
    }

    fn entry_size(self) -> usize {
        if self.is_64() {
            FAT_ARCH_64_BYTES
        } else {
            FAT_ARCH_32_BYTES
        }
    }
}

/// Validate enough of a FAT header to distinguish it from a Java class file.
fn fat_is_plausible(bytes: &[u8], family: FatFamily) -> bool {
    let big_endian = family.is_big_endian();
    let Some(arch_count) = read_fat_u32(bytes, 4, big_endian) else {
        return false;
    };
    if arch_count == 0 || arch_count > MAX_FAT_ARCHES {
        return false;
    }
    let arch_count = arch_count as usize;
    let Some(table_bytes) = arch_count.checked_mul(family.entry_size()) else {
        return false;
    };
    let Some(table_end) = FAT_HEADER_BYTES.checked_add(table_bytes) else {
        return false;
    };
    if table_end > bytes.len() {
        return false;
    }

    let mut slices: Vec<(u64, u64)> = Vec::with_capacity(arch_count);
    for index in 0..arch_count {
        let base = FAT_HEADER_BYTES + index * family.entry_size();
        let Some(cputype) = read_fat_u32(bytes, base, big_endian) else {
            return false;
        };
        if !is_plausible_fat_cputype(cputype) {
            return false;
        }
        let (offset, size, align) = if family.is_64() {
            let Some(offset) = read_fat_u64(bytes, base + 8, big_endian) else {
                return false;
            };
            let Some(size) = read_fat_u64(bytes, base + 16, big_endian) else {
                return false;
            };
            let Some(align) = read_fat_u32(bytes, base + 24, big_endian) else {
                return false;
            };
            (offset, size, align)
        } else {
            let Some(offset) = read_fat_u32(bytes, base + 8, big_endian) else {
                return false;
            };
            let Some(size) = read_fat_u32(bytes, base + 12, big_endian) else {
                return false;
            };
            let Some(align) = read_fat_u32(bytes, base + 16, big_endian) else {
                return false;
            };
            (u64::from(offset), u64::from(size), align)
        };
        if size == 0 || align > 15 {
            return false;
        }
        let Some(end) = offset.checked_add(size) else {
            return false;
        };
        if offset < table_end as u64 || end > bytes.len() as u64 {
            return false;
        }
        if offset % (1u64 << align) != 0 {
            return false;
        }
        slices.push((offset, end));
    }
    slices.sort_unstable();
    if slices.windows(2).any(|pair| pair[0].1 > pair[1].0) {
        return false;
    }
    true
}

/// Only CPU types that appear in real Mach-O FAT images are accepted.
fn is_plausible_fat_cputype(cputype: u32) -> bool {
    matches!(
        cputype,
        7 // CPU_TYPE_X86 / CPU_TYPE_I386
            | 11 // CPU_TYPE_MC88000
            | 12 // CPU_TYPE_ARM
            | 13 // CPU_TYPE_MC680x0
            | 14 // CPU_TYPE_SPARC
            | 15 // CPU_TYPE_I860
            | 18 // CPU_TYPE_POWERPC
            | CPU_TYPE_X86_64
            | CPU_TYPE_ARM64
            | 0x0200_000c // CPU_TYPE_ARM64_32
            | 0x0100_0012 // CPU_TYPE_POWERPC64
    )
}

fn read_fat_u32(bytes: &[u8], offset: usize, big_endian: bool) -> Option<u32> {
    let end = offset.checked_add(4)?;
    let raw = bytes.get(offset..end)?;
    Some(if big_endian {
        u32::from_be_bytes([raw[0], raw[1], raw[2], raw[3]])
    } else {
        u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]])
    })
}

fn read_fat_u64(bytes: &[u8], offset: usize, big_endian: bool) -> Option<u64> {
    let end = offset.checked_add(8)?;
    let raw = bytes.get(offset..end)?;
    let array = [
        raw[0], raw[1], raw[2], raw[3], raw[4], raw[5], raw[6], raw[7],
    ];
    Some(if big_endian {
        u64::from_be_bytes(array)
    } else {
        u64::from_le_bytes(array)
    })
}

/// Header fields that relocation must preserve exactly.
#[derive(Debug, Clone, Copy)]
struct Header {
    cputype: u32,
    cpusubtype: u32,
    filetype: u32,
}

/// Policy category of a Mach-O load-command path passed to the mapper.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) enum PathKind {
    Dylib,
    IdDylib,
    Rpath,
}

/// A load command recognized as carrying a relocatable path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    LoadDylib,
    WeakDylib,
    ReexportDylib,
    UpwardDylib,
    LazyDylib,
    IdDylib,
    Rpath,
}

impl Kind {
    fn from_command(command: u32) -> Option<Self> {
        match command {
            LC_LOAD_DYLIB => Some(Self::LoadDylib),
            LC_LOAD_WEAK_DYLIB => Some(Self::WeakDylib),
            LC_REEXPORT_DYLIB => Some(Self::ReexportDylib),
            LC_LOAD_UPWARD_DYLIB => Some(Self::UpwardDylib),
            LC_LAZY_LOAD_DYLIB => Some(Self::LazyDylib),
            LC_ID_DYLIB => Some(Self::IdDylib),
            LC_RPATH => Some(Self::Rpath),
            _ => None,
        }
    }

    fn path_kind(self) -> PathKind {
        match self {
            Self::IdDylib => PathKind::IdDylib,
            Self::Rpath => PathKind::Rpath,
            Self::LoadDylib
            | Self::WeakDylib
            | Self::ReexportDylib
            | Self::UpwardDylib
            | Self::LazyDylib => PathKind::Dylib,
        }
    }

    fn is_rpath(self) -> bool {
        matches!(self, Self::Rpath)
    }

    fn label(self) -> &'static str {
        match self {
            Self::LoadDylib => "LC_LOAD_DYLIB",
            Self::WeakDylib => "LC_LOAD_WEAK_DYLIB",
            Self::ReexportDylib => "LC_REEXPORT_DYLIB",
            Self::UpwardDylib => "LC_LOAD_UPWARD_DYLIB",
            Self::LazyDylib => "LC_LAZY_LOAD_DYLIB",
            Self::IdDylib => "LC_ID_DYLIB",
            Self::Rpath => "LC_RPATH",
        }
    }
}

/// One parsed load command.
#[derive(Debug)]
struct LoadCommand {
    command: u32,
    kind: Option<Kind>,
    name: Option<String>,
    name_range: Option<Range<usize>>,
}

/// A structurally validated thin 64-bit Mach-O image.
#[derive(Debug)]
struct Parsed {
    header: Header,
    commands: Vec<LoadCommand>,
    /// Whether the image arrived with a non-empty embedded code signature.
    signed: bool,
}

/// Parse and bound-check the header and every load command in `bytes`.
fn parse(bytes: &[u8]) -> Result<Parsed> {
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
fn validate_name(name: &str, kind: Kind, mapped: bool) -> Result<()> {
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
fn validate_relocatable(parsed: &Parsed) -> Result<()> {
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

/// One planned `install_name_tool` edit.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Change {
    kind: Kind,
    old: String,
    new: String,
}

/// The complete edit plan, including the expected value of every path-bearing
/// command (indexed like [`Parsed::commands`]).
#[derive(Debug)]
struct Plan {
    changes: Vec<Change>,
    expected: Vec<Option<String>>,
}

impl Plan {
    fn arguments(&self) -> Vec<String> {
        let mut arguments = Vec::with_capacity(self.changes.len().saturating_mul(3));
        for change in &self.changes {
            match change.kind {
                Kind::IdDylib => {
                    arguments.push("-id".to_owned());
                    arguments.push(change.new.clone());
                }
                Kind::Rpath => {
                    arguments.push("-rpath".to_owned());
                    arguments.push(change.old.clone());
                    arguments.push(change.new.clone());
                }
                _ => {
                    arguments.push("-change".to_owned());
                    arguments.push(change.old.clone());
                    arguments.push(change.new.clone());
                }
            }
        }
        arguments
    }
}

/// Call the mapper for every path-bearing command and build the edit plan.
fn plan(
    parsed: &Parsed,
    map: &mut impl FnMut(PathKind, &str, &[&str]) -> Result<Option<String>>,
) -> Result<Plan> {
    let rpaths: Vec<&str> = parsed
        .commands
        .iter()
        .filter(|command| command.kind == Some(Kind::Rpath))
        .filter_map(|command| command.name.as_deref())
        .collect();
    let mut mapped: HashMap<(PathKind, String), Option<String>> = HashMap::new();
    let mut expected = vec![None; parsed.commands.len()];
    let mut changes = Vec::new();

    for (index, command) in parsed.commands.iter().enumerate() {
        let Some(kind) = command.kind else {
            continue;
        };
        let old = command
            .name
            .as_deref()
            .with_context(|| format!("Load command {index} is missing its path"))?;
        let path_kind = kind.path_kind();
        let mapped_value = map(path_kind, old, &rpaths)
            .with_context(|| format!("Could not map {} path {old:?}", kind.label()))?;
        if let Some(new) = &mapped_value {
            validate_name(new, kind, true)?;
        }
        let key = (path_kind, old.to_owned());
        if let Some(previous) = mapped.get(&key) {
            if previous != &mapped_value {
                bail!(
                    "Path mapper returned inconsistent results for {old:?}: {previous:?} and {mapped_value:?}"
                );
            }
        } else {
            mapped.insert(key, mapped_value.clone());
        }
        let target = mapped_value.unwrap_or_else(|| old.to_owned());
        if target.contains(MARKER_TEXT) {
            bail!(
                "{} path {target:?} still contains a Homebrew placeholder; the mapper must resolve it",
                kind.label()
            );
        }
        if target != old {
            changes.push(Change {
                kind,
                old: old.to_owned(),
                new: target.clone(),
            });
        }
        expected[index] = Some(target);
    }

    Ok(Plan { changes, expected })
}

/// Reject plans that collapse distinct commands onto one path.
fn validate_targets(parsed: &Parsed, expected: &[Option<String>]) -> Result<()> {
    let mut dylibs: HashMap<&str, usize> = HashMap::new();
    let mut rpaths: HashMap<&str, usize> = HashMap::new();
    for (index, command) in parsed.commands.iter().enumerate() {
        let Some(kind) = command.kind else {
            continue;
        };
        let target = expected
            .get(index)
            .and_then(Option::as_deref)
            .with_context(|| format!("No relocation target planned for load command {index}"))?;
        let table = if kind.is_rpath() {
            &mut rpaths
        } else if kind == Kind::IdDylib {
            continue;
        } else {
            &mut dylibs
        };
        if let Some(previous) = table.insert(target, index) {
            bail!(
                "Relocation would make load commands {previous} and {index} share the path {target:?}"
            );
        }
    }
    Ok(())
}

/// Require every raw marker to sit in a supported command path and be resolved
/// by the plan. This runs before anything is mutated.
fn check_marker_plan(bytes: &[u8], parsed: &Parsed, expected: &[Option<String>]) -> Result<()> {
    let mut ranges: Vec<Range<usize>> = parsed
        .commands
        .iter()
        .filter_map(|command| command.name_range.clone())
        .collect();
    ranges.sort_unstable_by_key(|range| range.start);

    let mut range_index = 0usize;
    scan_markers(bytes, |offset| {
        while range_index < ranges.len() && ranges[range_index].end <= offset {
            range_index += 1;
        }
        let contained = range_index < ranges.len()
            && ranges[range_index].start <= offset
            && offset + MARKER.len() <= ranges[range_index].end;
        if !contained {
            bail!(
                "Raw Homebrew placeholder at file offset {offset} is outside a supported Mach-O load command; refusing to guess at its meaning"
            );
        }
        Ok(())
    })?;

    for (index, target) in expected.iter().enumerate() {
        if let Some(target) = target
            && target.contains(MARKER_TEXT)
        {
            bail!("Homebrew placeholder in load command {index} was not resolved ({target:?})");
        }
    }
    Ok(())
}

/// Fail if any raw marker remains anywhere in `bytes`.
fn reject_any_marker(bytes: &[u8]) -> Result<()> {
    scan_markers(bytes, |offset| {
        bail!("Rewritten image still contains a raw Homebrew placeholder at file offset {offset}")
    })
}

/// Visit the offset of every non-overlapping-width occurrence of [`MARKER`],
/// scanning the image once.
fn scan_markers(bytes: &[u8], mut visit: impl FnMut(usize) -> Result<()>) -> Result<()> {
    let mut search_from = 0usize;
    while let Some(tail) = bytes.get(search_from..) {
        let Some(relative) = tail
            .windows(MARKER.len())
            .position(|window| window == MARKER)
        else {
            break;
        };
        let offset = search_from + relative;
        visit(offset)?;
        let Some(next) = offset.checked_add(1) else {
            break;
        };
        search_from = next;
    }
    Ok(())
}

/// Run the Apple rewriter, sign, and verify, re-parsing after every mutation.
fn apply(path: &Path, original: &Parsed, plan: &Plan) -> Result<()> {
    let arguments = plan.arguments();
    run_tool(
        INSTALL_NAME_TOOL,
        &arguments,
        path,
        "install_name_tool could not rewrite the staged image",
    )?;

    let rewritten = fs::read(path)
        .with_context(|| format!("Could not read rewritten image {}", path.display()))?;
    verify_stage(original, plan, &rewritten)
        .context("The image did not match the relocation plan after install_name_tool")?;

    let sign_arguments = signing_arguments(original.signed);
    run_tool(
        CODESIGN,
        &sign_arguments,
        path,
        "codesign could not ad-hoc sign the staged image",
    )?;

    let signed = fs::read(path)
        .with_context(|| format!("Could not read signed image {}", path.display()))?;
    verify_stage(original, plan, &signed)
        .context("The image did not match the relocation plan after codesign")?;

    let verify_arguments = ["--verify".to_owned(), "--strict".to_owned()];
    run_tool(
        CODESIGN,
        &verify_arguments,
        path,
        "codesign could not verify the staged image",
    )?;

    let verified = fs::read(path)
        .with_context(|| format!("Could not read verified image {}", path.display()))?;
    verify_stage(original, plan, &verified)
        .context("The image did not match the relocation plan after signature verification")?;
    Ok(())
}

/// Parse the rewritten bytes and check them against the original identity and
/// the exact plan.
fn verify_stage(original: &Parsed, plan: &Plan, bytes: &[u8]) -> Result<()> {
    reject_any_marker(bytes)?;
    let parsed =
        parse(bytes).context("Rewritten file is not a supported thin 64-bit Mach-O image")?;
    validate_relocatable(&parsed).context("Rewritten load-command set is not relocatable")?;
    compare_rewrite(original, &plan.expected, &parsed)
}

/// Compare the non-signature load commands of a rewritten image with the plan.
///
/// `codesign` may add, replace, or remove `LC_CODE_SIGNATURE`; every other
/// command must keep its type, position relative to other non-signature
/// commands, and (for path-bearing commands) its planned value. No command may
/// appear that the original image did not have.
fn compare_rewrite(original: &Parsed, expected: &[Option<String>], after: &Parsed) -> Result<()> {
    if after.header.cputype != original.header.cputype
        || after.header.cpusubtype != original.header.cpusubtype
        || after.header.filetype != original.header.filetype
    {
        bail!("Rewritten Mach-O header identity changed");
    }

    let mut rewritten = after
        .commands
        .iter()
        .filter(|command| command.command != LC_CODE_SIGNATURE);
    for (index, command) in original.commands.iter().enumerate() {
        if command.command == LC_CODE_SIGNATURE {
            continue;
        }
        let Some(actual) = rewritten.next() else {
            bail!("Load command {index} disappeared from the rewritten image");
        };
        if actual.command != command.command {
            bail!(
                "Load command {index} changed type from {:#x} to {:#x}",
                command.command,
                actual.command
            );
        }
        match (&expected[index], actual.name.as_deref()) {
            (Some(target), Some(value)) if target == value => {}
            (Some(target), Some(value)) => {
                bail!(
                    "Load command {index} expected path {target:?}, but the rewritten image contains {value:?}"
                );
            }
            (Some(_), None) => {
                bail!("Load command {index} lost its path in the rewritten image");
            }
            (None, Some(value)) => {
                bail!(
                    "Load command {index} unexpectedly gained the path {value:?} in the rewritten image"
                );
            }
            (None, None) => {}
        }
    }
    if let Some(extra) = rewritten.next() {
        bail!(
            "Rewritten image gained an unexpected load command {:#x}",
            extra.command
        );
    }
    Ok(())
}

/// Build the ad-hoc signing arguments for a rebuilt image.
///
/// Only an image that arrived with a real signature can preserve metadata; an
/// unsigned image has nothing to preserve and is signed plainly.
fn signing_arguments(originally_signed: bool) -> Vec<String> {
    let mut arguments = vec!["--force".to_owned(), "--sign".to_owned(), "-".to_owned()];
    if originally_signed {
        arguments.push("--preserve-metadata=entitlements,requirements,flags,runtime".to_owned());
    }
    arguments
}

/// Run one absolute Apple tool with `arguments` followed by `path`.
fn run_tool(program: &str, arguments: &[String], path: &Path, context: &str) -> Result<()> {
    let output = Command::new(program)
        .args(arguments)
        .arg(path)
        .output()
        .with_context(|| format!("Could not run {program}"))?;
    if output.status.success() {
        return Ok(());
    }
    let mut diagnostics: Vec<String> = Vec::new();
    if let Some(stdout) = diagnostic(&output.stdout) {
        diagnostics.push(format!("stdout: {stdout}"));
    }
    if let Some(stderr) = diagnostic(&output.stderr) {
        diagnostics.push(format!("stderr: {stderr}"));
    }
    let detail = if diagnostics.is_empty() {
        "no diagnostic output".to_owned()
    } else {
        diagnostics.join("; ")
    };
    bail!("{context} ({}): {detail}", output.status)
}

/// Render a bounded, lossy version of one tool stream.
fn diagnostic(bytes: &[u8]) -> Option<String> {
    let end = bytes.len().min(MAX_TOOL_DIAGNOSTIC_BYTES);
    let text = String::from_utf8_lossy(&bytes[..end]);
    let text = text.trim();
    if text.is_empty() {
        None
    } else {
        Some(text.to_owned())
    }
}

/// Require a regular, non-symlink staged file.
fn ensure_regular_file(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("Could not inspect staged image {}", path.display()))?;
    if !metadata.file_type().is_file() {
        bail!("Staged image {} is not a regular file", path.display());
    }
    Ok(())
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32> {
    let Some(end) = offset.checked_add(4) else {
        bail!("Mach-O field offset overflows");
    };
    let raw = bytes
        .get(offset..end)
        .with_context(|| format!("Truncated Mach-O field at offset {offset}"))?;
    Ok(u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]))
}

#[cfg(test)]
mod tests;
