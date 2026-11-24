use super::*;
use std::path::PathBuf;

use crate::domain::plan_error::{ApiFailure, from_fetch_error};

struct FakeProbe {
    builtin: Option<&'static str>,
    cask: bool,
}

impl FakeProbe {
    fn casks(cask: bool) -> Self {
        Self {
            builtin: None,
            cask,
        }
    }
}

impl HintProbe for FakeProbe {
    fn macos_builtin(&self, _: &str) -> Option<PathBuf> {
        self.builtin.map(PathBuf::from)
    }

    async fn cask_exists(&self, _: &str) -> bool {
        self.cask
    }
}

fn not_found(name: &str, tap: Option<&str>) -> anyhow::Error {
    from_fetch_error(&anyhow::Error::new(ApiFailure::NotFound), name, tap, None).into()
}

#[tokio::test]
async fn renders_builtin_and_cask_hints_without_suggestions() {
    let names = ["tailor".to_string(), "unrelated".to_string()];
    let error = add_hints(
        not_found("tail", None),
        &FakeProbe {
            builtin: Some("/usr/bin/tail"),
            cask: true,
        },
        Some(&names),
    )
    .await;
    assert_eq!(
        format!("{error:#}"),
        "No Homebrew formula named 'tail'.\n  'tail' is already part of macOS: /usr/bin/tail\n  A cask with that name exists: brewrs install --cask tail"
    );
}

#[tokio::test]
async fn suggests_similar_names_only_without_other_hints() {
    let names = ["tailor".to_string(), "unrelated".to_string()];
    let error = add_hints(
        not_found("tail", None),
        &FakeProbe::casks(false),
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
    let names = ["gtail".to_string(), "tailor".to_string()];
    let error = add_hints(
        not_found("tail", None),
        &FakeProbe::casks(false),
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
    let error = add_hints(not_found("nothing", None), &FakeProbe::casks(false), None).await;
    assert_eq!(format!("{error:#}"), "No Homebrew formula named 'nothing'.");
}

#[tokio::test]
async fn tap_qualified_target_gets_similar_name_hints() {
    let names = ["ripgrep".to_string(), "unrelated".to_string()];
    let error = add_hints(
        not_found("ripgre", Some("homebrew/core")),
        &FakeProbe::casks(true),
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
    let error = add_hints(
        not_found("x", Some("user/tap")),
        &FakeProbe::casks(true),
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
        &anyhow::Error::new(ApiFailure::NotFound),
        "dep",
        None,
        Some("parent"),
    );
    let error = add_hints(error.into(), &FakeProbe::casks(true), None).await;
    assert_eq!(
        format!("{error:#}"),
        "Homebrew metadata for 'parent' lists dependency 'dep', which does not exist."
    );
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
