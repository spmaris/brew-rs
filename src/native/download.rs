//! Staged, checksum-verified fetch of official Homebrew bottles from GHCR.
//!
//! [`fetch`] is deliberately narrow. It only accepts lowercase SHA-256 digests
//! and `https://ghcr.io/v2/homebrew/core/.../blobs/sha256:<digest>` URLs, and it
//! only ever writes into `<storage.cache_dir>/native-bottles`. Archives are
//! streamed to a private staging file, hashed while they are written, verified
//! against the expected digest, and published with an atomic rename. Nothing is
//! unpacked, executed, or registered with Homebrew, and no global state is
//! touched.
//!
//! Cached files are re-verified before every reuse: a corrupt or incomplete
//! regular file is discarded and fetched again, while anything that is not a
//! regular file (notably a symlink) is refused instead of followed.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use reqwest::header::{ACCEPT, LOCATION};
use reqwest::{Client, Response, StatusCode};
use sha2::{Digest, Sha256};

use crate::config::Config;

/// Origin that hosts official Homebrew bottles.
const GHCR_ORIGIN: &str = "https://ghcr.io";
/// Host component of [`GHCR_ORIGIN`].
const GHCR_HOST: &str = "ghcr.io";
/// Fixed token endpoint used for anonymous pull tokens.
const GHCR_TOKEN_ENDPOINT: &str = "https://ghcr.io/token";
/// Service name GHCR expects on the token endpoint.
const GHCR_SERVICE: &str = "ghcr.io";
/// Path prefix shared by official Homebrew bottle blob URLs.
const BOTTLE_PATH_PREFIX: &str = "/v2/homebrew/core/";
/// Isolated directory created below `storage.cache_dir`.
use crate::cache::NATIVE_BOTTLE_DIR as BOTTLE_DIR_NAME;
/// Cache file suffix; official Homebrew bottles are gzipped tarballs.
const BOTTLE_SUFFIX: &str = ".tar.gz";
/// Longest accepted input URL.
const MAX_URL_BYTES: usize = 2048;
/// Deepest repository path accepted after `/v2/homebrew/core/`.
const MAX_REPOSITORY_SEGMENTS: usize = 4;
/// Redirect hops followed for a single blob request.
const MAX_REDIRECTS: usize = 5;
/// Longest accepted redirect target, including signed CDN queries.
const MAX_REDIRECT_URL_BYTES: usize = 8192;
/// Largest token endpoint response body that is read.
const MAX_TOKEN_BYTES: usize = 64 * 1024;
/// Longest bearer token accepted from GHCR.
const MAX_TOKEN_LEN: usize = 8192;
/// Largest bottle archive that will be downloaded or trusted from the cache.
const MAX_BOTTLE_BYTES: u64 = 4 * 1024 * 1024 * 1024;
/// Read and write buffer size.
const IO_BUFFER_BYTES: usize = 64 * 1024;

/// Fetch an official Homebrew bottle into the native bottle cache.
///
/// The archive is written atomically to
/// `<storage.cache_dir>/native-bottles/<sha256>.tar.gz` after its digest has
/// been verified while streaming. A previously cached file is reused only when
/// it still matches `sha256`; a mismatched regular file is discarded and
/// downloaded again. Nothing is unpacked or installed.
///
/// `url` must be an `https://ghcr.io/v2/homebrew/core/.../blobs/sha256:<digest>`
/// URL with no credentials, port, query, fragment, percent-encoding, or path
/// traversal segments, and `sha256` must be exactly 64 lowercase hexadecimal
/// characters equal to the digest embedded in `url`.
pub async fn fetch(config: &Config, url: &str, sha256: &str) -> Result<PathBuf> {
    let source = parse_bottle_url(url, sha256)?;
    let directory = bottle_cache_dir(config)?;
    let destination = directory.join(format!("{}{BOTTLE_SUFFIX}", source.digest));

    // Validate the cache entry before creating a client so a verified file can
    // be served entirely offline.
    if let Some(cached) = reuse_cached_bottle(&destination, &source.digest)? {
        return Ok(cached);
    }

    let client = build_client(config)?;
    let response = request_bottle(&client, &source, config).await?;
    stage_bottle(&directory, &destination, &source.digest, response, config).await?;
    Ok(destination)
}

/// A validated bottle location.
struct BottleSource {
    /// Registry repository, for example `homebrew/core/wget`.
    repository: String,
    /// Lowercase SHA-256 digest from the URL and the caller.
    digest: String,
    /// Canonical URL rebuilt from validated components.
    url: String,
}

impl BottleSource {
    /// Minimal pull scope needed for exactly this repository.
    fn pull_scope(&self) -> String {
        format!("repository:{}:pull", self.repository)
    }
}

/// Parse and strictly validate a GHCR bottle blob URL.
fn parse_bottle_url(url: &str, sha256: &str) -> Result<BottleSource> {
    if !is_lower_hex_digest(sha256) {
        bail!("refusing to fetch a bottle: sha256 must be 64 lowercase hexadecimal characters");
    }
    if url.len() > MAX_URL_BYTES {
        bail!("refusing to fetch a bottle: URL exceeds {MAX_URL_BYTES} bytes");
    }
    if url.chars().any(|c| c.is_control() || c.is_whitespace()) {
        bail!("refusing to fetch a bottle: URL contains whitespace or control characters");
    }

    // Requiring this exact prefix also rejects ports, userinfo, trailing-dot
    // hosts, and look-alike hosts such as `ghcr.io.evil.example`.
    let Some(path) = url.strip_prefix(GHCR_ORIGIN) else {
        bail!("refusing to fetch a bottle: URL must start with {GHCR_ORIGIN}/");
    };
    if !path.starts_with('/') {
        bail!("refusing to fetch a bottle: URL must be on the {GHCR_HOST} origin");
    }
    if path.contains('?') || path.contains('#') {
        bail!("refusing to fetch a bottle: URL must not contain a query or fragment");
    }
    if path.contains('%') {
        bail!("refusing to fetch a bottle: URL must not contain percent-encoding");
    }
    if path.contains('\\') {
        bail!("refusing to fetch a bottle: URL must not contain backslashes");
    }

    let Some(rest) = path.strip_prefix(BOTTLE_PATH_PREFIX) else {
        bail!("refusing to fetch a bottle: expected a {BOTTLE_PATH_PREFIX}... path");
    };
    let segments: Vec<&str> = rest.split('/').collect();
    if segments.len() < 3 {
        bail!("refusing to fetch a bottle: expected <repository>/blobs/sha256:<digest>");
    }

    let digest_segment = segments[segments.len() - 1];
    let blob_segment = segments[segments.len() - 2];
    if blob_segment != "blobs" {
        bail!("refusing to fetch a bottle: URL is not a blob download path");
    }
    let Some(url_digest) = digest_segment.strip_prefix("sha256:") else {
        bail!("refusing to fetch a bottle: blob path must end in sha256:<digest>");
    };
    if url_digest != sha256 {
        bail!("refusing to fetch a bottle: URL digest does not match the requested sha256");
    }

    let repository_segments = &segments[..segments.len() - 2];
    if repository_segments.is_empty() || repository_segments.len() > MAX_REPOSITORY_SEGMENTS {
        bail!("refusing to fetch a bottle: unexpected repository path depth");
    }
    for segment in repository_segments {
        if !is_valid_repository_segment(segment) {
            bail!("refusing to fetch a bottle: invalid repository segment '{segment}'");
        }
    }

    let digest = sha256.to_owned();
    let repository_path = repository_segments.join("/");
    Ok(BottleSource {
        repository: format!("homebrew/core/{repository_path}"),
        url: format!("{GHCR_ORIGIN}{BOTTLE_PATH_PREFIX}{repository_path}/blobs/sha256:{digest}"),
        digest,
    })
}

/// Whether `value` is exactly 64 lowercase hexadecimal characters.
fn is_lower_hex_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// Whether `segment` is one safe, lowercase registry path component.
fn is_valid_repository_segment(segment: &str) -> bool {
    if segment.is_empty() || segment.len() > 64 || segment == "." || segment == ".." {
        return false;
    }
    let mut characters = segment.chars();
    let Some(first) = characters.next() else {
        return false;
    };
    if !first.is_ascii_lowercase() && !first.is_ascii_digit() {
        return false;
    }
    characters.all(|character| {
        character.is_ascii_lowercase()
            || character.is_ascii_digit()
            || matches!(character, '.' | '_' | '-' | '+' | '@')
    })
}

/// Create and validate the isolated native bottle cache directory.
fn bottle_cache_dir(config: &Config) -> Result<PathBuf> {
    let directory = config.storage.cache_dir.join(BOTTLE_DIR_NAME);

    match fs::symlink_metadata(&directory) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() {
                bail!(
                    "refusing to use '{}': the native bottle directory is a symlink",
                    directory.display()
                );
            }
            if !metadata.is_dir() {
                bail!(
                    "refusing to use '{}': the native bottle path is not a directory",
                    directory.display()
                );
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            fs::create_dir_all(&directory).with_context(|| {
                format!(
                    "failed to create the native bottle cache directory: {}",
                    directory.display()
                )
            })?;
            let metadata = fs::symlink_metadata(&directory).with_context(|| {
                format!(
                    "failed to inspect the native bottle cache directory: {}",
                    directory.display()
                )
            })?;
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                bail!(
                    "refusing to use '{}': the native bottle directory is not a real directory",
                    directory.display()
                );
            }
        }
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "failed to inspect the native bottle cache directory: {}",
                    directory.display()
                )
            });
        }
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).with_context(|| {
            format!(
                "failed to restrict permissions on the native bottle cache directory: {}",
                directory.display()
            )
        })?;
    }

    Ok(directory)
}

/// Reuse `path` only when it is a regular file whose digest still matches.
///
/// Returns `Ok(None)` when the path is absent or held a mismatched regular
/// file that was discarded. Anything that is not a regular file is an error so
/// a symlink can never be followed out of the cache.
fn reuse_cached_bottle(path: &Path, digest: &str) -> Result<Option<PathBuf>> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed to inspect cached bottle: {}", path.display()));
        }
    };

    if metadata.file_type().is_symlink() {
        bail!(
            "refusing to reuse '{}': the cached bottle is a symlink",
            path.display()
        );
    }
    if !metadata.is_file() {
        bail!(
            "refusing to reuse '{}': the cached bottle is not a regular file",
            path.display()
        );
    }

    match verify_file_digest(path, digest) {
        Ok(()) => Ok(Some(path.to_path_buf())),
        Err(error) => {
            // The entry is a regular file inside our dedicated directory, so
            // discarding it is safe. A file that cannot be verified must never
            // be treated as complete.
            match fs::remove_file(path) {
                Ok(()) => {
                    log::debug!(
                        "discarded unverified cached bottle {}: {error:#}",
                        path.display()
                    );
                }
                Err(remove_error) if remove_error.kind() == io::ErrorKind::NotFound => {}
                Err(remove_error) => {
                    return Err(remove_error).with_context(|| {
                        format!(
                            "failed to discard an unverified cached bottle: {}",
                            path.display()
                        )
                    });
                }
            }
            Ok(None)
        }
    }
}

/// Stream `path` through SHA-256 and compare it with `expected`.
fn verify_file_digest(path: &Path, expected: &str) -> Result<()> {
    verify_file_digest_bounded(path, expected, MAX_BOTTLE_BYTES)
}

/// [`verify_file_digest`] with an explicit size limit.
fn verify_file_digest_bounded(path: &Path, expected: &str, limit: u64) -> Result<()> {
    let mut file = File::open(path)
        .with_context(|| format!("failed to open cached bottle: {}", path.display()))?;
    let metadata = file
        .metadata()
        .with_context(|| format!("failed to inspect cached bottle: {}", path.display()))?;
    if !metadata.is_file() {
        bail!(
            "refusing to reuse '{}': the cached bottle is not a regular file",
            path.display()
        );
    }
    if metadata.len() > limit {
        bail!(
            "refusing to reuse '{}': the cached bottle is {} bytes, above the {limit} byte limit",
            path.display(),
            metadata.len()
        );
    }

    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; IO_BUFFER_BYTES];
    let mut total = 0_u64;
    loop {
        let read = file
            .read(&mut buffer)
            .with_context(|| format!("failed to read cached bottle: {}", path.display()))?;
        if read == 0 {
            break;
        }
        total = total
            .checked_add(read as u64)
            .context("cached bottle size overflowed")?;
        if total > limit {
            bail!(
                "refusing to reuse '{}': the cached bottle exceeds the {limit} byte limit",
                path.display()
            );
        }
        hasher.update(&buffer[..read]);
    }

    let actual = hex::encode(hasher.finalize());
    if actual != expected {
        bail!(
            "SHA-256 mismatch for '{}': expected {expected}, got {actual}",
            path.display()
        );
    }
    Ok(())
}

/// Build the HTTP client used for GHCR requests.
fn build_client(config: &Config) -> Result<Client> {
    let mut builder = Client::builder()
        // Redirects are followed manually so a bearer token can never be
        // forwarded to a CDN host and every hop is validated.
        .redirect(reqwest::redirect::Policy::none())
        .https_only(true)
        .connect_timeout(request_timeout(config));

    if let Some(user_agent) = config.downloads.user_agent.as_deref() {
        builder = builder.user_agent(user_agent);
    }
    if !config.downloads.use_proxy {
        builder = builder.no_proxy();
    }

    builder
        .build()
        .context("failed to build the bottle HTTP client")
}

/// Download the bottle blob, handling a public GHCR pull token and CDN redirects.
async fn request_bottle(
    client: &Client,
    source: &BottleSource,
    config: &Config,
) -> Result<Response> {
    let mut url = source.url.clone();
    let mut redirects = 0_usize;
    let mut token: Option<String> = None;
    let mut token_attempted = false;

    loop {
        let mut request = client.get(&url).header(ACCEPT, "application/octet-stream");
        if let Some(bearer) = token.as_deref() {
            // Defensive in addition to redirect validation: a token is only
            // ever attached to a request addressed to the GHCR origin.
            if is_ghcr_url(&url) {
                request = request.bearer_auth(bearer);
            }
        }

        let response = tokio::time::timeout(request_timeout(config), request.send())
            .await
            .with_context(|| format!("timed out requesting bottle from {url}"))?
            .with_context(|| format!("failed to request bottle from {url}"))?;

        match response.status() {
            StatusCode::OK => return Ok(response),
            StatusCode::UNAUTHORIZED => {
                if token_attempted || !is_ghcr_url(&url) {
                    bail!("GHCR rejected the bottle request with HTTP 401 Unauthorized");
                }
                token_attempted = true;
                // The fixed endpoint and the minimal repository scope are used
                // instead of the challenge's realm, which is attacker-influenced.
                token = Some(request_pull_token(client, &source.pull_scope(), config).await?);
                // Retry the same URL with the token; this is not a redirect.
            }
            status
                if matches!(
                    status,
                    StatusCode::MOVED_PERMANENTLY
                        | StatusCode::FOUND
                        | StatusCode::SEE_OTHER
                        | StatusCode::TEMPORARY_REDIRECT
                        | StatusCode::PERMANENT_REDIRECT
                ) =>
            {
                redirects += 1;
                if redirects > MAX_REDIRECTS {
                    bail!("refusing to follow more than {MAX_REDIRECTS} bottle redirects");
                }
                let location = response
                    .headers()
                    .get(LOCATION)
                    .and_then(|value| value.to_str().ok())
                    .context("bottle redirect did not include a valid Location header")?;
                url = resolve_redirect(&url, location)?;
            }
            status => bail!("bottle download failed with HTTP {status}"),
        }
    }
}

/// Whether `url` targets the GHCR origin.
fn is_ghcr_url(url: &str) -> bool {
    url.starts_with("https://ghcr.io/")
}

/// Resolve and validate one redirect hop.
fn resolve_redirect(base: &str, location: &str) -> Result<String> {
    if location.len() > MAX_REDIRECT_URL_BYTES {
        bail!("refusing to follow a bottle redirect: Location header is too long");
    }
    let base = reqwest::Url::parse(base).context("failed to parse the bottle request URL")?;
    let target = base
        .join(location)
        .with_context(|| format!("bottle redirect has an invalid Location header: {location}"))?;
    if target.fragment().is_some() {
        bail!("refusing to follow a bottle redirect with a fragment");
    }
    if !is_allowed_redirect(&target) {
        bail!(
            "refusing to follow a bottle redirect to '{}': not an allowed GitHub container host",
            target
        );
    }
    Ok(target.to_string())
}

/// Whether a redirect target is an allowed, credential-free GitHub host.
fn is_allowed_redirect(url: &reqwest::Url) -> bool {
    if url.scheme() != "https" {
        return false;
    }
    // An explicit port in the original URL means `Url` keeps it; default ports
    // are normalized away by the parser.
    if url.port().is_some() {
        return false;
    }
    if !url.username().is_empty() || url.password().is_some() {
        return false;
    }
    let Some(host) = url.host_str() else {
        return false;
    };
    host == GHCR_HOST || host.ends_with(".githubusercontent.com")
}

/// Request an anonymous pull token with the fixed endpoint and minimal scope.
async fn request_pull_token(client: &Client, scope: &str, config: &Config) -> Result<String> {
    let mut url = reqwest::Url::parse(GHCR_TOKEN_ENDPOINT)
        .context("failed to parse the built-in GHCR token endpoint")?;
    url.query_pairs_mut()
        .append_pair("service", GHCR_SERVICE)
        .append_pair("scope", scope);

    let response = tokio::time::timeout(
        request_timeout(config),
        client.get(url).header(ACCEPT, "application/json").send(),
    )
    .await
    .context("timed out requesting a GHCR pull token")?
    .context("failed to request a GHCR pull token")?;

    if response.status() != StatusCode::OK {
        bail!(
            "GHCR pull token request failed with HTTP {}",
            response.status()
        );
    }

    let mut response = response;
    let mut body = Vec::new();
    loop {
        let chunk = tokio::time::timeout(chunk_timeout(config), response.chunk())
            .await
            .context("timed out reading the GHCR pull token response")?
            .context("failed to read the GHCR pull token response")?;
        let Some(chunk) = chunk else {
            break;
        };
        if body.len() + chunk.len() > MAX_TOKEN_BYTES {
            bail!("GHCR pull token response exceeded {MAX_TOKEN_BYTES} bytes");
        }
        body.extend_from_slice(&chunk);
    }

    #[derive(serde::Deserialize)]
    struct TokenResponse {
        #[serde(default)]
        token: Option<String>,
        #[serde(default)]
        access_token: Option<String>,
    }

    let payload: TokenResponse =
        serde_json::from_slice(&body).context("failed to parse the GHCR pull token response")?;
    let token = payload
        .token
        .or(payload.access_token)
        .context("GHCR pull token response contained no token")?;

    // Keep the value header-safe: no whitespace, separators, or control bytes.
    if token.is_empty()
        || token.len() > MAX_TOKEN_LEN
        || !token.bytes().all(|b| b.is_ascii_graphic())
    {
        bail!("GHCR returned an unusable pull token");
    }
    Ok(token)
}

/// Stream the response into a staging file, hashing and verifying as it goes.
async fn stage_bottle(
    directory: &Path,
    destination: &Path,
    digest: &str,
    response: Response,
    config: &Config,
) -> Result<()> {
    if let Some(length) = response.content_length() {
        if length > MAX_BOTTLE_BYTES {
            bail!("refusing to download a bottle larger than the {MAX_BOTTLE_BYTES} byte limit");
        }
    }
    ensure_destination_is_not_symlink(destination)?;

    let mut staged = StagedBottle::create(directory, digest)?;
    let mut hasher = Sha256::new();
    let mut response = response;
    let mut total = 0_u64;

    loop {
        let chunk = tokio::time::timeout(chunk_timeout(config), response.chunk())
            .await
            .context("timed out while streaming the bottle")?
            .context("failed while streaming the bottle")?;
        let Some(chunk) = chunk else {
            break;
        };
        total = total
            .checked_add(chunk.len() as u64)
            .context("bottle size overflowed")?;
        if total > MAX_BOTTLE_BYTES {
            bail!("bottle download exceeded the {MAX_BOTTLE_BYTES} byte limit");
        }
        hasher.update(&chunk);
        staged
            .write_all(&chunk)
            .context("failed to write the staged bottle")?;
    }

    if total == 0 {
        bail!("bottle download produced an empty file");
    }
    let actual = hex::encode(hasher.finalize());
    if actual != digest {
        bail!("SHA-256 mismatch for the downloaded bottle: expected {digest}, got {actual}");
    }

    staged.commit(destination)?;
    sync_directory(directory);
    Ok(())
}

/// A uniquely named staging file that removes itself unless committed.
struct StagedBottle {
    path: PathBuf,
    file: Option<File>,
    keep: bool,
}

impl StagedBottle {
    /// Create a new private staging file in `directory`.
    fn create(directory: &Path, digest: &str) -> Result<Self> {
        static COUNTER: AtomicU64 = AtomicU64::new(0);

        for _ in 0..16 {
            let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos();
            let path = directory.join(format!(
                ".{digest}.{}.{nanos}.{unique}.tmp",
                std::process::id()
            ));

            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }

            match options.open(&path) {
                Ok(file) => {
                    return Ok(Self {
                        path,
                        file: Some(file),
                        keep: false,
                    });
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!("failed to create a staging file in {}", directory.display())
                    });
                }
            }
        }

        bail!(
            "failed to create a unique staging file in {}",
            directory.display()
        )
    }

    /// Append streamed bytes to the staging file.
    fn write_all(&mut self, chunk: &[u8]) -> io::Result<()> {
        match self.file.as_mut() {
            Some(file) => file.write_all(chunk),
            None => Err(io::Error::other("staged bottle file is closed")),
        }
    }

    /// Flush the staging file and atomically publish it at `destination`.
    fn commit(mut self, destination: &Path) -> Result<()> {
        if let Some(file) = self.file.take() {
            file.sync_all().with_context(|| {
                format!("failed to flush the staged bottle: {}", self.path.display())
            })?;
        }
        ensure_destination_is_not_symlink(destination)?;
        fs::rename(&self.path, destination).with_context(|| {
            format!("failed to publish the bottle to {}", destination.display())
        })?;
        // The rename consumed the staging path; the drop guard must not unlink
        // the published file.
        self.keep = true;
        Ok(())
    }
}

impl Drop for StagedBottle {
    fn drop(&mut self) {
        if self.keep {
            return;
        }
        drop(self.file.take());
        match fs::remove_file(&self.path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                log::debug!(
                    "failed to remove the staged bottle {}: {error}",
                    self.path.display()
                );
            }
        }
    }
}

/// Refuse to overwrite a symlink at a cache destination.
fn ensure_destination_is_not_symlink(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => bail!(
            "refusing to replace '{}': the destination is a symlink",
            path.display()
        ),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("failed to inspect '{}'", path.display())),
    }
}

/// Best-effort directory flush after an atomic publish.
fn sync_directory(directory: &Path) {
    #[cfg(unix)]
    if let Ok(handle) = File::open(directory) {
        if let Err(error) = handle.sync_all() {
            log::debug!(
                "failed to flush the native bottle directory {}: {error}",
                directory.display()
            );
        }
    }
    #[cfg(not(unix))]
    let _ = directory;
}

/// Timeout for connection setup and response headers.
fn request_timeout(config: &Config) -> Duration {
    Duration::from_secs(config.downloads.timeout.clamp(1, 300))
}

/// Idle timeout for a single streamed chunk.
fn chunk_timeout(config: &Config) -> Duration {
    request_timeout(config)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;
    use tempfile::tempdir;

    /// SHA-256 of `FIXTURE`.
    const DIGEST: &str = "a948904f2f0f479b8f8197694b30184b0d2ed1c1cd2a1ec0fb85d299a192a447";
    /// Offline bottle stand-in; the digest above is of these exact bytes.
    const FIXTURE: &[u8] = b"hello world\n";

    fn bottle_url(digest: &str) -> String {
        format!("https://ghcr.io/v2/homebrew/core/wget/blobs/sha256:{digest}")
    }

    fn test_config(cache_dir: &Path) -> Config {
        let mut config = Config::default();
        config.storage.cache_dir = cache_dir.to_path_buf();
        config
    }

    fn parse(value: &str) -> reqwest::Url {
        reqwest::Url::parse(value).expect("test URL should parse")
    }

    #[test]
    fn accepts_official_bottle_url() {
        let source = parse_bottle_url(&bottle_url(DIGEST), DIGEST).expect("valid bottle URL");
        assert_eq!(source.digest, DIGEST);
        assert_eq!(source.repository, "homebrew/core/wget");
        assert_eq!(source.url, bottle_url(DIGEST));
        assert_eq!(source.pull_scope(), "repository:homebrew/core/wget:pull");
    }

    #[test]
    fn accepts_versioned_repository_paths() {
        let url = format!("https://ghcr.io/v2/homebrew/core/python/3.12/blobs/sha256:{DIGEST}");
        let source = parse_bottle_url(&url, DIGEST).expect("versioned bottle URL");
        assert_eq!(source.repository, "homebrew/core/python/3.12");
        assert_eq!(
            source.pull_scope(),
            "repository:homebrew/core/python/3.12:pull"
        );
    }

    #[test]
    fn rejects_unsafe_or_unrelated_urls() {
        let url = bottle_url(DIGEST);
        let cases = [
            ("http scheme", url.replacen("https://", "http://", 1)),
            (
                "other host",
                url.replacen("ghcr.io", "ghcr.io.evil.example", 1),
            ),
            ("explicit port", url.replacen("ghcr.io", "ghcr.io:443", 1)),
            (
                "userinfo",
                url.replacen("ghcr.io", "user:password@ghcr.io", 1),
            ),
            ("query", format!("{url}?scope=everything")),
            ("fragment", format!("{url}#fragment")),
            (
                "path traversal",
                url.replacen("core/wget", "core/../wget", 1),
            ),
            (
                "encoded traversal",
                url.replacen("core/wget", "core/%2e%2e/wget", 1),
            ),
            ("backslash", url.replacen("core/wget", "core\\wget", 1)),
            ("empty segment", url.replacen("core/wget", "core//wget", 1)),
            (
                "wrong repository prefix",
                url.replacen("/v2/homebrew/core/", "/v2/homebrew/cask/", 1),
            ),
            ("not a blob path", url.replacen("/blobs/", "/manifests/", 1)),
            ("missing digest prefix", url.replacen("sha256:", "", 1)),
            (
                "uppercase digest in URL",
                url.replacen(DIGEST, &DIGEST.to_uppercase(), 1),
            ),
            ("trailing path", format!("{url}/extra")),
            ("whitespace", format!(" {url}")),
            ("empty string", String::new()),
        ];

        for (name, candidate) in cases {
            assert!(
                parse_bottle_url(&candidate, DIGEST).is_err(),
                "unexpectedly accepted {name}: {candidate}"
            );
        }
    }

    #[test]
    fn rejects_invalid_digests() {
        let uppercase = DIGEST.to_uppercase();
        assert!(parse_bottle_url(&bottle_url(&uppercase), &uppercase).is_err());

        let short = &DIGEST[..63];
        let short_url = format!("https://ghcr.io/v2/homebrew/core/wget/blobs/sha256:{short}");
        assert!(parse_bottle_url(&short_url, short).is_err());

        assert!(parse_bottle_url(&bottle_url(DIGEST), &uppercase).is_err());
        assert!(parse_bottle_url(&bottle_url(DIGEST), "").is_err());
        assert!(parse_bottle_url(&bottle_url(DIGEST), &DIGEST.replace('a', "g")).is_err());
    }

    #[test]
    fn rejects_digest_that_differs_from_the_url() {
        let other = "b".repeat(64);
        assert!(parse_bottle_url(&bottle_url(DIGEST), &other).is_err());
    }

    #[test]
    fn redirects_are_limited_to_github_container_hosts() {
        let allowed = [
            "https://ghcr.io/v2/homebrew/core/wget/blobs/sha256:abc",
            "https://pkg-containers.githubusercontent.com/ghcr01/blob/abc?sig=xyz",
            "https://objects.githubusercontent.com/some/object",
        ];
        for candidate in allowed {
            assert!(
                is_allowed_redirect(&parse(candidate)),
                "should allow redirect to {candidate}"
            );
        }

        let denied = [
            "http://pkg-containers.githubusercontent.com/ghcr01/blob/abc",
            "https://evil.example/blob",
            "https://githubusercontent.com/blob",
            "https://githubusercontent.com.evil.example/blob",
            "https://pkg-containers.githubusercontent.com:8443/blob",
            "https://user:pass@pkg-containers.githubusercontent.com/blob",
            "https://127.0.0.1/blob",
            "https://ghcr.io./blob",
        ];
        for candidate in denied {
            assert!(
                !is_allowed_redirect(&parse(candidate)),
                "should refuse redirect to {candidate}"
            );
        }
    }

    #[test]
    fn resolves_relative_redirects_on_ghcr_only() {
        let base = bottle_url(DIGEST);
        let next = resolve_redirect(&base, "/v2/homebrew/core/wget/blobs/sha256:abc")
            .expect("relative redirect on GHCR");
        assert_eq!(
            next,
            "https://ghcr.io/v2/homebrew/core/wget/blobs/sha256:abc"
        );
        assert!(resolve_redirect(&base, "https://evil.example/steal").is_err());
        assert!(resolve_redirect(&base, "http://ghcr.io/v2/steal").is_err());
    }

    #[test]
    fn token_is_only_attached_to_ghcr_urls() {
        assert!(is_ghcr_url("https://ghcr.io/v2/homebrew/core/wget/blobs/x"));
        assert!(!is_ghcr_url("https://ghcr.io.evil.example/v2/x"));
        assert!(!is_ghcr_url("http://ghcr.io/v2/x"));
        assert!(!is_ghcr_url(
            "https://pkg-containers.githubusercontent.com/ghcr01/blob/x"
        ));
    }

    #[test]
    fn creates_private_isolated_bottle_directory() {
        let temp = tempdir().expect("temporary directory");
        let cache_dir = temp.path().join("cache");
        let directory =
            bottle_cache_dir(&test_config(&cache_dir)).expect("create native bottle directory");

        assert_eq!(directory, cache_dir.join(BOTTLE_DIR_NAME));
        assert!(directory.is_dir());
        assert!(
            !fs::symlink_metadata(&directory)
                .expect("directory metadata")
                .file_type()
                .is_symlink()
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&directory)
                    .expect("directory metadata")
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
        }

        // A second call is idempotent.
        assert_eq!(
            bottle_cache_dir(&test_config(&cache_dir)).expect("reuse directory"),
            directory
        );
    }

    #[test]
    fn reuses_only_a_verified_cached_bottle() {
        let temp = tempdir().expect("temporary directory");
        let path = temp.path().join(format!("{DIGEST}{BOTTLE_SUFFIX}"));
        fs::write(&path, FIXTURE).expect("write fixture");

        assert!(verify_file_digest(&path, DIGEST).is_ok());
        let reused = reuse_cached_bottle(&path, DIGEST).expect("verify cache entry");
        assert_eq!(reused.as_deref(), Some(path.as_path()));
        assert!(path.exists());
    }

    #[test]
    fn discards_a_cached_bottle_that_fails_verification() {
        let temp = tempdir().expect("temporary directory");
        let path = temp.path().join(format!("{DIGEST}{BOTTLE_SUFFIX}"));
        fs::write(&path, b"corrupt").expect("write corrupt fixture");

        let reused = reuse_cached_bottle(&path, DIGEST).expect("discard corrupt cache entry");
        assert!(reused.is_none());
        assert!(!path.exists(), "an unverified cached file must be removed");
    }

    #[test]
    fn rejects_a_cached_bottle_above_the_size_limit() {
        let temp = tempdir().expect("temporary directory");
        let path = temp.path().join(format!("{DIGEST}{BOTTLE_SUFFIX}"));
        fs::write(&path, FIXTURE).expect("write fixture");

        let error = verify_file_digest_bounded(&path, DIGEST, FIXTURE.len() as u64 - 1)
            .expect_err("oversized bottle must be refused");
        assert!(format!("{error:#}").contains("byte limit"));
        assert!(
            path.exists(),
            "a refused file is not removed by verification"
        );
    }

    #[test]
    fn staging_file_removes_itself_unless_committed() {
        let temp = tempdir().expect("temporary directory");
        let staged_path = {
            let mut staged =
                StagedBottle::create(temp.path(), DIGEST).expect("create staging file");
            staged.write_all(FIXTURE).expect("write staging file");
            assert!(staged.path.exists());
            staged.path.clone()
        };
        assert!(
            !staged_path.exists(),
            "an uncommitted staging file must not survive"
        );
    }

    #[test]
    fn staging_files_have_unique_names() {
        let temp = tempdir().expect("temporary directory");
        let first = StagedBottle::create(temp.path(), DIGEST).expect("first staging file");
        let second = StagedBottle::create(temp.path(), DIGEST).expect("second staging file");

        assert_ne!(first.path, second.path);
        assert!(first.path.exists() && second.path.exists());
    }

    #[test]
    fn commit_publishes_the_verified_bottle() {
        let temp = tempdir().expect("temporary directory");
        let destination = temp.path().join(format!("{DIGEST}{BOTTLE_SUFFIX}"));
        fs::write(&destination, b"stale").expect("write old file");

        let mut staged = StagedBottle::create(temp.path(), DIGEST).expect("create staging file");
        staged.write_all(FIXTURE).expect("write staging file");
        staged.commit(&destination).expect("publish bottle");

        assert_eq!(fs::read(&destination).expect("read bottle"), FIXTURE);
        assert!(verify_file_digest(&destination, DIGEST).is_ok());
    }

    #[test]
    fn fetch_rejects_bad_urls_before_touching_the_cache() {
        let temp = tempdir().expect("temporary directory");
        let config = test_config(temp.path());

        let error = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
            .block_on(fetch(
                &config,
                "https://example.com/v2/homebrew/core/wget/blobs/sha256/x",
                DIGEST,
            ))
            .expect_err("non-GHCR URL must be refused");

        assert!(format!("{error:#}").contains("ghcr.io"));
        assert!(
            !temp.path().join(BOTTLE_DIR_NAME).exists(),
            "URL validation must run before the cache is created"
        );
    }

    #[test]
    fn fetch_serves_a_verified_cache_hit_offline() {
        let temp = tempdir().expect("temporary directory");
        let config = test_config(temp.path());
        let directory = bottle_cache_dir(&config).expect("create cache directory");
        let path = directory.join(format!("{DIGEST}{BOTTLE_SUFFIX}"));
        fs::write(&path, FIXTURE).expect("write fixture");

        let fetched = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
            .block_on(fetch(&config, &bottle_url(DIGEST), DIGEST))
            .expect("cache hit");

        assert_eq!(fetched, path);
    }

    #[cfg(unix)]
    #[test]
    fn refuses_a_symlinked_bottle_directory() {
        use std::os::unix::fs::symlink;

        let temp = tempdir().expect("temporary directory");
        let cache_dir = temp.path().join("cache");
        fs::create_dir_all(&cache_dir).expect("create cache directory");
        let outside = temp.path().join("outside");
        fs::create_dir_all(&outside).expect("create outside directory");
        symlink(&outside, cache_dir.join(BOTTLE_DIR_NAME)).expect("symlink bottle directory");

        assert!(
            bottle_cache_dir(&test_config(&cache_dir)).is_err(),
            "a symlinked native bottle directory must be refused"
        );
    }

    #[cfg(unix)]
    #[test]
    fn refuses_a_symlinked_cached_bottle() {
        use std::os::unix::fs::symlink;

        let temp = tempdir().expect("temporary directory");
        let outside = temp.path().join("outside.tar.gz");
        fs::write(&outside, FIXTURE).expect("write outside fixture");
        let path = temp.path().join(format!("{DIGEST}{BOTTLE_SUFFIX}"));
        symlink(&outside, &path).expect("symlink cache entry");

        assert!(
            reuse_cached_bottle(&path, DIGEST).is_err(),
            "a symlinked cached bottle must be refused, not followed"
        );
        assert!(outside.exists(), "the symlink target must be untouched");
    }
}
