//! User-facing sentences that more than one command prints, kept in one place
//! so the wording cannot drift between formulae, casks and `native plan`.

/// Formulae and casks were both checked and nothing needs upgrading.
pub fn everything_up_to_date() -> &'static str {
    "Everything is up to date."
}

pub fn all_packages_up_to_date() -> &'static str {
    "All packages are up to date."
}

pub fn all_casks_up_to_date() -> &'static str {
    "All casks are up to date."
}

pub fn no_packages_installed() -> &'static str {
    "No packages installed yet. Install one with: brewrs install <name>"
}

pub fn no_casks_installed() -> &'static str {
    "No casks installed yet."
}

/// Closing line of a dry run, using the same nouns as the panel summary.
pub fn dry_run_summary(packages: usize, casks: usize) -> String {
    let noun = |count: usize, one: &str, many: &str| {
        format!("{count} {}", if count == 1 { one } else { many })
    };
    let changes = match (packages, casks) {
        (0, 0) => return "Dry run: nothing to do.".to_string(),
        (packages, 0) => noun(packages, "package", "packages"),
        (0, casks) => noun(casks, "cask", "casks"),
        (packages, casks) => format!(
            "{} and {}",
            noun(packages, "package", "packages"),
            noun(casks, "cask", "casks")
        ),
    };
    format!("Dry run: {changes} would change, nothing applied.")
}

/// Printed when the user declines the confirmation; `noun` is `packages` or
/// `casks`.
pub fn cancelled(noun: &str) -> String {
    format!("Cancelled. No {noun} changed.")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dry_run_names_what_would_change() {
        assert_eq!(dry_run_summary(0, 0), "Dry run: nothing to do.");
        assert_eq!(
            dry_run_summary(1, 0),
            "Dry run: 1 package would change, nothing applied."
        );
        assert_eq!(
            dry_run_summary(2, 0),
            "Dry run: 2 packages would change, nothing applied."
        );
        assert_eq!(
            dry_run_summary(0, 2),
            "Dry run: 2 casks would change, nothing applied."
        );
        assert_eq!(
            dry_run_summary(2, 1),
            "Dry run: 2 packages and 1 cask would change, nothing applied."
        );
    }

    #[test]
    fn cancelled_names_the_kind() {
        assert_eq!(cancelled("casks"), "Cancelled. No casks changed.");
    }
}
