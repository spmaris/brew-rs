use anyhow::{Result, bail};

use super::source::{CASK_TAP, CORE_TAP};

/// Longest accepted formula name or cask token.
pub(super) const MAX_NAME_LEN: usize = 128;

/// Longest accepted tap prefix.
pub(super) const MAX_TAP_LEN: usize = 32;

/// Reporting periods published by the analytics API.
pub(super) const ANALYTICS_PERIODS: [&str; 3] = ["30d", "90d", "365d"];

/// Which Homebrew namespace a request resolved to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Kind {
    Formula,
    Cask,
}

impl Kind {
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Formula => "formula",
            Self::Cask => "cask",
        }
    }

    pub(super) fn tap(self) -> &'static str {
        match self {
            Self::Formula => CORE_TAP,
            Self::Cask => CASK_TAP,
        }
    }

    /// Path segment used by the per-package endpoint.
    pub(super) fn endpoint(self) -> &'static str {
        match self {
            Self::Formula => "formula",
            Self::Cask => "cask",
        }
    }

    /// Analytics category published for this namespace.
    pub(super) fn analytics_category(self) -> &'static str {
        match self {
            Self::Formula => "install",
            Self::Cask => "cask-install",
        }
    }

    /// Tap spelling used in analytics endpoint paths.
    pub(super) fn analytics_tap(self) -> &'static str {
        match self {
            Self::Formula => "homebrew-core",
            Self::Cask => "homebrew-cask",
        }
    }
}

impl std::fmt::Display for Kind {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.label())
    }
}

/// A validated package argument, with the namespace implied by an explicit tap.
pub(super) struct Requested<'a> {
    pub(super) name: &'a str,
    pub(super) kind: Option<Kind>,
}

/// Validate a package argument and strip one of the official tap prefixes.
pub(super) fn parse_package(raw: &str) -> Result<Requested<'_>> {
    let raw = raw.trim();
    if raw.is_empty() {
        bail!("Package name must not be empty");
    }
    if raw.len() > MAX_NAME_LEN + MAX_TAP_LEN + 2 {
        bail!("Package name '{raw}' is longer than the supported maximum");
    }

    let mut qualified = None;
    for (prefix, kind) in [(CORE_TAP, Kind::Formula), (CASK_TAP, Kind::Cask)] {
        let prefix = format!("{prefix}/");
        if let Some(rest) = raw.strip_prefix(&prefix) {
            qualified = Some((rest, kind));
            break;
        }
    }
    let (name, kind) = match qualified {
        Some((rest, kind)) => {
            if rest.contains('/') {
                bail!("Unsupported tap in '{raw}': only {CORE_TAP} and {CASK_TAP} are supported");
            }
            (rest, Some(kind))
        }
        None => {
            if raw.contains('/') {
                bail!("Unsupported tap in '{raw}': only {CORE_TAP} and {CASK_TAP} are supported");
            }
            (raw, None)
        }
    };

    validate_identifier(name, "package name")?;
    Ok(Requested { name, kind })
}

/// Combine an explicit tap with the CLI kind flags, rejecting contradictions.
pub(super) fn requested_kind(
    raw: &str,
    requested: &Requested<'_>,
    formula: bool,
    cask: bool,
) -> Result<Option<Kind>> {
    if formula && cask {
        bail!("--formula and --cask are mutually exclusive");
    }
    let flag = if formula {
        Some(Kind::Formula)
    } else if cask {
        Some(Kind::Cask)
    } else {
        None
    };
    match (requested.kind, flag) {
        (Some(tap), Some(flag)) if tap != flag => bail!(
            "'{raw}' names a {tap} in {} but --{flag} was requested; pass the matching flag or drop --{flag}",
            tap.tap()
        ),
        (tap, flag) => Ok(tap.or(flag)),
    }
}

/// Accept only lowercase Homebrew identifiers and reject traversal and unsafe
/// characters before they can reach a URL.
pub(super) fn validate_identifier<'a>(name: &'a str, what: &str) -> Result<&'a str> {
    if name.is_empty() || name.len() > MAX_NAME_LEN {
        bail!("{what} '{name}' must be between 1 and {MAX_NAME_LEN} characters");
    }
    if name.contains("..") {
        bail!("{what} '{name}' must not contain '..'");
    }
    let mut characters = name.chars();
    match characters.next() {
        Some(first) if first.is_ascii_lowercase() || first.is_ascii_digit() => {}
        _ => bail!("{what} '{name}' must start with a lowercase letter or digit"),
    }
    if !name.chars().all(|character| {
        character.is_ascii_lowercase()
            || character.is_ascii_digit()
            || matches!(character, '@' | '+' | '-' | '_' | '.')
    }) {
        bail!("{what} '{name}' may only contain lowercase letters, digits and '@+._-'");
    }
    Ok(name)
}

/// Reject analytics periods outside the three the API publishes.
pub(super) fn validate_period(period: &str) -> Result<&str> {
    let period = period.trim();
    if ANALYTICS_PERIODS.contains(&period) {
        return Ok(period);
    }
    bail!(
        "unsupported analytics period '{period}'; the Homebrew API publishes {} — choose one of: {}",
        ANALYTICS_PERIODS.len(),
        ANALYTICS_PERIODS.join(", ")
    )
}
