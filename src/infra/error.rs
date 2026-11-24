//! Helpers shared by the typed errors of the infra layer.

use std::error::Error;

/// `error` and its causes joined with `": "`, the same text `anyhow` prints
/// for `{:#}`.
pub(crate) fn error_chain(error: &(dyn Error + 'static)) -> String {
    let mut text = error.to_string();
    let mut cause = error.source();
    while let Some(next) = cause {
        text.push_str(": ");
        text.push_str(&next.to_string());
        cause = next.source();
    }
    text
}
