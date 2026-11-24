//! Size limits shared by every extractor and the tree validator.

/// Largest single extracted file or naked payload (4 GiB).
pub(super) const MAX_ARCHIVE_BYTES: u64 = 4 * 1024 * 1024 * 1024;
/// Largest total uncompressed bytes allowed for an unpacked cask (8 GiB).
pub(super) const MAX_UNPACKED_BYTES: u64 = 8 * 1024 * 1024 * 1024;
/// Maximum archive/tree entries allowed for one cask.
pub(super) const MAX_ENTRIES: usize = 65_536;
