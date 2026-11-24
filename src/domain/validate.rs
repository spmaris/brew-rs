//! Pure validators shared by planning, manifests and the infra layer.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use url::Url;

mod error;

pub(crate) use error::UrlError;

/// Longest accepted initial cask download URL.
pub(crate) const MAX_URL_BYTES: usize = 2048;
/// Longest accepted package version.
const MAX_VERSION_LEN: usize = 128;

/// Validate an initial cask download URL before accepting it into a plan or
/// connecting to it.
pub(crate) fn validate_cask_url(url: &str) -> Result<Url, UrlError> {
    validate_cask_url_with_limit(url, MAX_URL_BYTES, "cask URL")
}

pub(crate) fn validate_cask_url_with_limit(
    raw: &str,
    max_bytes: usize,
    label: &'static str,
) -> Result<Url, UrlError> {
    if raw.is_empty() {
        return Err(UrlError::Empty { label });
    }
    if raw.len() > max_bytes {
        return Err(UrlError::TooLong { label, max_bytes });
    }
    if raw.chars().any(|ch| ch.is_control() || ch.is_whitespace()) {
        return Err(UrlError::ControlCharacters { label });
    }
    if raw.contains('\\') {
        return Err(UrlError::Backslash { label });
    }

    let parsed = Url::parse(raw).map_err(|source| UrlError::Parse {
        label,
        raw: raw.to_owned(),
        source,
    })?;
    validate_parsed_cask_url(&parsed, label)?;
    Ok(parsed)
}

fn validate_parsed_cask_url(parsed: &Url, label: &'static str) -> Result<(), UrlError> {
    if parsed.scheme() != "https" {
        return Err(UrlError::NotHttps { label });
    }
    if parsed.fragment().is_some() {
        return Err(UrlError::Fragment { label });
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(UrlError::Credentials { label });
    }
    if parsed.port().is_some() {
        return Err(UrlError::Port { label });
    }
    let Some(host) = parsed.host_str() else {
        return Err(UrlError::NoHost { label });
    };
    if is_disallowed_host(host) {
        return Err(UrlError::DisallowedHost {
            label,
            host: host.to_owned(),
        });
    }
    Ok(())
}

fn is_disallowed_host(host: &str) -> bool {
    let normalized = host.trim_end_matches('.');
    if normalized.is_empty() {
        return true;
    }
    let lower = normalized.to_ascii_lowercase();
    if lower == "localhost" || lower.ends_with(".localhost") || lower.ends_with(".local") {
        return true;
    }
    let ip_candidate = normalized
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
        .unwrap_or(normalized);
    if let Ok(addr) = ip_candidate.parse::<IpAddr>() {
        return is_disallowed_ip(addr);
    }
    false
}

fn is_disallowed_ip(addr: IpAddr) -> bool {
    match addr {
        IpAddr::V4(v4) => is_disallowed_v4(v4),
        IpAddr::V6(v6) => is_disallowed_v6(v6),
    }
}

fn is_disallowed_v4(v4: Ipv4Addr) -> bool {
    v4.is_loopback()
        || v4.is_private()
        || v4.is_link_local()
        || v4.is_unspecified()
        || v4.octets()[0] == 0
        || v4.is_broadcast()
        || v4.is_multicast()
        || v4.is_documentation()
}

fn is_disallowed_v6(v6: Ipv6Addr) -> bool {
    if let Some(v4) = v6.to_ipv4_mapped() {
        return is_disallowed_v4(v4);
    }
    let first = v6.segments()[0];
    let is_ula = (first & 0xfe00) == 0xfc00;
    let is_unicast_link_local = (first & 0xffc0) == 0xfe80;
    v6.is_loopback() || v6.is_unspecified() || v6.is_multicast() || is_ula || is_unicast_link_local
}

/// Whether `value` is exactly 64 lowercase hexadecimal characters.
pub(crate) fn is_lower_hex_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

pub(crate) fn is_safe_version(version: &str) -> bool {
    if version.is_empty() || version.len() > MAX_VERSION_LEN || version.contains("..") {
        return false;
    }
    version
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'+' | b'-'))
}

#[cfg(test)]
mod tests;
