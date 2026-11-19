use anyhow::{Context, Result, bail};
use reqwest::{Client, ClientBuilder};
use std::time::Duration;
use tokio::time::timeout;

use crate::config::Config;

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

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    async fn serve(response: &'static [u8]) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") && request.len() < 4096 {
                request.push(socket.read_u8().await.unwrap());
            }
            let _ = socket.write_all(response).await;
        });
        format!("http://{address}/metadata")
    }

    fn direct_client() -> HttpClient {
        let timeout = Duration::from_secs(2);
        let client = ClientBuilder::new()
            .no_proxy()
            .timeout(timeout)
            .build()
            .unwrap();
        HttpClient { client, timeout }
    }

    #[tokio::test]
    async fn not_found_retains_http_status() {
        let url = serve(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n").await;
        let error = direct_client()
            .fetch_json::<serde_json::Value>(&url)
            .await
            .unwrap_err();
        assert_eq!(
            error
                .downcast_ref::<reqwest::Error>()
                .and_then(reqwest::Error::status),
            Some(reqwest::StatusCode::NOT_FOUND)
        );
    }

    #[tokio::test]
    async fn streams_chunked_json() {
        let url = serve(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\n[1,\r\n2\r\n2]\r\n0\r\n\r\n").await;
        assert_eq!(
            direct_client().fetch_json::<Vec<u32>>(&url).await.unwrap(),
            vec![1, 2]
        );
    }

    #[tokio::test]
    async fn rejects_excessive_declared_length_before_reading_body() {
        let url = serve(b"HTTP/1.1 200 OK\r\nContent-Length: 200000000\r\n\r\n").await;
        let error = direct_client()
            .fetch_json::<serde_json::Value>(&url)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("response limit"));
    }
}
