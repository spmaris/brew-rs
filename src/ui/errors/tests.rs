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
