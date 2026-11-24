use std::time::Duration;

use anyhow::{Context, Result, bail};
use reqwest::header::{ACCEPT, LOCATION};
use reqwest::{Client, Response, StatusCode};

use crate::config::Config;

use super::consts::{
    GHCR_SERVICE, GHCR_TOKEN_ENDPOINT, MAX_REDIRECTS, MAX_TOKEN_BYTES, MAX_TOKEN_LEN,
};
use super::url::{BottleSource, is_ghcr_url, resolve_redirect};

/// Build the HTTP client used for GHCR requests.
pub(super) fn build_client(config: &Config) -> Result<Client> {
    build_verified_client(config, "bottle")
}

/// Build an HTTPS-only, manual-redirect HTTP client respecting proxy and agent settings.
pub(crate) fn build_verified_client(config: &Config, label: &str) -> Result<Client> {
    let mut builder = Client::builder()
        // Redirects are followed manually so every hop can be validated before
        // connecting or forwarding headers.
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
        .with_context(|| format!("failed to build the {label} HTTP client"))
}

/// Download the bottle blob, handling a public GHCR pull token and CDN redirects.
pub(super) async fn request_bottle(
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
            StatusCode::MOVED_PERMANENTLY
            | StatusCode::FOUND
            | StatusCode::SEE_OTHER
            | StatusCode::TEMPORARY_REDIRECT
            | StatusCode::PERMANENT_REDIRECT => {
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

/// Request an anonymous pull token with the fixed endpoint and minimal scope.
pub(super) async fn request_pull_token(
    client: &Client,
    scope: &str,
    config: &Config,
) -> Result<String> {
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

/// Timeout for connection setup and response headers.
pub(crate) fn request_timeout(config: &Config) -> Duration {
    Duration::from_secs(config.downloads.timeout.clamp(1, 300))
}

/// Idle timeout for a single streamed chunk.
pub(super) fn chunk_timeout(config: &Config) -> Duration {
    request_timeout(config)
}
