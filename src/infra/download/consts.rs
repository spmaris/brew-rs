/// Origin that hosts official Homebrew bottles.
pub(super) const GHCR_ORIGIN: &str = "https://ghcr.io";
/// Host component of [`GHCR_ORIGIN`].
pub(super) const GHCR_HOST: &str = "ghcr.io";
/// Fixed token endpoint used for anonymous pull tokens.
pub(super) const GHCR_TOKEN_ENDPOINT: &str = "https://ghcr.io/token";
/// Service name GHCR expects on the token endpoint.
pub(super) const GHCR_SERVICE: &str = "ghcr.io";
/// Path prefix shared by official Homebrew bottle blob URLs.
pub(super) const BOTTLE_PATH_PREFIX: &str = "/v2/homebrew/core/";
/// Isolated directory created below `storage.cache_dir`.
pub(super) use crate::infra::cache::NATIVE_BOTTLE_DIR as BOTTLE_DIR_NAME;
/// Cache file suffix; official Homebrew bottles are gzipped tarballs.
pub(super) const BOTTLE_SUFFIX: &str = ".tar.gz";
/// Longest accepted input URL.
pub(super) const MAX_URL_BYTES: usize = 2048;
/// Deepest repository path accepted after `/v2/homebrew/core/`.
pub(super) const MAX_REPOSITORY_SEGMENTS: usize = 4;
/// Redirect hops followed for a single blob request.
pub(super) const MAX_REDIRECTS: usize = 5;
/// Longest accepted redirect target, including signed CDN queries.
pub(super) const MAX_REDIRECT_URL_BYTES: usize = 8192;
/// Largest token endpoint response body that is read.
pub(super) const MAX_TOKEN_BYTES: usize = 64 * 1024;
/// Longest bearer token accepted from GHCR.
pub(super) const MAX_TOKEN_LEN: usize = 8192;
/// Largest bottle archive that will be downloaded or trusted from the cache.
pub(super) const MAX_BOTTLE_BYTES: u64 = 4 * 1024 * 1024 * 1024;
/// Read and write buffer size.
pub(super) const IO_BUFFER_BYTES: usize = 64 * 1024;
