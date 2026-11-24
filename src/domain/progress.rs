//! Port for reporting streamed download progress.

/// Receives byte-level progress from a download.
///
/// `start` is called once when response headers arrive, `advance` once per
/// written chunk, and `finish` once after a successful, verified download. A
/// failed download never calls `finish`; the caller clears its own display.
pub(crate) trait DownloadProgress: Send + Sync {
    fn start(&self, total: Option<u64>);
    fn advance(&self, bytes: u64);
    fn finish(&self);
}

/// Progress sink that draws nothing.
pub(crate) struct NoProgress;

impl DownloadProgress for NoProgress {
    fn start(&self, _total: Option<u64>) {}
    fn advance(&self, _bytes: u64) {}
    fn finish(&self) {}
}
