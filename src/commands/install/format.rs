use std::fs;
use std::path::Path;

use super::shellenv::posix_quote;

/// The exceptional note shown for a plan row, or an empty string.
pub(super) fn formula_note(keg_only: bool) -> String {
    if keg_only {
        "keg-only".to_string()
    } else {
        String::new()
    }
}

/// The confirmation question for an install with pending packages and/or
/// promotions; `confirm` appends the `[y/N]` suffix.
pub(super) fn install_question(pending: usize, promotions: usize) -> String {
    match (pending, promotions) {
        (0, count) => format!(
            "Mark {count} {} as requested?",
            plural(count, "package", "packages")
        ),
        (count, 0) => format!("Install {count} {}?", plural(count, "package", "packages")),
        (installs, marks) => format!(
            "Install {installs} {} and mark {marks} as requested?",
            plural(installs, "package", "packages")
        ),
    }
}

/// `1 package`, or `2 packages (1 requested, 1 dependency)`; the breakdown is
/// dropped when nothing was pulled in as a dependency.
pub(super) fn package_count(requested: usize, dependencies: usize) -> String {
    let count = requested + dependencies;
    let total = format!("{count} {}", plural(count, "package", "packages"));
    if dependencies == 0 {
        total
    } else {
        format!(
            "{total} ({requested} requested, {dependencies} {})",
            plural(dependencies, "dependency", "dependencies")
        )
    }
}

pub(super) fn plural<'a>(count: usize, one: &'a str, many: &'a str) -> &'a str {
    if count == 1 { one } else { many }
}

/// Comma-joined items, keeping the first four and summarising the rest as
/// `, and N more` so a wide result line stays readable.
pub(super) fn limited_list(items: &[String]) -> String {
    const LIMIT: usize = 4;
    if items.len() <= LIMIT {
        return items.join(", ");
    }
    format!(
        "{}, and {} more",
        items[..LIMIT].join(", "),
        items.len() - LIMIT
    )
}

/// The `Installed ...` outcome text: each requested formula as `name version`,
/// plus a dependency count when the plan also pulled in non-requested formulae.
pub(in crate::commands) fn installed_summary(requested: &[String], dependencies: usize) -> String {
    if requested.is_empty() {
        return format!(
            "Installed {dependencies} {}",
            plural(dependencies, "package", "packages")
        );
    }
    let mut text = format!("Installed {}", limited_list(requested));
    if dependencies != 0 {
        text.push_str(&format!(
            " (+{dependencies} {})",
            plural(dependencies, "dependency", "dependencies")
        ));
    }
    text
}

/// The `Upgraded ...` outcome text, with each `name old → new` transition.
pub(in crate::commands) fn upgraded_summary(upgraded: &[String]) -> String {
    format!("Upgraded {}", limited_list(upgraded))
}

/// The `Removed ...` outcome text. Explicitly requested removals are named and
/// any autoremoved leftovers are counted; a pure autoremove names its packages.
pub(in crate::commands) fn removed_summary(requested: &[String], unused: &[String]) -> String {
    if requested.is_empty() {
        return format!("Removed {}", limited_list(unused));
    }
    let mut text = format!("Removed {}", limited_list(requested));
    if !unused.is_empty() {
        text.push_str(&format!(
            " (+{} unused {})",
            unused.len(),
            plural(unused.len(), "dependency", "dependencies")
        ));
    }
    text
}

/// Whether any `$PATH` entry names `bin`, matching by exact text or by
/// canonical path so a symlinked prefix entry still counts.
pub(super) fn path_includes(path_value: &std::ffi::OsStr, bin: &Path) -> bool {
    let canonical = fs::canonicalize(bin).ok();
    std::env::split_paths(path_value).any(|entry| {
        entry == bin
            || canonical
                .as_ref()
                .is_some_and(|bin| fs::canonicalize(&entry).is_ok_and(|entry| entry == *bin))
    })
}

/// The follow-up hint shown after an install when `$PATH` does not reach the
/// prefix's `bin`: the shellenv command that adds it.
pub(super) fn next_path_hint(
    path_value: &std::ffi::OsStr,
    bin: &Path,
    prefix: Option<&Path>,
) -> Option<String> {
    if path_includes(path_value, bin) {
        return None;
    }
    let command = match prefix {
        Some(prefix) => format!(
            "eval \"$(brewrs shellenv --prefix {})\"",
            posix_quote(&prefix.to_string_lossy())
        ),
        None => "eval \"$(brewrs shellenv)\"".to_string(),
    };
    Some(format!("{command}  (add it to ~/.zprofile to keep it)"))
}
