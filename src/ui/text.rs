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
