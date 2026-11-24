use reqwest::{Client, ClientBuilder};
use std::time::Duration;
use tokio::time::timeout;

use crate::config::Config;

use super::error::ApiError;

// Includes the expanded public formula catalog. Count bytes after decoding so
// compressed responses cannot bypass the bound.
const MAX_JSON_BYTES: usize = 96 * 1024 * 1024;

pub struct HttpClient {
    client: Client,
    timeout: Duration,
}

impl HttpClient {
    pub fn new(config: &Config) -> Result<Self, ApiError> {
        let mut client_builder =
            ClientBuilder::new().timeout(Duration::from_secs(config.downloads.timeout));

        if !config.downloads.use_proxy {
            client_builder = client_builder.no_proxy();
        }
        if let Some(user_agent) = &config.downloads.user_agent {
            client_builder = client_builder.user_agent(user_agent);
        }

        let client = client_builder.build().map_err(ApiError::ClientBuild)?;

        Ok(Self {
            client,
            timeout: Duration::from_secs(config.downloads.timeout),
        })
    }

    /// Generic JSON fetch with typed error handling.
    pub async fn get_json<T>(&self, url: &str) -> Result<T, ApiError>
    where
        T: serde::de::DeserializeOwned,
    {
        let response = timeout(self.timeout, self.client.get(url).send())
            .await
            .map_err(ApiError::TimedOut)?
            .map_err(ApiError::Send)?;

        let mut response = response.error_for_status().map_err(ApiError::Status)?;
        if !response.status().is_success() {
            return Err(ApiError::UnexpectedStatus(response.status()));
        }
        if response
            .content_length()
            .is_some_and(|bytes| bytes > MAX_JSON_BYTES as u64)
        {
            return Err(ApiError::ResponseTooLarge);
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(ApiError::ReadBody)? {
            if chunk.len() > MAX_JSON_BYTES - bytes.len() {
                return Err(ApiError::ResponseTooLarge);
            }
            bytes.extend_from_slice(&chunk);
        }
        serde_json::from_slice(&bytes).map_err(ApiError::InvalidJson)
    }
}

#[cfg(test)]
mod tests;
