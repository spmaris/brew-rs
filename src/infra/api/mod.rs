/// Official Homebrew formula and cask API base.
pub(crate) const API_BASE: &str = "https://formulae.brew.sh/api";

pub mod brew_client;
pub mod catalog_cache;
pub(crate) mod error;
pub mod http_client;
pub mod types;

pub use brew_client::BrewApiClient;
pub use catalog_cache::{CachePolicy, clean_catalogs};
pub(crate) use error::ApiError;
pub use types::*;
