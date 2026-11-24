use super::error::ApiError;
use super::http_client::HttpClient;
use super::types::*;
use crate::config::Config;

pub struct BrewApiClient {
    http: HttpClient,
    base_url: String,
}

impl BrewApiClient {
    pub fn new(config: &Config) -> Result<Self, ApiError> {
        let http = HttpClient::new(config)?;

        Ok(Self {
            http,
            base_url: "https://formulae.brew.sh/api".to_string(),
        })
    }

    /// Get all formulae metadata
    pub async fn get_all_formulae(&self) -> Result<Vec<Formula>, ApiError> {
        let url = format!("{}/formula.json", self.base_url);
        self.http.get_json(&url).await
    }

    /// Get all casks metadata
    pub async fn get_all_casks(&self) -> Result<Vec<Cask>, ApiError> {
        let url = format!("{}/cask.json", self.base_url);
        self.http.get_json(&url).await
    }

    /// Get analytics data
    pub async fn get_analytics(&self, category: &str, period: &str) -> Result<Analytics, ApiError> {
        let url = format!("{}/analytics/{}/{}.json", self.base_url, category, period);
        self.http.get_json(&url).await
    }
}
