use anyhow::{Context, Result, bail};
use reqwest::{Client, ClientBuilder};
use std::time::Duration;
use tokio::time::timeout;

use crate::config::Config;
use crate::domain::plan_error::ApiFailure;

// Includes the expanded public formula catalog. Count bytes after decoding so
// compressed responses cannot bypass the bound.
const MAX_JSON_BYTES: usize = 96 * 1024 * 1024;

pub struct HttpClient {
    client: Client,
    timeout: Duration,
}

impl HttpClient {
    pub fn new(config: &Config) -> Result<Self> {
        let mut client_builder =
            ClientBuilder::new().timeout(Duration::from_secs(config.downloads.timeout));

        if !config.downloads.use_proxy {
            client_builder = client_builder.no_proxy();
        }
        if let Some(user_agent) = &config.downloads.user_agent {
            client_builder = client_builder.user_agent(user_agent);
        }

        let client = client_builder
            .build()
            .context("Failed to create HTTP client")?;

        Ok(Self {
            client,
            timeout: Duration::from_secs(config.downloads.timeout),
        })
    }

    /// Generic JSON fetch with error handling
    pub async fn fetch_json<T>(&self, url: &str) -> Result<T>
    where
        T: serde::de::DeserializeOwned,
    {
        let response = timeout(self.timeout, self.client.get(url).send())
            .await
            .context("Request timed out")?
            .context("Failed to send request")?;

        let mut response = response.error_for_status().context("API request failed")?;
        if !response.status().is_success() {
            bail!("Unexpected API response: HTTP {}", response.status());
        }
        if response
            .content_length()
            .is_some_and(|bytes| bytes > MAX_JSON_BYTES as u64)
        {
            bail!("Repository JSON exceeds the 96 MiB response limit");
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .context("Failed to read API response")?
        {
            if chunk.len() > MAX_JSON_BYTES - bytes.len() {
                bail!("Repository JSON exceeds the 96 MiB response limit");
            }
            bytes.extend_from_slice(&chunk);
        }
        serde_json::from_slice(&bytes).context("Failed to parse JSON response")
    }
}

/// Whether an error chain contains a typed HTTP `404 Not Found` status.
pub(crate) fn is_not_found(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<reqwest::Error>()
            .and_then(reqwest::Error::status)
            == Some(reqwest::StatusCode::NOT_FOUND)
    })
}

fn classify_parts(status: Option<u16>, transport_failure: bool) -> Option<ApiFailure> {
    match status {
        Some(404) => Some(ApiFailure::NotFound),
        Some(code) if (500..600).contains(&code) => Some(ApiFailure::Server(code)),
        Some(_) => None,
        None => transport_failure.then_some(ApiFailure::Unreachable),
    }
}

/// Classify an API error chain as not-found, unreachable, or a 5xx response.
/// Other failures (4xx, malformed bodies, ...) return `None`.
pub(crate) fn classify(error: &anyhow::Error) -> Option<ApiFailure> {
    error.chain().find_map(|cause| {
        if let Some(failure) = cause.downcast_ref::<ApiFailure>() {
            return Some(*failure);
        }
        if cause.is::<tokio::time::error::Elapsed>() {
            return Some(ApiFailure::Unreachable);
        }
        let error = cause.downcast_ref::<reqwest::Error>()?;
        classify_parts(
            error.status().map(|status| status.as_u16()),
            error.is_connect() || error.is_timeout() || error.is_request(),
        )
    })
}

#[cfg(test)]
mod tests;
