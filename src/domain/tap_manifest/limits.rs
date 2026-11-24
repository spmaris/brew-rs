/// Supported manifest and receipt schema.
pub(super) const SCHEMA_VERSION: u32 = 1;
/// Prefix-relative root for installed tap payloads.
pub(crate) const TAPROOM: &str = "Taproom";
/// Largest accepted manifest, including whitespace and comments.
pub(crate) const MAX_MANIFEST_BYTES: u64 = 256 * 1024;
/// Largest accepted tap registry record.
pub(crate) const MAX_RECORD_BYTES: u64 = 16 * 1024;
/// Largest accepted tap receipt.
pub(crate) const MAX_RECEIPT_BYTES: u64 = 64 * 1024;
/// Largest accepted naked executable artifact.
pub(crate) const MAX_ARTIFACT_BYTES: u64 = 512 * 1024 * 1024;
/// Maximum number of taps registered in one prefix.
pub(crate) const MAX_TAPS: usize = 64;
/// Maximum number of packages declared by one tap.
pub(crate) const MAX_PACKAGES: usize = 64;
/// Maximum accepted owner or repository component.
pub(super) const MAX_TAP_COMPONENT_LEN: usize = 64;
/// Maximum accepted artifact URL length.
pub(super) const MAX_URL_BYTES: usize = 2048;
/// Maximum accepted package token.
pub(super) const MAX_TOKEN_LEN: usize = 128;
/// Maximum accepted installed executable name.
pub(super) const MAX_BIN_LEN: usize = 255;
/// Maximum accepted package version.
pub(super) const MAX_VERSION_LEN: usize = 128;
/// Maximum accepted source-manifest path in a registry record or receipt.
pub(super) const MAX_SOURCE_PATH_BYTES: usize = 4096;
