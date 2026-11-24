use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::domain::validate::{is_lower_hex_digest, validate_cask_url};

use super::limits::{MAX_ARTIFACT_BYTES, MAX_SOURCE_PATH_BYTES, SCHEMA_VERSION, TAPROOM};
use super::names::{
    is_known_arch, parse_tap_name, validate_bin_syntax, validate_token_syntax, validate_version,
};

/// Durable record that a tap manifest was registered.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TapRecord {
    pub(super) schema: u32,
    pub(crate) tap: String,
    pub(crate) source: String,
    pub(crate) manifest_sha256: String,
}

impl TapRecord {
    /// Build a record for a validated manifest.
    pub(crate) fn new(tap: &str, source: &Path, manifest_sha256: &str) -> Result<Self> {
        let source = source
            .to_str()
            .context("the tap manifest path must be valid UTF-8")?;
        let record = Self {
            schema: SCHEMA_VERSION,
            tap: tap.to_string(),
            source: source.to_string(),
            manifest_sha256: manifest_sha256.to_string(),
        };
        record.validate(tap)?;
        Ok(record)
    }

    /// Validate the record against the tap directory that contains it.
    pub(crate) fn validate(&self, expected_tap: &str) -> Result<()> {
        if self.schema != SCHEMA_VERSION {
            bail!(
                "unsupported tap registry schema {}; expected {SCHEMA_VERSION}",
                self.schema
            );
        }
        parse_tap_name(&self.tap)?;
        if self.tap != expected_tap {
            bail!("tap registry records '{}', not '{expected_tap}'", self.tap);
        }
        if !is_lower_hex_digest(&self.manifest_sha256) {
            bail!(
                "tap registry has an invalid manifest sha256: expected 64 lowercase hexadecimal characters"
            );
        }
        validate_source_path(&self.source)?;
        Ok(())
    }
}

/// Durable identity of one installed tap package.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TapReceipt {
    pub(super) schema: u32,
    pub(crate) tap: String,
    pub(crate) token: String,
    pub(crate) qualified: String,
    pub(crate) version: String,
    pub(crate) bin: String,
    pub(crate) architecture: String,
    pub(crate) artifact_url: String,
    pub(crate) artifact_sha256: String,
    pub(crate) artifact_bytes: u64,
    pub(crate) manifest_sha256: String,
    pub(crate) source_manifest: String,
    pub(crate) keg: String,
    pub(crate) link: String,
}

impl TapReceipt {
    /// Build a receipt with the exact keg and link layout used by the tap lane.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        tap: &str,
        token: &str,
        version: &str,
        bin: &str,
        architecture: &str,
        artifact_url: &str,
        artifact_sha256: &str,
        artifact_bytes: u64,
        manifest_sha256: &str,
        source_manifest: &Path,
    ) -> Result<Self> {
        let (owner, repo) = parse_tap_name(tap)?;
        let source_manifest = source_manifest
            .to_str()
            .context("the tap source manifest path must be valid UTF-8")?;
        let receipt = Self {
            schema: SCHEMA_VERSION,
            tap: tap.to_string(),
            token: token.to_string(),
            qualified: format!("{tap}/{token}"),
            version: version.to_string(),
            bin: bin.to_string(),
            architecture: architecture.to_string(),
            artifact_url: artifact_url.to_string(),
            artifact_sha256: artifact_sha256.to_string(),
            artifact_bytes,
            manifest_sha256: manifest_sha256.to_string(),
            source_manifest: source_manifest.to_string(),
            keg: format!("{TAPROOM}/{owner}/{repo}/{token}/{version}"),
            link: format!("bin/{bin}"),
        };
        receipt.validate()?;
        Ok(receipt)
    }

    /// Validate a decoded receipt without consulting external state.
    pub(crate) fn validate(&self) -> Result<()> {
        if self.schema != SCHEMA_VERSION {
            bail!(
                "unsupported tap receipt schema {}; expected {SCHEMA_VERSION}",
                self.schema
            );
        }
        let (owner, repo) = parse_tap_name(&self.tap)?;
        validate_token_syntax(&self.token)?;
        if self.qualified != format!("{}/{}", self.tap, self.token) {
            bail!(
                "tap receipt qualified name '{}' does not match '{}' and '{}'",
                self.qualified,
                self.tap,
                self.token
            );
        }
        validate_version(&self.version)?;
        validate_bin_syntax(&self.bin)?;
        if !is_known_arch(&self.architecture) {
            bail!(
                "tap receipt has unsupported architecture '{}'",
                self.architecture
            );
        }
        validate_cask_url(&self.artifact_url).context("tap receipt has an unsafe artifact URL")?;
        if !is_lower_hex_digest(&self.artifact_sha256) {
            bail!(
                "tap receipt has an invalid artifact sha256: expected 64 lowercase hexadecimal characters"
            );
        }
        if self.artifact_bytes == 0 || self.artifact_bytes > MAX_ARTIFACT_BYTES {
            bail!(
                "tap receipt artifact size {} is outside the supported range",
                self.artifact_bytes
            );
        }
        if !is_lower_hex_digest(&self.manifest_sha256) {
            bail!(
                "tap receipt has an invalid manifest sha256: expected 64 lowercase hexadecimal characters"
            );
        }
        validate_source_path(&self.source_manifest)?;
        let expected_keg = format!(
            "{TAPROOM}/{owner}/{repo}/{token}/{version}",
            token = self.token,
            version = self.version
        );
        if self.keg != expected_keg {
            bail!(
                "tap receipt keg '{}' is not the expected '{}'",
                self.keg,
                expected_keg
            );
        }
        let expected_link = format!("bin/{}", self.bin);
        if self.link != expected_link {
            bail!(
                "tap receipt link '{}' is not the expected '{}'",
                self.link,
                expected_link
            );
        }
        Ok(())
    }
}

fn validate_source_path(value: &str) -> Result<()> {
    if value.is_empty() || value.len() > MAX_SOURCE_PATH_BYTES {
        bail!("tap source path must be between 1 and {MAX_SOURCE_PATH_BYTES} bytes");
    }
    if value.chars().any(char::is_control) {
        bail!("tap source path must not contain control characters");
    }
    if !Path::new(value).is_absolute() {
        bail!("tap source path '{value}' must be absolute");
    }
    Ok(())
}
