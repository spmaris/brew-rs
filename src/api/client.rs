use anyhow::{Context, Result};
use reqwest::{Client, ClientBuilder};
use std::time::Duration;
use tokio::time::timeout;

use super::types::*;
use crate::config::Config;

pub struct BrewApiClient {
    client: Client,
    base_url: String,
    timeout: Duration,
}

impl BrewApiClient {
    pub fn new(config: &Config) -> Result<Self> {
        let mut client_builder = ClientBuilder::new()
            .timeout(Duration::from_secs(config.downloads.timeout));

        if let Some(user_agent) = &config.downloads.user_agent {
            client_builder = client_builder.user_agent(user_agent);
        }

        let client = client_builder
            .build()
            .context("Failed to create HTTP client")?;

        Ok(Self {
            client,
            base_url: "https://formulae.brew.sh/api".to_string(),
            timeout: Duration::from_secs(config.downloads.timeout),
        })
    }

    /// Get all formulae metadata
    pub async fn get_all_formulae(&self) -> Result<Vec<Formula>> {
        let url = format!("{}/formula.json", self.base_url);
        self.fetch_json(&url).await
    }

    /// Get all casks metadata
    pub async fn get_all_casks(&self) -> Result<Vec<Cask>> {
        let url = format!("{}/cask.json", self.base_url);
        self.fetch_json(&url).await
    }

    /// Get specific formula information
    pub async fn get_formula(&self, name: &str) -> Result<Formula> {
        let url = format!("{}/formula/{}.json", self.base_url, name);
        self.fetch_json(&url).await
    }

    /// Get specific cask information
    pub async fn get_cask(&self, name: &str) -> Result<Cask> {
        let url = format!("{}/cask/{}.json", self.base_url, name);
        self.fetch_json(&url).await
    }

    /// Search formulae by name and description
    pub async fn search_formulae(&self, query: &str, limit: usize) -> Result<Vec<Formula>> {
        let formulae = self.get_all_formulae().await?;
        let query_lower = query.to_lowercase();
        
        let mut matches: Vec<_> = formulae
            .into_iter()
            .filter_map(|formula| {
                let name_score = if formula.name.to_lowercase().contains(&query_lower) {
                    Some(100) // Exact name match gets highest score
                } else {
                    None
                };
                
                let desc_score = if formula.desc.to_lowercase().contains(&query_lower) {
                    Some(50) // Description match gets lower score
                } else {
                    None
                };
                
                if let Some(score) = name_score.or(desc_score) {
                    Some((formula, score))
                } else {
                    None
                }
            })
            .collect();
        
        // Sort by score (name matches first) then by name
        matches.sort_by(|a, b| {
            b.1.cmp(&a.1).then_with(|| a.0.name.cmp(&b.0.name))
        });
        
        Ok(matches
            .into_iter()
            .take(limit)
            .map(|(formula, _)| formula)
            .collect())
    }

    /// Search casks by name and description
    pub async fn search_casks(&self, query: &str, limit: usize) -> Result<Vec<Cask>> {
        let casks = self.get_all_casks().await?;
        let query_lower = query.to_lowercase();
        
        let mut matches: Vec<_> = casks
            .into_iter()
            .filter_map(|cask| {
                let name_score = if cask.token.to_lowercase().contains(&query_lower) ||
                    cask.name.iter().any(|n| n.to_lowercase().contains(&query_lower)) {
                    Some(100)
                } else {
                    None
                };
                
                let desc_score = if cask.desc.to_lowercase().contains(&query_lower) {
                    Some(50)
                } else {
                    None
                };
                
                if let Some(score) = name_score.or(desc_score) {
                    Some((cask, score))
                } else {
                    None
                }
            })
            .collect();
        
        matches.sort_by(|a, b| {
            b.1.cmp(&a.1).then_with(|| a.0.token.cmp(&b.0.token))
        });
        
        Ok(matches
            .into_iter()
            .take(limit)
            .map(|(cask, _)| cask)
            .collect())
    }

    /// Get analytics data
    pub async fn get_analytics(&self, category: &str, period: &str) -> Result<Analytics> {
        let url = format!("{}/analytics/{}/{}.json", self.base_url, category, period);
        self.fetch_json(&url).await
    }

    /// Get analytics for specific formula
    pub async fn get_formula_analytics(&self, category: &str, period: &str) -> Result<Analytics> {
        let url = format!("{}/analytics/{}/homebrew-core/{}.json", self.base_url, category, period);
        self.fetch_json(&url).await
    }

    /// Get analytics for casks
    pub async fn get_cask_analytics(&self, period: &str) -> Result<Analytics> {
        let url = format!("{}/analytics/cask-install/homebrew-cask/{}.json", self.base_url, period);
        self.fetch_json(&url).await
    }

    /// Download a bottle file
    pub async fn download_bottle(&self, url: &str) -> Result<Vec<u8>> {
        let response = timeout(self.timeout, self.client.get(url).send())
            .await
            .context("Request timed out")?
            .context("Failed to send request")?;

        if !response.status().is_success() {
            anyhow::bail!("Failed to download bottle: HTTP {}", response.status());
        }

        let bytes = response
            .bytes()
            .await
            .context("Failed to read response body")?;

        Ok(bytes.to_vec())
    }

    /// Generic JSON fetch with error handling
    async fn fetch_json<T>(&self, url: &str) -> Result<T>
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

    /// Get the appropriate bottle URL for ARM64 macOS
    pub fn get_arm64_bottle_url(&self, formula: &Formula) -> Option<String> {
        let bottle = formula.bottle.as_ref()?;
        
        // Try different ARM64 variants in order of preference
        let platforms = ["arm64_sequoia", "arm64_sonoma", "arm64_ventura"];
        
        for platform in &platforms {
            if let Some(bottle_file) = bottle.stable.files.get(*platform) {
                return Some(bottle_file.url.clone());
            }
        }
        
        None
    }

    /// Verify bottle checksum
    pub fn verify_bottle_checksum(&self, data: &[u8], expected_sha256: &str) -> Result<()> {
        use sha2::{Sha256, Digest};
        
        let mut hasher = Sha256::new();
        hasher.update(data);
        let result = hasher.finalize();
        let actual_sha256 = hex::encode(result);
        
        if actual_sha256 != expected_sha256 {
            anyhow::bail!(
                "Checksum verification failed: expected {}, got {}",
                expected_sha256,
                actual_sha256
            );
        }
        
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    #[tokio::test]
    async fn test_client_creation() {
        let config = Config::default();
        let client = BrewApiClient::new(&config);
        assert!(client.is_ok());
    }

    #[test]
    fn test_bottle_url_selection() {
        let config = Config::default();
        let client = BrewApiClient::new(&config).unwrap();
        
        // Test with empty formula
        let formula = Formula::default();
        assert!(client.get_arm64_bottle_url(&formula).is_none());
    }

    #[test]
    fn test_checksum_verification() {
        let config = Config::default();
        let client = BrewApiClient::new(&config).unwrap();
        
        let data = b"test data";
        let expected = "916f0027a575074ce72a331777c3478d6513f786a591bd892da1a577bf2335f9";
        
        assert!(client.verify_bottle_checksum(data, expected).is_ok());
        assert!(client.verify_bottle_checksum(data, "wrong_checksum").is_err());
    }
}