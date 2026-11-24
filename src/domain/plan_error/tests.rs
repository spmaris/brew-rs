use super::*;

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
