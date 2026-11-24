//! Typed refusals of a version outside the supported numeric shape.

use super::{MAX_COMPONENTS, MAX_REVISION_DIGITS, MAX_VERSION_LEN};

#[derive(Debug, thiserror::Error)]
pub(crate) enum VersionError {
    #[error("native version is empty; expected a numeric dotted version")]
    Empty,
    #[error("native version is {len} bytes long, above the {MAX_VERSION_LEN}-byte limit")]
    TooLong { len: usize },
    #[error("native version contains whitespace; whitespace is not supported")]
    Whitespace,
    #[error("native version contains non-ASCII text; only ASCII digits, '.' and '_' are supported")]
    NonAscii,
    #[error(
        "native version '{input}' contains '{separator}'; prerelease, date and build-metadata versions are not supported"
    )]
    Prerelease { input: String, separator: char },
    #[error("native version '{input}' has more than one '_revision' separator")]
    MultipleRevisions { input: String },
    #[error("native version '{input}' has no numeric base before '_revision'")]
    NoBase { input: String },
    #[error("native version '{input}' has an empty '_revision'")]
    EmptyRevision { input: String },
    #[error("native version '{input}' has a revision above {MAX_REVISION_DIGITS} decimal digits")]
    RevisionTooLong { input: String },
    #[error(
        "native version '{input}' has a non-decimal '_revision'; only decimal digits are supported"
    )]
    NonDecimalRevision { input: String },
    #[error("native version '{input}' has an empty numeric component")]
    EmptyComponent { input: String },
    #[error(
        "native version '{input}' has an unsupported character in component '{component}'; only numeric dotted versions are supported"
    )]
    BadComponent { input: String, component: String },
    #[error("native version '{input}' has more than {MAX_COMPONENTS} numeric components")]
    TooManyComponents { input: String },
}
