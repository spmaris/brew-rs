//! Wording for cask step lines, kept pure so it can be tested directly.

/// What a step acts on: the single app bundle, `N apps`, or the cask itself
/// when nothing in it is an app bundle.
fn target(tokens: &[String], apps: &[String]) -> String {
    match (apps, tokens) {
        ([app], _) => app.clone(),
        ([], [token]) => token.clone(),
        ([], _) => format!("{} casks", tokens.len()),
        _ => format!("{} apps", apps.len()),
    }
}

/// `(active, done)` for copying the staged apps into the appdir, or linking
/// casks that ship no app bundle.
pub(super) fn commit_labels(tokens: &[String], apps: &[String]) -> (String, String) {
    let (active, done) = if apps.is_empty() {
        ("Linking", "Linked")
    } else {
        ("Copying", "Copied")
    };
    let target = target(tokens, apps);
    (format!("{active} {target}"), format!("{done} {target}"))
}

/// `(active, done)` for removing installed casks.
pub(super) fn remove_labels(tokens: &[String], apps: &[String]) -> (String, String) {
    let target = target(tokens, apps);
    (format!("Removing {target}"), format!("Removed {target}"))
}

/// `(active, done)` for swapping upgraded casks in for their old versions.
pub(super) fn replace_labels(tokens: &[String]) -> (String, String) {
    let target = match tokens {
        [token] => token.clone(),
        _ => format!("{} casks", tokens.len()),
    };
    (format!("Replacing {target}"), format!("Replaced {target}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| item.to_string()).collect()
    }

    #[test]
    fn commit_names_a_single_app() {
        let (active, done) = commit_labels(&strings(&["alt-tab"]), &strings(&["AltTab.app"]));
        assert_eq!(active, "Copying AltTab.app");
        assert_eq!(done, "Copied AltTab.app");
    }

    #[test]
    fn commit_counts_several_apps() {
        let (active, done) = commit_labels(&strings(&["suite"]), &strings(&["A.app", "B.app"]));
        assert_eq!(active, "Copying 2 apps");
        assert_eq!(done, "Copied 2 apps");
    }

    #[test]
    fn commit_links_casks_without_apps() {
        let (active, done) = commit_labels(&strings(&["tool"]), &[]);
        assert_eq!(active, "Linking tool");
        assert_eq!(done, "Linked tool");
        let (active, _) = commit_labels(&strings(&["a", "b"]), &[]);
        assert_eq!(active, "Linking 2 casks");
    }

    #[test]
    fn remove_prefers_app_names_then_tokens() {
        let (active, done) = remove_labels(&strings(&["alt-tab"]), &strings(&["AltTab.app"]));
        assert_eq!(active, "Removing AltTab.app");
        assert_eq!(done, "Removed AltTab.app");
        let (active, done) = remove_labels(&strings(&["tool"]), &[]);
        assert_eq!(active, "Removing tool");
        assert_eq!(done, "Removed tool");
        let (active, _) = remove_labels(&strings(&["a", "b"]), &strings(&["A.app", "B.app"]));
        assert_eq!(active, "Removing 2 apps");
    }

    #[test]
    fn replace_names_the_cask_or_counts() {
        let (active, done) = replace_labels(&strings(&["alt-tab"]));
        assert_eq!(active, "Replacing alt-tab");
        assert_eq!(done, "Replaced alt-tab");
        let (active, done) = replace_labels(&strings(&["a", "b"]));
        assert_eq!(active, "Replacing 2 casks");
        assert_eq!(done, "Replaced 2 casks");
    }
}
