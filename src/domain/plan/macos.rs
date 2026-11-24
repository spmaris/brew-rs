//! macOS release and bottle-tag knowledge shared by the planner and cask planner.

use super::error::MacosTagError;

/// Maximum length of a macOS minimum version string.
pub(super) const MAX_REQUIREMENT_VERSION_LEN: usize = 16;

/// A parsed macOS release, compared as a dotted triple.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(in crate::domain) struct MacosVersion {
    pub(super) major: u32,
    pub(super) minor: u32,
    pub(super) patch: u32,
}

/// Known Homebrew macOS bottle codenames and the release they name, oldest
/// first.
///
/// A tag whose codename is missing here counts as an unsupported platform, so a
/// new macOS release is refused until its system libraries and version have
/// been checked. The version feeds only minimum-requirement comparisons.
pub(super) const MACOS_RELEASES: [(&str, u32, u32); 12] = [
    ("el_capitan", 10, 11),
    ("sierra", 10, 12),
    ("high_sierra", 10, 13),
    ("mojave", 10, 14),
    ("catalina", 10, 15),
    ("big_sur", 11, 0),
    ("monterey", 12, 0),
    ("ventura", 13, 0),
    ("sonoma", 14, 0),
    ("sequoia", 15, 0),
    ("tahoe", 26, 0),
    ("golden_gate", 27, 0),
];

/// CPU architecture named by a Homebrew bottle tag.
pub(in crate::domain) fn arch_for_tag(tag: &str) -> Option<&'static str> {
    if tag.starts_with("arm64_") {
        Some("arm64")
    } else if tag.starts_with("x86_64_") || macos_version_for_tag(tag).is_some() {
        Some("x86_64")
    } else {
        None
    }
}

/// Parse a numeric macOS version (`12`, `10.15`, `14.2.1`) into a dotted triple.
///
/// Only one to three non-empty numeric components of at most three digits are
/// accepted, leading zeros are refused, and a major version below 10 is treated
/// as unknown. A false return keeps the requirement blocked.
pub(in crate::domain) fn parse_macos_version(value: &str) -> Option<MacosVersion> {
    if value.is_empty() || value.len() > MAX_REQUIREMENT_VERSION_LEN {
        return None;
    }
    let mut components = [0_u32; 3];
    let mut count = 0;
    for component in value.split('.') {
        if count == components.len()
            || component.is_empty()
            || component.len() > 3
            || !component.bytes().all(|byte| byte.is_ascii_digit())
            || (component.len() > 1 && component.starts_with('0'))
        {
            return None;
        }
        components[count] = component.parse().ok()?;
        count += 1;
    }
    if count == 0 || components[0] < 10 {
        return None;
    }
    Some(MacosVersion {
        major: components[0],
        minor: components[1],
        patch: components[2],
    })
}

/// Parse either a numeric macOS version (`13`, `14.2`) or a known codename (`sequoia`).
pub(in crate::domain) fn parse_macos_version_or_codename(value: &str) -> Option<MacosVersion> {
    parse_macos_version(value).or_else(|| {
        MACOS_RELEASES
            .iter()
            .find(|(name, _, _)| *name == value)
            .map(|(_, major, minor)| MacosVersion {
                major: *major,
                minor: *minor,
                patch: 0,
            })
    })
}

/// Homebrew uses `arm64_<codename>` on Apple Silicon and bare codenames on Intel.
/// Neither Linux tags nor unknown codenames provide this dependency guarantee.
pub(crate) fn is_supported_macos_tag(tag: &str) -> bool {
    macos_version_for_tag(tag).is_some()
}

/// Map a macOS bottle tag to the release it names, if the release is known.
pub(in crate::domain) fn macos_version_for_tag(tag: &str) -> Option<MacosVersion> {
    let codename = tag.strip_prefix("arm64_").unwrap_or(tag);
    MACOS_RELEASES
        .iter()
        .find(|(name, _, _)| *name == codename)
        .map(|(_, major, minor)| MacosVersion {
            major: *major,
            minor: *minor,
            patch: 0,
        })
}

pub(crate) fn macos_tag(arch: &str, version: &str) -> Result<String, MacosTagError> {
    let release = match version.split('.').next() {
        Some("11") => "big_sur",
        Some("12") => "monterey",
        Some("13") => "ventura",
        Some("14") => "sonoma",
        Some("15") => "sequoia",
        Some("26") => "tahoe",
        _ => {
            return Err(MacosTagError::UnknownRelease {
                version: version.to_owned(),
            });
        }
    };
    match arch {
        "aarch64" => Ok(format!("arm64_{release}")),
        "x86_64" => Ok(release.into()),
        _ => Err(MacosTagError::UnknownArch {
            arch: arch.to_owned(),
        }),
    }
}
