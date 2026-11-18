use anyhow::{Context, Result};
use reqwest::{Client, ClientBuilder};
use std::time::Duration;
use tokio::time::timeout;

use crate::config::Config;

pub struct HttpClient {
    client: Client,
    timeout: Duration,
}

impl HttpClient {
    pub fn new(config: &Config) -> Result<Self> {
        let mut client_builder =
            ClientBuilder::new().timeout(Duration::from_secs(config.downloads.timeout));

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

        if !response.status().is_success() {
            anyhow::bail!("API request failed: HTTP {}", response.status());
        }

        let data = response
            .json::<T>()
            .await
            .context("Failed to parse JSON response")?;

        Ok(data)
    }
}
