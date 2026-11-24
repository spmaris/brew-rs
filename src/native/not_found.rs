//! Friendly errors for formula lookups against the official API.
//!
//! The planner maps raw API failures to [`PlanError`], whose `Display` is the
//! whole user-facing message (no error chain). The raw chain, with URLs, is
//! logged at debug level instead.

use std::fmt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use crate::api::http_client::{ApiFailure, HttpClient, classify};
use crate::config::Config;

const MACOS_DIRS: [&str; 4] = ["/usr/bin", "/bin", "/usr/sbin", "/sbin"];
const MAX_SUGGESTIONS: usize = 3;

#[derive(Debug)]
pub(super) enum PlanError {
    /// A user-requested formula does not exist. `tap` is set for a
    /// tap-qualified target.
    NotFound {
        name: String,
        tap: Option<String>,
        hints: Vec<String>,
    },
    /// A formula's metadata lists a dependency the API does not know.
    MissingDependency {
        parent: String,
        dependency: String,
    },
    Api(ApiFailure),
}

impl fmt::Display for PlanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound {
                name,
                tap: Some(tap),
                hints,
            } => {
                write!(f, "No formula named '{name}' in tap {tap}.")?;
                hints.iter().try_for_each(|hint| write!(f, "\n  {hint}"))
            }
            Self::NotFound {
                name,
                tap: None,
                hints,
            } => {
                write!(f, "No Homebrew formula named '{name}'.")?;
                hints.iter().try_for_each(|hint| write!(f, "\n  {hint}"))
            }
            Self::MissingDependency { parent, dependency } => write!(
                f,
                "Homebrew metadata for '{parent}' lists dependency '{dependency}', which does not exist."
            ),
            Self::Api(failure) => failure.fmt(f),
        }
    }
}

impl std::error::Error for PlanError {}

/// Convert a failed fetch of `name` into the friendly error. `parent` is the
/// formula that listed `name` as a dependency, or `None` for a user target.
/// Failures that are not not-found/network/5xx become a readable metadata
/// error; the original chain is logged at debug level.
pub(super) fn from_fetch_error(
    error: anyhow::Error,
    name: &str,
    tap: Option<&str>,
    parent: Option<&str>,
) -> anyhow::Error {
    let Some(failure) = classify(&error) else {
        log::debug!("formula lookup for '{name}' failed: {error:#}");
        return anyhow::anyhow!(
            "Could not read Homebrew metadata for '{name}'.\n  Run with BREW_RS_LOG=debug for details."
        );
    };
    log::debug!("formula lookup for '{name}' failed: {error:#}");
    anyhow::Error::new(match (failure, parent) {
        (ApiFailure::NotFound, Some(parent)) => PlanError::MissingDependency {
            parent: parent.into(),
            dependency: name.into(),
        },
        (ApiFailure::NotFound, None) => PlanError::NotFound {
            name: name.into(),
            tap: tap.map(str::to_owned),
            hints: Vec::new(),
        },
        (failure, _) => PlanError::Api(failure),
    })
}

/// Whether a cask with this token exists. Failures count as "no".
pub(super) trait CaskProbe {
    async fn cask_exists(&self, token: &str) -> bool;
}

pub(super) struct HttpCaskProbe(HttpClient);

impl HttpCaskProbe {
    pub(super) fn new(config: &Config) -> anyhow::Result<Self> {
        HttpClient::new(config).map(Self)
    }
}

impl CaskProbe for HttpCaskProbe {
    async fn cask_exists(&self, token: &str) -> bool {
        let url = format!("{}/cask/{token}.json", super::plan::API_BASE);
        self.0.fetch_json::<serde_json::Value>(&url).await.is_ok()
    }
}

/// Fill in hints on a user-target [`PlanError::NotFound`]; any other error is
/// returned unchanged.
pub(super) async fn add_hints(
    error: anyhow::Error,
    macos_dirs: &[&Path],
    casks: &impl CaskProbe,
    formula_names: Option<&[String]>,
) -> anyhow::Error {
    match error.downcast::<PlanError>() {
        Ok(PlanError::NotFound {
            name,
            tap: None,
            hints: _,
        }) => {
            let mut hints = Vec::new();
            if let Some(path) = macos_builtin(&name, macos_dirs) {
                hints.push(format!(
                    "'{name}' is already part of macOS: {}",
                    path.display()
                ));
            }
            if casks.cask_exists(&name).await {
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

/// [`add_hints`] with the real macOS directories, cask API, and cached names.
pub(super) async fn add_hints_online(error: anyhow::Error, config: &Config) -> anyhow::Error {
    let dirs: Vec<&Path> = MACOS_DIRS.iter().map(Path::new).collect();
    let Ok(probe) = HttpCaskProbe::new(config) else {
        return error;
    };
    let names = crate::api::catalog_cache::cached_formula_names(config);
    add_hints(error, &dirs, &probe, names.as_deref()).await
}

/// A regular, executable file named exactly `name` in one of `dirs`.
fn macos_builtin(name: &str, dirs: &[&Path]) -> Option<PathBuf> {
    if name.is_empty() || name == "." || name == ".." || name.contains(['/', '\\', '\0']) {
        return None;
    }
    dirs.iter().map(|dir| dir.join(name)).find(|path| {
        std::fs::metadata(path)
            .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
    })
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
mod tests {
    use super::*;

    struct FakeCasks(bool);
    impl CaskProbe for FakeCasks {
        async fn cask_exists(&self, _: &str) -> bool {
            self.0
        }
    }

    fn not_found(name: &str, tap: Option<&str>) -> anyhow::Error {
        from_fetch_error(anyhow::Error::new(ApiFailure::NotFound), name, tap, None)
    }

    fn executable(dir: &Path, name: &str, mode: u32) {
        let path = dir.join(name);
        std::fs::write(&path, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    #[tokio::test]
    async fn renders_builtin_and_cask_hints_without_suggestions() {
        let dir = tempfile::tempdir().unwrap();
        executable(dir.path(), "tail", 0o755);
        let names = ["tailor".to_string(), "unrelated".to_string()];
        let error = add_hints(
            not_found("tail", None),
            &[dir.path()],
            &FakeCasks(true),
            Some(&names),
        )
        .await;
        assert_eq!(
            format!("{error:#}"),
            format!(
                "No Homebrew formula named 'tail'.\n  'tail' is already part of macOS: {}/tail\n  A cask with that name exists: brewrs install --cask tail",
                dir.path().display()
            )
        );
    }

    #[tokio::test]
    async fn suggests_similar_names_only_without_other_hints() {
        let dir = tempfile::tempdir().unwrap();
        let names = ["tailor".to_string(), "unrelated".to_string()];
        let error = add_hints(
            not_found("tail", None),
            &[dir.path()],
            &FakeCasks(false),
            Some(&names),
        )
        .await;
        assert_eq!(
            format!("{error:#}"),
            "No Homebrew formula named 'tail'.\n  Did you mean: brewrs install tailor"
        );
    }

    #[tokio::test]
    async fn several_similar_names_become_a_runnable_list() {
        let dir = tempfile::tempdir().unwrap();
        let names = ["gtail".to_string(), "tailor".to_string()];
        let error = add_hints(
            not_found("tail", None),
            &[dir.path()],
            &FakeCasks(false),
            Some(&names),
        )
        .await;
        assert_eq!(
            format!("{error:#}"),
            "No Homebrew formula named 'tail'.\n  Did you mean one of:\n    brewrs install gtail\n    brewrs install tailor"
        );
    }

    #[tokio::test]
    async fn no_hints_when_nothing_applies() {
        let dir = tempfile::tempdir().unwrap();
        let error = add_hints(
            not_found("nothing", None),
            &[dir.path()],
            &FakeCasks(false),
            None,
        )
        .await;
        assert_eq!(format!("{error:#}"), "No Homebrew formula named 'nothing'.");
    }

    #[tokio::test]
    async fn tap_qualified_target_gets_similar_name_hints() {
        let dir = tempfile::tempdir().unwrap();
        let names = ["ripgrep".to_string(), "unrelated".to_string()];
        let error = add_hints(
            not_found("ripgre", Some("homebrew/core")),
            &[dir.path()],
            &FakeCasks(true),
            Some(&names),
        )
        .await;
        assert_eq!(
            format!("{error:#}"),
            "No formula named 'ripgre' in tap homebrew/core.\n  Did you mean: brewrs install ripgrep"
        );
    }

    #[tokio::test]
    async fn tap_qualified_target_without_catalog_names_stays_bare() {
        let dir = tempfile::tempdir().unwrap();
        let error = add_hints(
            not_found("x", Some("user/tap")),
            &[dir.path()],
            &FakeCasks(true),
            None,
        )
        .await;
        assert_eq!(
            format!("{error:#}"),
            "No formula named 'x' in tap user/tap."
        );
    }

    #[tokio::test]
    async fn dependency_not_found_has_no_hints() {
        let error = from_fetch_error(
            anyhow::Error::new(ApiFailure::NotFound),
            "dep",
            None,
            Some("parent"),
        );
        let dir = tempfile::tempdir().unwrap();
        let error = add_hints(error, &[dir.path()], &FakeCasks(true), None).await;
        assert_eq!(
            format!("{error:#}"),
            "Homebrew metadata for 'parent' lists dependency 'dep', which does not exist."
        );
    }

    #[test]
    fn network_and_server_failures_are_friendly() {
        let error = from_fetch_error(
            anyhow::Error::new(ApiFailure::Unreachable).context("Failed to send request"),
            "x",
            None,
            None,
        );
        assert_eq!(
            format!("{error:#}"),
            "Could not reach formulae.brew.sh. Check your network connection."
        );
        let error = from_fetch_error(anyhow::Error::new(ApiFailure::Server(502)), "x", None, None);
        assert_eq!(
            format!("{error:#}"),
            "formulae.brew.sh returned an error (HTTP 502). Try again later."
        );
    }

    #[test]
    fn other_failures_become_a_readable_metadata_error() {
        let error = from_fetch_error(anyhow::anyhow!("bad json"), "x", None, None);
        assert_eq!(
            format!("{error:#}"),
            "Could not read Homebrew metadata for 'x'.\n  Run with BREW_RS_LOG=debug for details."
        );
    }

    #[test]
    fn macos_builtin_requires_plain_executable_files() {
        let dir = tempfile::tempdir().unwrap();
        executable(dir.path(), "tool", 0o755);
        executable(dir.path(), "data", 0o644);
        std::fs::create_dir(dir.path().join("folder")).unwrap();
        let dirs = [dir.path()];
        assert!(macos_builtin("tool", &dirs).is_some());
        assert!(macos_builtin("data", &dirs).is_none());
        assert!(macos_builtin("folder", &dirs).is_none());
        assert!(macos_builtin("missing", &dirs).is_none());
        assert!(macos_builtin("../tool", &dirs).is_none());
        assert!(macos_builtin("..", &dirs).is_none());
        assert!(macos_builtin("", &dirs).is_none());
    }

    #[test]
    fn suggests_close_names_only() {
        let names: Vec<String> = ["wget", "wget2", "curl", "tailor", "gtail", "w"]
            .map(String::from)
            .into();
        assert_eq!(similar_names("wgat", &names), ["wget"]);
        assert_eq!(similar_names("tail", &names), ["gtail", "tailor"]);
        assert!(similar_names("zzzzzz", &names).is_empty());
        assert!(similar_names("curl", &names).is_empty());
    }
}
