//! Mach-O format constants and the Homebrew marker prefix.

/// Little-endian 64-bit Mach-O magic, as stored in the file.
pub(super) const MH_MAGIC_64_BYTES: [u8; 4] = [0xcf, 0xfa, 0xed, 0xfe];
/// Big-endian 64-bit Mach-O magic, as stored in the file.
pub(super) const MH_CIGAM_64_BYTES: [u8; 4] = [0xfe, 0xed, 0xfa, 0xcf];
/// Little-endian 32-bit Mach-O magic, as stored in the file.
pub(super) const MH_MAGIC_32_BYTES: [u8; 4] = [0xce, 0xfa, 0xed, 0xfe];
/// Big-endian 32-bit Mach-O magic, as stored in the file.
pub(super) const MH_CIGAM_32_BYTES: [u8; 4] = [0xfe, 0xed, 0xfa, 0xce];

/// `CPU_TYPE_X86_64`.
pub(super) const CPU_TYPE_X86_64: u32 = 0x0100_0007;
/// `CPU_TYPE_ARM64` (`CPU_TYPE_ARM64_32` is a different, unsupported type).
pub(super) const CPU_TYPE_ARM64: u32 = 0x0100_000c;

/// `MH_EXECUTE`.
pub(super) const MH_EXECUTE: u32 = 0x2;
/// `MH_DYLIB`.
pub(super) const MH_DYLIB: u32 = 0x6;
/// `MH_BUNDLE`.
pub(super) const MH_BUNDLE: u32 = 0x8;

/// `LC_LOAD_DYLIB`.
pub(super) const LC_LOAD_DYLIB: u32 = 0x0000_000c;
/// `LC_ID_DYLIB`.
pub(super) const LC_ID_DYLIB: u32 = 0x0000_000d;
/// `LC_LOAD_WEAK_DYLIB` (`LC_REQ_DYLD | 0x18`).
pub(super) const LC_LOAD_WEAK_DYLIB: u32 = 0x8000_0018;
/// `LC_RPATH` (`LC_REQ_DYLD | 0x1c`).
pub(super) const LC_RPATH: u32 = 0x8000_001c;
/// `LC_REEXPORT_DYLIB` (`LC_REQ_DYLD | 0x1f`).
pub(super) const LC_REEXPORT_DYLIB: u32 = 0x8000_001f;
/// `LC_LAZY_LOAD_DYLIB`.
pub(super) const LC_LAZY_LOAD_DYLIB: u32 = 0x0000_0020;
/// `LC_LOAD_UPWARD_DYLIB` (`LC_REQ_DYLD | 0x23`).
pub(super) const LC_LOAD_UPWARD_DYLIB: u32 = 0x8000_0023;
/// `LC_CODE_SIGNATURE`.
pub(super) const LC_CODE_SIGNATURE: u32 = 0x0000_001d;

/// The marker prefix Homebrew uses for every relocatable path.
pub(super) const MARKER: &[u8] = b"@@HOMEBREW_";
/// The marker prefix as text, for mapped string checks.
pub(super) const MARKER_TEXT: &str = "@@HOMEBREW_";
