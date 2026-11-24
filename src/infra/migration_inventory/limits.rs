/// Longest formula, cask, version, tap component or service label accepted.
pub(super) const MAX_NAME_LEN: usize = 128;
/// Most entries examined in one directory.
pub(super) const MAX_DIRECTORY_ENTRIES: usize = 4_096;
/// Most formulae inventoried.
pub(super) const MAX_FORMULAE: usize = 4_096;
/// Most installed versions kept for one formula or cask.
pub(super) const MAX_VERSIONS_PER_PACKAGE: usize = 64;
/// Most casks inventoried.
pub(super) const MAX_CASKS: usize = 4_096;
/// Most taps inventoried.
pub(super) const MAX_TAPS: usize = 512;
/// Most services inventoried.
pub(super) const MAX_SERVICES: usize = 1_024;
/// Largest install receipt read; larger files are skipped as incomplete.
pub(super) const MAX_RECEIPT_BYTES: u64 = 1024 * 1024;
/// Most warnings retained before one summary warning is added.
pub(super) const MAX_WARNINGS: usize = 200;
