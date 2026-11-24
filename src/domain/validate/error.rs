//! Typed refusals of an untrusted download URL.

/// Why a cask download URL was refused. `label` names the URL in the message
/// (for example `cask URL` or `cask redirect URL`).
#[derive(Debug, thiserror::Error)]
pub(crate) enum UrlError {
    #[error("refusing empty {label}")]
    Empty { label: &'static str },
    #[error("refusing {label}: URL exceeds {max_bytes} bytes")]
    TooLong {
        label: &'static str,
        max_bytes: usize,
    },
    #[error("refusing {label}: URL contains whitespace or control characters")]
    ControlCharacters { label: &'static str },
    #[error("refusing {label}: URL must not contain backslashes")]
    Backslash { label: &'static str },
    #[error("invalid {label} '{raw}'")]
    Parse {
        label: &'static str,
        raw: String,
        #[source]
        source: url::ParseError,
    },
    #[error("refusing {label}: URL scheme must be https")]
    NotHttps { label: &'static str },
    #[error("refusing {label}: URL must not contain a fragment")]
    Fragment { label: &'static str },
    #[error("refusing {label}: URL must not contain credentials")]
    Credentials { label: &'static str },
    #[error("refusing {label}: explicit non-default port is not allowed")]
    Port { label: &'static str },
    #[error("refusing {label}: URL does not name a host")]
    NoHost { label: &'static str },
    #[error("refusing {label}: host '{host}' is not an allowed public HTTPS host")]
    DisallowedHost { label: &'static str, host: String },
}
