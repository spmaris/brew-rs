//! Hint text for friendly formula lookup errors.
//!
//! The planner's typed [`PlanError`] carries the whole user-facing message in
//! its `Display`; this module only fills in the hint lines of a not-found
//! error.

use crate::domain::plan_error::{HintProbe, PlanError};

const MAX_SUGGESTIONS: usize = 3;

/// Fill in hints on a user-target [`PlanError::NotFound`]; any other error is
/// returned unchanged.
pub(crate) async fn add_hints(
    error: anyhow::Error,
    probe: &impl HintProbe,
    formula_names: Option<&[String]>,
) -> anyhow::Error {
    match error.downcast::<PlanError>() {
        Ok(PlanError::NotFound {
            name,
            tap: None,
            hints: _,
        }) => {
            let mut hints = Vec::new();
            if let Some(path) = probe.macos_builtin(&name) {
                hints.push(format!(
                    "'{name}' is already part of macOS: {}",
                    path.display()
                ));
            }
            if probe.cask_exists(&name).await {
                hints.push(format!(
                    "A cask with that name exists: brewrs install --cask {name}"
                ));
            }
            // A built-in or cask match already explains the name; fuzzy
            // suggestions on top of it are noise.
            let similar = formula_names
                .filter(|_| hints.is_empty())
                .map(|names| similar_names(&name, names));
            if let Some(similar) = similar.filter(|names| !names.is_empty()) {
                hints.push(similar_hint(&similar));
            }
            anyhow::Error::new(PlanError::NotFound {
                name,
                tap: None,
                hints,
            })
        }
        Ok(PlanError::NotFound {
            name,
            tap: Some(tap),
            hints: _,
        }) => {
            // Only homebrew/core is reachable, and the cached catalog is core,
            // so its names are the right suggestions for a tap-qualified miss.
            let hints = formula_names
                .map(|names| similar_names(&name, names))
                .filter(|similar| !similar.is_empty())
                .map(|similar| vec![similar_hint(&similar)])
                .unwrap_or_default();
            anyhow::Error::new(PlanError::NotFound {
                name,
                tap: Some(tap),
                hints,
            })
        }
        Ok(other) => anyhow::Error::new(other),
        Err(error) => error,
    }
}

/// A runnable suggestion hint: one install command for one match, an
/// extra-indented list otherwise.
fn similar_hint(names: &[String]) -> String {
    if let [only] = names {
        return format!("Did you mean: brewrs install {only}");
    }
    let mut hint = String::from("Did you mean one of:");
    for name in names {
        hint.push_str(&format!("\n    brewrs install {name}"));
    }
    hint
}

/// Up to [`MAX_SUGGESTIONS`] names that contain `name` or are
/// within a small edit distance, closest first.
fn similar_names(name: &str, names: &[String]) -> Vec<String> {
    let max_distance = (name.len() / 3).clamp(1, 2);
    let mut scored: Vec<(usize, &String)> = names
        .iter()
        .filter(|candidate| candidate.as_str() != name)
        .filter_map(|candidate| {
            let distance = edit_distance(name, candidate);
            let related = name.len() >= 3 && candidate.contains(name);
            if distance <= max_distance {
                Some((distance, candidate))
            } else {
                related.then_some((max_distance + 1, candidate))
            }
        })
        .collect();
    scored.sort_by(|a, b| (a.0, a.1.len(), a.1).cmp(&(b.0, b.1.len(), b.1)));
    scored
        .into_iter()
        .take(MAX_SUGGESTIONS)
        .map(|(_, candidate)| candidate.clone())
        .collect()
}

fn edit_distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut diagonal = row[0];
        row[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let above = row[j + 1];
            row[j + 1] = (diagonal + usize::from(ca != *cb))
                .min(above + 1)
                .min(row[j] + 1);
            diagonal = above;
        }
    }
    row[b.len()]
}

#[cfg(test)]
mod tests;
