//! Recognition of Mach-O family images, including FAT container plausibility.

use super::constants::{CPU_TYPE_ARM64, CPU_TYPE_X86_64};
use super::constants::{
    MH_CIGAM_32_BYTES, MH_CIGAM_64_BYTES, MH_MAGIC_32_BYTES, MH_MAGIC_64_BYTES,
};

/// Size of `fat_header`.
pub(super) const FAT_HEADER_BYTES: usize = 8;
/// Size of one `fat_arch`.
pub(super) const FAT_ARCH_32_BYTES: usize = 20;
/// Size of one `fat_arch_64`.
pub(super) const FAT_ARCH_64_BYTES: usize = 32;
/// Largest plausible number of slices in a FAT image. Real images have a
/// handful; Java class files begin with the same bytes and carry their version
/// where `nfat_arch` would live, so a tight bound also separates the two.
pub(super) const MAX_FAT_ARCHES: u32 = 32;

/// FAT magic (`FAT_MAGIC`), as stored in the file.
pub(super) const FAT_MAGIC_BYTES: [u8; 4] = [0xca, 0xfe, 0xba, 0xbe];
/// 64-bit FAT magic (`FAT_MAGIC_64`), as stored in the file.
pub(super) const FAT_MAGIC_64_BYTES: [u8; 4] = [0xca, 0xfe, 0xba, 0xbf];
/// Byte-swapped FAT magic (`FAT_CIGAM`), as stored in the file.
pub(super) const FAT_CIGAM_BYTES: [u8; 4] = [0xbe, 0xba, 0xfe, 0xca];
/// Byte-swapped 64-bit FAT magic (`FAT_CIGAM_64`), as stored in the file.
pub(super) const FAT_CIGAM_64_BYTES: [u8; 4] = [0xbf, 0xba, 0xfe, 0xca];

/// Compare the first four bytes of `magic` with a stored magic constant.
pub(super) fn has_magic(magic: &[u8], expected: [u8; 4]) -> bool {
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
#[cfg(test)]
pub(in crate::infra::relocate) fn is_macho(bytes: &[u8]) -> bool {
    is_macho_prefix(bytes, bytes.len() as u64)
}

/// Longest prefix [`is_macho_prefix`] may need: the FAT header plus the
/// largest plausible architecture table fits well within it.
pub(in crate::infra::relocate) const MACHO_PREFIX_BYTES: usize = 4096;

/// [`is_macho`] for a file whose first bytes are `prefix` and whose total size
/// is `file_len`. `prefix` must be the whole file or at least
/// [`MACHO_PREFIX_BYTES`] bytes; the answer is then identical to [`is_macho`]
/// on the full contents.
pub(in crate::infra::relocate) fn is_macho_prefix(prefix: &[u8], file_len: u64) -> bool {
    let bytes = prefix;
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
    fat_is_plausible(bytes, file_len, family)
}

/// A byte-order/slice-width variant of the FAT container header.
#[derive(Clone, Copy)]
pub(super) enum FatFamily {
    Fat32Big,
    Fat32Little,
    Fat64Big,
    Fat64Little,
}

impl FatFamily {
    pub(super) fn from_magic(magic: &[u8]) -> Option<Self> {
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
fn fat_is_plausible(bytes: &[u8], file_len: u64, family: FatFamily) -> bool {
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
        if offset < table_end as u64 || end > file_len {
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
