//! Typed failures of text-file placeholder relocation.

use std::path::PathBuf;
use std::str::Utf8Error;

use super::super::paths::PathsError;

/// Why a staged text file could not be relocated.
#[derive(Debug, thiserror::Error)]
pub(crate) enum TextError {
    #[error(transparent)]
    Paths(#[from] PathsError),

    // --- classification and decoding ---
    #[error("Text relocation requires a UTF-8 relative path")]
    NonUtf8Path,
    #[error("Unsupported pkg-config file name {name:?}")]
    UnsupportedPkgConfigName { name: String },
    #[error("Refusing to relocate {}: text file exceeds the {limit} byte limit", .path.display())]
    TooLarge { path: PathBuf, limit: usize },
    #[error("Refusing to rewrite {}", .path.display())]
    Rewrite {
        path: PathBuf,
        #[source]
        source: Box<TextError>,
    },
    #[error("Refusing to leave a Homebrew marker in {}", .path.display())]
    LeftMarker { path: PathBuf },
    #[error("{} is not valid UTF-8 text", .path.display())]
    NotUtf8 {
        path: PathBuf,
        #[source]
        source: Utf8Error,
    },
    #[error("Refusing to rewrite {}: file must end with a newline", .path.display())]
    MissingNewline { path: PathBuf },
    #[error("Refusing to rewrite {}: file contains unsupported control bytes", .path.display())]
    ControlBytes { path: PathBuf },

    // --- general text ---
    #[error("Missing formula name after {marker}")]
    MissingFormulaName { marker: &'static str },
    #[error("Missing '/' after formula name in {marker}{name}")]
    MissingSlash { marker: &'static str, name: String },
    #[error("Missing version after {marker}{name}/")]
    MissingVersion { marker: &'static str, name: String },
    #[error("Unsupported Homebrew placeholder {token:?}")]
    UnsupportedPlaceholder { token: String },
    #[error("Rewritten line still contains an unresolved Homebrew placeholder")]
    UnresolvedPlaceholder,
    #[error("Unsupported trailing token after Homebrew placeholder in {preview:?}")]
    TrailingToken { preview: String },
    #[error("Bare @@HOMEBREW_PREFIX@@/opt placeholder without formula name is unsupported")]
    BareOpt,
    #[error("Unsupported '..' traversal after Homebrew placeholder")]
    ParentTraversal,

    // --- mapping ---
    #[error("Unsupported literal path {value:?} in {context}")]
    UnsupportedLiteral {
        value: String,
        context: &'static str,
    },
    #[error("Unsupported Homebrew placeholder path {placeholder:?}")]
    UnsupportedPlaceholderPath { placeholder: String },
    #[error(
        "Unsupported pcre2-config prefix placeholder {value:?}: expected a complete pinned keg"
    )]
    UnsupportedPrefixPlaceholder { value: String },
    #[error("Unsupported fixed Homebrew path {value:?}")]
    UnsupportedFixedPath { value: String },
    #[error("Unsupported mapped path {value:?}")]
    UnsupportedMappedPath { value: String },
    #[error("Cannot quote a shell value containing control bytes")]
    UnquotableShellValue,

    // --- pcre2-config ---
    #[error("pcre2-config must start with the upstream #!/bin/sh line")]
    Pcre2MissingShebang,
    #[error("pcre2-config contains non-ASCII bytes")]
    Pcre2NonAscii,
    #[error("Unsupported pcre2-config test line {line:?}")]
    Pcre2UnsupportedTestLine { line: String },
    #[error("Unsupported pcre2-config echo line {line:?}")]
    Pcre2UnsupportedEchoLine { line: String },
    #[error("pcre2-config still contains a fixed /opt/homebrew path")]
    Pcre2FixedPathRemains,
    #[error("pcre2-config line does not end with a newline")]
    Pcre2MissingNewline,
    #[error("duplicate pcre2-config {what}")]
    Pcre2Duplicate { what: &'static str },
    #[error("pcre2-config is missing its {what}")]
    Pcre2Missing { what: &'static str },
    #[error("pcre2-config has only one of its two fixed-prefix libR branches")]
    Pcre2LibRBranches,
    #[error("Unsupported Homebrew placeholder in pcre2-config line {body:?}")]
    Pcre2UnsupportedMarkerLine { body: String },
    #[error("Unsupported fixed /opt/homebrew path in pcre2-config line {body:?}")]
    Pcre2UnsupportedFixedLine { body: String },

    // --- pkg-config ---
    #[error("Fixed /opt/homebrew path in a pkg-config file is unsupported")]
    PcFixedPath,
    #[error("duplicate pkg-config variable {name:?} carries a Homebrew placeholder")]
    PcDuplicateVariable { name: String },
    #[error("Unsupported literal path {value:?} in pkg-config variable {name:?}")]
    PcUnsupportedLiteral { value: String, name: String },
    #[error("Homebrew marker did not occur in a supported pkg-config context")]
    PcNoMarkerContext,
    #[error("pkg-config line does not end with a newline")]
    PcMissingNewline,
    #[error("Homebrew placeholder outside a pkg-config variable assignment")]
    PcOutsideAssignment,
    #[error("Unsupported pkg-config variable name {name:?}")]
    PcUnsupportedName { name: String },
    #[error("Mapped pkg-config path {value:?} cannot be represented literally")]
    PcUnrepresentable { value: String },
}
