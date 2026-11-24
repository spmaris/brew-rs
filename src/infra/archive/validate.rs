//! Validation of the digest, name and version arguments.

use super::error::ArchiveError;

/// Length of a SHA-256 digest in lowercase hexadecimal.
const SHA256_HEX_LEN: usize = 64;
/// Longest formula name accepted in an archive path.
const MAX_NAME_LEN: usize = 128;
/// Longest version accepted in an archive path.
const MAX_VERSION_LEN: usize = 128;

/// Require exactly 64 lowercase hexadecimal characters.
pub(super) fn validate_digest(sha256: &str) -> Result<(), ArchiveError> {
    if sha256.len() != SHA256_HEX_LEN
        || !sha256
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
    {
        return Err(ArchiveError::InvalidDigest {
            len: SHA256_HEX_LEN,
        });
    }
    Ok(())
}

/// Require a safe Homebrew formula name.
pub(super) fn validate_name(name: &str) -> Result<(), ArchiveError> {
    if name.is_empty() || name.len() > MAX_NAME_LEN {
        return Err(ArchiveError::InvalidNameLength { max: MAX_NAME_LEN });
    }
    if !name.as_bytes()[0].is_ascii_lowercase() && !name.as_bytes()[0].is_ascii_digit() {
        return Err(ArchiveError::InvalidNameStart {
            name: name.to_owned(),
        });
    }
    if name.contains("..")
        || !name.bytes().all(|byte| {
            byte.is_ascii_lowercase()
                || byte.is_ascii_digit()
                || matches!(byte, b'@' | b'+' | b'-' | b'_' | b'.')
        })
    {
        return Err(ArchiveError::InvalidNameChars {
            name: name.to_owned(),
        });
    }
    Ok(())
}

/// Require a safe bottle version.
pub(super) fn validate_version(version: &str) -> Result<(), ArchiveError> {
    if version.is_empty() || version.len() > MAX_VERSION_LEN {
        return Err(ArchiveError::InvalidVersionLength {
            max: MAX_VERSION_LEN,
        });
    }
    if version.contains("..")
        || !version
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'+' | b'-'))
    {
        return Err(ArchiveError::InvalidVersionChars {
            version: version.to_owned(),
        });
    }
    Ok(())
}
