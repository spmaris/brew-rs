//! Bounded declarative model for the explicit third-party tap lane.
//!
//! A tap manifest is user-provided TOML. It is parsed as data and never
//! evaluated: there is no Ruby, no shell, no source build, and no formula
//! compatibility claim. Every identifier, URL, digest, and architecture is
//! validated before the manifest can enter brewrs state, and every collection
//! is bounded.
//!
//! The manifest format is intentionally narrow:
//!
//! ```toml
//! schema = 1
//! tap = "pproenca/tap"
//!
//! [[packages]]
//! token = "agent-tui"
//! version = "1.1.0"
//! bin = "agent-tui"
//!
//! [packages.artifacts.arm64]
//! url = "https://github.com/pproenca/agent-tui/releases/download/v1.1.0/agent-tui-darwin-arm64"
//! sha256 = "0daed495a2d90acce7a720cd5ef94c7613c54b1277bffc7466b998f3edf023b5"
//!
//! [packages.artifacts.x86_64]
//! url = "https://github.com/pproenca/agent-tui/releases/download/v1.1.0/agent-tui-darwin-x64"
//! sha256 = "81c071afb4df3c688059ebe6336f543cfe5e79082147d3571274c2508ea798cb"
//! ```
//!
//! Unknown fields, unknown architectures, duplicate identifiers, duplicate
//! artifacts, reserved names, and unsupported schemas fail closed.

use std::collections::{BTreeMap, HashSet};
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use super::validate::{is_lower_hex_digest, validate_cask_url};

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
const MAX_TAP_COMPONENT_LEN: usize = 64;
/// Maximum accepted artifact URL length.
const MAX_URL_BYTES: usize = 2048;
/// Maximum accepted package token.
const MAX_TOKEN_LEN: usize = 128;
/// Maximum accepted installed executable name.
const MAX_BIN_LEN: usize = 255;
/// Maximum accepted package version.
const MAX_VERSION_LEN: usize = 128;
/// Maximum accepted source-manifest path in a registry record or receipt.
const MAX_SOURCE_PATH_BYTES: usize = 4096;

/// Owners whose namespace is reserved for the official Homebrew taps.
const RESERVED_TAP_OWNERS: [&str; 2] = ["homebrew", "linuxbrew"];
/// Repository names that would look like an official tap.
const RESERVED_TAP_REPOS: [&str; 10] = [
    "core",
    "cask",
    "cask-fonts",
    "cask-versions",
    "bundle",
    "services",
    "command-not-found",
    "homebrew-core",
    "homebrew-cask",
    "api",
];
/// Package tokens that would collide with brewrs commands or prefix layout.
const RESERVED_PACKAGE_TOKENS: [&str; 40] = [
    "bin",
    "sbin",
    "opt",
    "cellar",
    "caskroom",
    "taproom",
    "applications",
    "share",
    "etc",
    "lib",
    "include",
    "var",
    "brew",
    "brewrs",
    "brew-rs",
    "homebrew",
    "core",
    "cask",
    "casks",
    "formula",
    "formulae",
    "tap",
    "taps",
    "list",
    "install",
    "uninstall",
    "remove",
    "upgrade",
    "update",
    "search",
    "info",
    "plan",
    "fetch",
    "clean",
    "config",
    "migrate",
    "services",
    "deps",
    "autoremove",
    "shellenv",
];
/// Installed executable names that would collide with brewrs itself.
const RESERVED_BIN_NAMES: [&str; 44] = [
    "bin",
    "sbin",
    "opt",
    "cellar",
    "caskroom",
    "taproom",
    "applications",
    "share",
    "etc",
    "lib",
    "include",
    "var",
    "brew",
    "brewrs",
    "brew-rs",
    "homebrew",
    "core",
    "cask",
    "casks",
    "formula",
    "formulae",
    "tap",
    "taps",
    "list",
    "install",
    "uninstall",
    "remove",
    "upgrade",
    "update",
    "search",
    "info",
    "plan",
    "fetch",
    "clean",
    "config",
    "migrate",
    "services",
    "deps",
    "autoremove",
    "shellenv",
    "analytics",
    "check-update",
    "check-upgrade",
    "native",
];

/// One local declarative tap manifest.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TapManifest {
    pub(super) schema: u32,
    pub(super) tap: String,
    pub(crate) packages: Vec<ManifestPackage>,
}

/// One package declared by a tap manifest.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ManifestPackage {
    pub(crate) token: String,
    pub(crate) version: String,
    pub(crate) bin: String,
    pub(crate) artifacts: BTreeMap<String, Artifact>,
}

/// One architecture-specific naked executable artifact.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Artifact {
    pub(crate) url: String,
    pub(crate) sha256: String,
}

impl TapManifest {
    /// Parse and validate a manifest for exactly `expected_tap`.
    pub(crate) fn parse(bytes: &[u8], expected_tap: &str) -> Result<Self> {
        if bytes.len() as u64 > MAX_MANIFEST_BYTES {
            bail!("tap manifest exceeds the {MAX_MANIFEST_BYTES} byte limit");
        }
        let text = std::str::from_utf8(bytes).context("tap manifest is not valid UTF-8")?;
        let manifest: Self =
            toml::from_str(text).context("failed to parse the tap manifest as TOML")?;
        manifest.validate(expected_tap)?;
        Ok(manifest)
    }

    /// Validate the whole manifest, including cross-package collisions.
    fn validate(&self, expected_tap: &str) -> Result<()> {
        if self.schema != SCHEMA_VERSION {
            bail!(
                "unsupported tap manifest schema {}; expected {SCHEMA_VERSION}",
                self.schema
            );
        }
        let (owner, repo) = parse_tap_name(&self.tap)?;
        if self.tap != expected_tap {
            bail!("manifest declares tap '{}', not '{expected_tap}'", self.tap);
        }
        if self.packages.is_empty() {
            bail!("tap '{owner}/{repo}' declares no packages");
        }
        if self.packages.len() > MAX_PACKAGES {
            bail!(
                "tap '{owner}/{repo}' declares {} packages, above the {MAX_PACKAGES} package limit",
                self.packages.len()
            );
        }

        let mut tokens = HashSet::with_capacity(self.packages.len());
        let mut bins = HashSet::with_capacity(self.packages.len());
        let mut urls = HashSet::new();
        let mut digests = HashSet::new();

        for package in &self.packages {
            validate_token_identifier(&package.token)?;
            validate_version(&package.version)?;
            validate_bin_identifier(&package.bin)?;
            if !tokens.insert(package.token.as_str()) {
                bail!(
                    "tap '{owner}/{repo}' declares package token '{}' more than once",
                    package.token
                );
            }
            if !bins.insert(package.bin.as_str()) {
                bail!(
                    "tap '{owner}/{repo}' declares executable name '{}' more than once",
                    package.bin
                );
            }
            if package.artifacts.is_empty() {
                bail!(
                    "tap package '{}' declares no architecture artifacts",
                    package.token
                );
            }
            if package.artifacts.len() > 2 {
                bail!(
                    "tap package '{}' declares {} artifacts; only arm64 and x86_64 are supported",
                    package.token,
                    package.artifacts.len()
                );
            }
            for (architecture, artifact) in &package.artifacts {
                if !is_known_arch(architecture) {
                    bail!(
                        "tap package '{}' declares unsupported architecture '{architecture}'; expected arm64 or x86_64",
                        package.token
                    );
                }
                if !is_lower_hex_digest(&artifact.sha256) {
                    bail!(
                        "tap package '{}' has an invalid sha256 for '{architecture}': expected 64 lowercase hexadecimal characters",
                        package.token
                    );
                }
                if artifact.url.len() > MAX_URL_BYTES {
                    bail!(
                        "tap package '{}' has a URL longer than {MAX_URL_BYTES} bytes",
                        package.token
                    );
                }
                validate_cask_url(&artifact.url).with_context(|| {
                    format!(
                        "tap package '{}' has an unsafe '{architecture}' artifact URL",
                        package.token
                    )
                })?;
                if !urls.insert(artifact.url.as_str()) {
                    bail!(
                        "tap '{owner}/{repo}' declares artifact URL '{}' more than once; selected artifacts must be unambiguous",
                        artifact.url
                    );
                }
                if !digests.insert(artifact.sha256.as_str()) {
                    bail!(
                        "tap '{owner}/{repo}' declares artifact sha256 '{}' more than once; selected artifacts must be unambiguous",
                        artifact.sha256
                    );
                }
            }
        }

        // A token in one package must not name another package's executable.
        for (index, package) in self.packages.iter().enumerate() {
            for other in self.packages.iter().skip(index + 1) {
                if package.token == other.bin || package.bin == other.token {
                    bail!(
                        "tap '{owner}/{repo}' has colliding identifiers: package '{}' and executable '{}' name different packages",
                        package.token,
                        package.bin
                    );
                }
            }
        }
        Ok(())
    }

    /// Find one declared package by exact token.
    pub(crate) fn package(&self, token: &str) -> Result<&ManifestPackage> {
        self.packages
            .iter()
            .find(|package| package.token == token)
            .with_context(|| {
                format!(
                    "tap '{}' does not declare package '{token}'; declared packages: {}",
                    self.tap,
                    self.packages
                        .iter()
                        .map(|package| package.token.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            })
    }
}

impl ManifestPackage {
    /// Select the one artifact for `architecture`.
    pub(crate) fn artifact_for(&self, architecture: &str) -> Result<&Artifact> {
        if let Some(artifact) = self.artifacts.get(architecture) {
            return Ok(artifact);
        }
        bail!(
            "tap package '{}' has no {architecture} artifact; declared architectures: {}",
            self.token,
            self.artifacts
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>()
                .join(", ")
        )
    }
}

/// Parsed form of a fully qualified `<owner>/<repo>/<token>` target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct QualifiedTarget<'a> {
    pub(crate) owner: &'a str,
    pub(crate) repo: &'a str,
    pub(crate) token: &'a str,
}

impl QualifiedTarget<'_> {
    /// The `<owner>/<repo>` tap name.
    pub(crate) fn tap(&self) -> String {
        format!("{}/{}", self.owner, self.repo)
    }
}

/// Parse and validate a fully qualified target. Bare and two-component
/// targets are refused so a tap package can never silently resolve to an
/// official formula or cask.
pub(crate) fn parse_qualified(raw: &str) -> Result<QualifiedTarget<'_>> {
    if raw.is_empty() || raw.len() > 2 * MAX_TAP_COMPONENT_LEN + 1 + MAX_TOKEN_LEN {
        bail!("tap target '{raw}' has an unsupported length");
    }
    let mut parts = raw.split('/');
    let Some(owner) = parts.next().filter(|part| !part.is_empty()) else {
        bail!("tap target '{raw}' is not fully qualified; use <owner>/<repo>/<token>");
    };
    let Some(repo) = parts.next().filter(|part| !part.is_empty()) else {
        bail!("tap target '{raw}' is not fully qualified; use <owner>/<repo>/<token>");
    };
    let Some(token) = parts.next().filter(|part| !part.is_empty()) else {
        bail!("tap target '{raw}' is not fully qualified; use <owner>/<repo>/<token>");
    };
    if parts.next().is_some() {
        bail!("tap target '{raw}' has too many path components; use <owner>/<repo>/<token>");
    }
    validate_tap_component(owner, "tap owner")?;
    validate_tap_component(repo, "tap repository")?;
    if is_reserved_owner(owner) {
        bail!("tap owner '{owner}' is reserved for official Homebrew taps");
    }
    if is_reserved_repo(repo) {
        bail!("tap repository '{owner}/{repo}' is reserved");
    }
    validate_token_identifier(token)?;
    Ok(QualifiedTarget { owner, repo, token })
}

/// Parse and validate a `<owner>/<repo>` tap name.
pub(crate) fn parse_tap_name(raw: &str) -> Result<(String, String)> {
    if raw.is_empty() || raw.len() > 2 * MAX_TAP_COMPONENT_LEN + 1 {
        bail!("tap name '{raw}' has an unsupported length");
    }
    let Some((owner, repo)) = raw.split_once('/') else {
        bail!("tap name '{raw}' must be <owner>/<repo>");
    };
    if repo.contains('/') {
        bail!("tap name '{raw}' must contain exactly one '/'");
    }
    validate_tap_component(owner, "tap owner")?;
    validate_tap_component(repo, "tap repository")?;
    if is_reserved_owner(owner) {
        bail!("tap owner '{owner}' is reserved for official Homebrew taps");
    }
    if is_reserved_repo(repo) {
        bail!("tap repository '{raw}' is reserved");
    }
    Ok((owner.to_string(), repo.to_string()))
}

fn validate_tap_component(value: &str, what: &str) -> Result<()> {
    if value.is_empty() || value.len() > MAX_TAP_COMPONENT_LEN {
        bail!("{what} '{value}' must be between 1 and {MAX_TAP_COMPONENT_LEN} characters");
    }
    if value.contains("..") {
        bail!("{what} '{value}' must not contain '..'");
    }
    if value.starts_with('.') || value.ends_with('.') {
        bail!("{what} '{value}' must not start or end with '.'");
    }
    let mut characters = value.chars();
    match characters.next() {
        Some(first) if first.is_ascii_lowercase() || first.is_ascii_digit() => {}
        _ => bail!("{what} '{value}' must start with a lowercase letter or digit"),
    }
    if !characters.all(|character| {
        character.is_ascii_lowercase()
            || character.is_ascii_digit()
            || matches!(character, '.' | '_' | '-')
    }) {
        bail!("{what} '{value}' may only contain lowercase letters, digits, '.', '_' and '-'");
    }
    Ok(())
}

fn validate_token_identifier(value: &str) -> Result<()> {
    validate_token_syntax(value)?;
    if RESERVED_PACKAGE_TOKENS.contains(&value) {
        bail!("package token '{value}' is reserved");
    }
    Ok(())
}

fn validate_token_syntax(value: &str) -> Result<()> {
    if value.is_empty() || value.len() > MAX_TOKEN_LEN {
        bail!("package token '{value}' must be between 1 and {MAX_TOKEN_LEN} characters");
    }
    if value.contains("..") {
        bail!("package token '{value}' must not contain '..'");
    }
    if value.starts_with('.') || value.ends_with('.') {
        bail!("package token '{value}' must not start or end with '.'");
    }
    let mut characters = value.chars();
    match characters.next() {
        Some(first) if first.is_ascii_lowercase() || first.is_ascii_digit() => {}
        _ => bail!("package token '{value}' must start with a lowercase letter or digit"),
    }
    if !characters.all(|character| {
        character.is_ascii_lowercase()
            || character.is_ascii_digit()
            || matches!(character, '@' | '+' | '-' | '_' | '.')
    }) {
        bail!("package token '{value}' may only contain [a-z0-9@+._-]");
    }
    Ok(())
}

fn validate_bin_identifier(value: &str) -> Result<()> {
    validate_bin_syntax(value)?;
    if RESERVED_BIN_NAMES.contains(&value) {
        bail!("executable name '{value}' is reserved");
    }
    Ok(())
}

fn validate_bin_syntax(value: &str) -> Result<()> {
    if value.is_empty() || value.len() > MAX_BIN_LEN {
        bail!("executable name '{value}' must be between 1 and {MAX_BIN_LEN} characters");
    }
    if value.contains("..") {
        bail!("executable name '{value}' must not contain '..'");
    }
    if value.starts_with('.') || value.ends_with('.') {
        bail!("executable name '{value}' must not start or end with '.'");
    }
    let mut characters = value.chars();
    match characters.next() {
        Some(first) if first.is_ascii_lowercase() || first.is_ascii_digit() || first == '_' => {}
        _ => bail!("executable name '{value}' must start with a lowercase letter, digit or '_'"),
    }
    if !characters.all(|character| {
        character.is_ascii_lowercase()
            || character.is_ascii_digit()
            || matches!(character, '@' | '+' | '-' | '_' | '.')
    }) {
        bail!("executable name '{value}' may only contain [a-z0-9@+._-]");
    }
    Ok(())
}

fn validate_version(value: &str) -> Result<()> {
    if value.is_empty() || value.len() > MAX_VERSION_LEN {
        bail!("version '{value}' must be between 1 and {MAX_VERSION_LEN} characters");
    }
    if value.contains("..") {
        bail!("version '{value}' must not contain '..'");
    }
    if value.ends_with('.') {
        bail!("version '{value}' must not end with '.'");
    }
    if !value
        .chars()
        .next()
        .is_some_and(|first| first.is_ascii_alphanumeric())
    {
        bail!("version '{value}' must start with a letter or digit");
    }
    if !value.chars().all(|character| {
        character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '+' | '-')
    }) {
        bail!("version '{value}' may only contain [A-Za-z0-9._+-]");
    }
    Ok(())
}

fn is_reserved_owner(owner: &str) -> bool {
    RESERVED_TAP_OWNERS.contains(&owner)
}

fn is_reserved_repo(repo: &str) -> bool {
    RESERVED_TAP_REPOS.contains(&repo)
}

/// Whether `architecture` is one of the two supported macOS target names.
pub(crate) fn is_known_arch(architecture: &str) -> bool {
    matches!(architecture, "arm64" | "x86_64")
}

/// The host architecture name used to select a manifest artifact.
pub(crate) fn host_arch() -> Result<&'static str> {
    if std::env::consts::OS != "macos" {
        bail!(
            "third-party tap artifacts are naked macOS executables; this host is {}",
            std::env::consts::OS
        );
    }
    match std::env::consts::ARCH {
        "aarch64" => Ok("arm64"),
        "x86_64" => Ok("x86_64"),
        other => bail!("unsupported host architecture '{other}' for macOS tap artifacts"),
    }
}

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

#[cfg(test)]
mod tests;
