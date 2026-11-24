use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::domain::validate::{is_lower_hex_digest, validate_cask_url};

use super::error::RecordError;
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
    pub(crate) fn new(
        tap: &str,
        source: &Path,
        manifest_sha256: &str,
    ) -> Result<Self, RecordError> {
        let source = source.to_str().ok_or(RecordError::ManifestPathNotUtf8)?;
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
    pub(crate) fn validate(&self, expected_tap: &str) -> Result<(), RecordError> {
        if self.schema != SCHEMA_VERSION {
            return Err(RecordError::UnsupportedRegistrySchema { found: self.schema });
        }
        parse_tap_name(&self.tap)?;
        if self.tap != expected_tap {
            return Err(RecordError::RegistryTapMismatch {
                recorded: self.tap.clone(),
                expected: expected_tap.to_owned(),
            });
        }
        if !is_lower_hex_digest(&self.manifest_sha256) {
            return Err(RecordError::InvalidRegistryManifestSha256);
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
    ) -> Result<Self, RecordError> {
        let (owner, repo) = parse_tap_name(tap)?;
        let source_manifest = source_manifest
            .to_str()
            .ok_or(RecordError::SourceManifestPathNotUtf8)?;
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
    pub(crate) fn validate(&self) -> Result<(), RecordError> {
        if self.schema != SCHEMA_VERSION {
            return Err(RecordError::UnsupportedReceiptSchema { found: self.schema });
        }
        let (owner, repo) = parse_tap_name(&self.tap)?;
        validate_token_syntax(&self.token)?;
        if self.qualified != format!("{}/{}", self.tap, self.token) {
            return Err(RecordError::QualifiedMismatch {
                qualified: self.qualified.clone(),
                tap: self.tap.clone(),
                token: self.token.clone(),
            });
        }
        validate_version(&self.version)?;
        validate_bin_syntax(&self.bin)?;
        if !is_known_arch(&self.architecture) {
            return Err(RecordError::UnsupportedArchitecture {
                architecture: self.architecture.clone(),
            });
        }
        validate_cask_url(&self.artifact_url).map_err(RecordError::UnsafeArtifactUrl)?;
        if !is_lower_hex_digest(&self.artifact_sha256) {
            return Err(RecordError::InvalidArtifactSha256);
        }
        if self.artifact_bytes == 0 || self.artifact_bytes > MAX_ARTIFACT_BYTES {
            return Err(RecordError::ArtifactSize {
                bytes: self.artifact_bytes,
            });
        }
        if !is_lower_hex_digest(&self.manifest_sha256) {
            return Err(RecordError::InvalidReceiptManifestSha256);
        }
        validate_source_path(&self.source_manifest)?;
        let expected_keg = format!(
            "{TAPROOM}/{owner}/{repo}/{token}/{version}",
            token = self.token,
            version = self.version
        );
        if self.keg != expected_keg {
            return Err(RecordError::KegMismatch {
                found: self.keg.clone(),
                expected: expected_keg,
            });
        }
        let expected_link = format!("bin/{}", self.bin);
        if self.link != expected_link {
            return Err(RecordError::LinkMismatch {
                found: self.link.clone(),
                expected: expected_link,
            });
        }
        Ok(())
    }
}

fn validate_source_path(value: &str) -> Result<(), RecordError> {
    if value.is_empty() || value.len() > MAX_SOURCE_PATH_BYTES {
        return Err(RecordError::SourcePathLength);
    }
    if value.chars().any(char::is_control) {
        return Err(RecordError::SourcePathControl);
    }
    if !Path::new(value).is_absolute() {
        return Err(RecordError::SourcePathNotAbsolute {
            value: value.to_owned(),
        });
    }
    Ok(())
}
