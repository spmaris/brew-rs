//! Small text helpers shared by command output.

/// Digits of a digest rendered in a table or message.
const SHORT_DIGEST_LEN: usize = 12;

/// Render a digest prefix, keeping full digests out of the terminal while
/// still naming each side of a conflict. Cuts on a character boundary.
pub(crate) fn short_digest(digest: &str) -> &str {
    match digest.char_indices().nth(SHORT_DIGEST_LEN) {
        Some((end, _)) => digest.get(..end).unwrap_or(digest),
        None => digest,
    }
}

/// Render a byte count in binary units: `812 B`, `1.0 KiB`, `812.4 MiB`.
pub(crate) fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["KiB", "MiB", "GiB", "TiB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64 / 1024.0;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    let name = UNITS.get(unit).copied().unwrap_or("TiB");
    format!("{value:.1} {name}")
}

/// Finished text for a download step.
pub(crate) fn downloaded_text(name: &str, version: &str, bytes: u64, cached: bool) -> String {
    if cached {
        format!("Using cached {name} {version} (checksum verified)")
    } else {
        format!(
            "Downloaded {name} {version} ({}, checksum verified)",
            format_size(bytes)
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_use_binary_units() {
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(1023), "1023 B");
        assert_eq!(format_size(1024), "1.0 KiB");
        assert_eq!(format_size(852_000_000), "812.5 MiB");
        assert_eq!(format_size(851_900_000), "812.4 MiB");
        assert_eq!(format_size(1_503_238_554), "1.4 GiB");
    }

    #[test]
    fn download_text_names_size_or_cache() {
        assert_eq!(
            downloaded_text("jq", "1.8.1", 1_048_576, false),
            "Downloaded jq 1.8.1 (1.0 MiB, checksum verified)"
        );
        assert_eq!(
            downloaded_text("jq", "1.8.1", 1_048_576, true),
            "Using cached jq 1.8.1 (checksum verified)"
        );
    }
}
