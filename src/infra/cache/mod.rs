pub mod bottle_store;
pub mod cleanup;
pub mod manager;

pub use manager::CacheManager;

pub(crate) const NATIVE_BOTTLE_DIR: &str = "native-bottles";
pub(crate) const NATIVE_CASK_DIR: &str = "native-casks";
