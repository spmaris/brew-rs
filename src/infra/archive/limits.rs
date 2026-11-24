//! Size, count and depth budgets for bottle extraction.

/// Largest compressed archive that will be hashed or decompressed.
pub(super) const MAX_ARCHIVE_BYTES: u64 = 4 * 1024 * 1024 * 1024;
/// Largest single extracted file.
pub(super) const MAX_FILE_BYTES: u64 = 4 * 1024 * 1024 * 1024;
/// Largest total expanded archive, including tar framing.
pub(super) const MAX_DECOMPRESSED_BYTES: u64 = 8 * 1024 * 1024 * 1024;
/// Most entries accepted.
pub(super) const MAX_ENTRIES: usize = 200_000;
/// Deepest path below `<name>/<version>/` accepted.
pub(super) const MAX_DEPTH: usize = 64;
/// Longest individual path component accepted.
pub(super) const MAX_COMPONENT_BYTES: usize = 255;
/// Longest symlink target string accepted, in bytes.
pub(super) const MAX_LINK_TARGET_BYTES: usize = 1024;
/// Most components accepted in one symlink target, including `..`.
pub(super) const MAX_LINK_COMPONENTS: usize = 2 * MAX_DEPTH;
/// Most symlinks accepted in one bottle.
pub(super) const MAX_SYMLINKS: usize = 20_000;
/// Maximum number of symlink hops allowed when resolving a staged symlink target.
pub(super) const MAX_SYMLINK_HOPS: usize = 8;
/// Buffer used for hashing.
pub(super) const IO_BUFFER_BYTES: usize = 64 * 1024;
