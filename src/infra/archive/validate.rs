//! Validation of the digest, name and version arguments.

use anyhow::{Result, bail};

/// Length of a SHA-256 digest in lowercase hexadecimal.
const SHA256_HEX_LEN: usize = 64;
/// Longest formula name accepted in an archive path.
const MAX_NAME_LEN: usize = 128;
/// Longest version accepted in an archive path.
const MAX_VERSION_LEN: usize = 128;

/// Require exactly 64 lowercase hexadecimal characters.
pub(super) fn validate_digest(sha256: &str) -> Result<()> {
    if sha256.len() != SHA256_HEX_LEN
        || !sha256
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
    {
        bail!("bottle sha256 must be exactly {SHA256_HEX_LEN} lowercase hexadecimal characters");
    }
    Ok(())
}

/// Require a safe Homebrew formula name.
pub(super) fn validate_name(name: &str) -> Result<()> {
    if name.is_empty() || name.len() > MAX_NAME_LEN {
        bail!("bottle name must be between 1 and {MAX_NAME_LEN} bytes");
    }
    if !name.as_bytes()[0].is_ascii_lowercase() && !name.as_bytes()[0].is_ascii_digit() {
        bail!("bottle name '{name}' must start with a lowercase letter or digit");
    }
    if name.contains("..")
        || !name.bytes().all(|byte| {
            byte.is_ascii_lowercase()
                || byte.is_ascii_digit()
                || matches!(byte, b'@' | b'+' | b'-' | b'_' | b'.')
        })
    {
        bail!("bottle name '{name}' contains unsupported characters");
    }
    Ok(())
}

/// Require a safe bottle version.
pub(super) fn validate_version(version: &str) -> Result<()> {
    if version.is_empty() || version.len() > MAX_VERSION_LEN {
        bail!("bottle version must be between 1 and {MAX_VERSION_LEN} bytes");
    }
    if version.contains("..")
        || !version
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'+' | b'-'))
    {
        bail!("bottle version '{version}' contains unsupported characters");
    }
    Ok(())
}
