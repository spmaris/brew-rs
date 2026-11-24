/// Files removed from the legacy on-disk bottle store.
///
/// Catalogue metadata lives in the API layer and is cleaned through
/// [`crate::infra::api::clean_catalogs`], so this type only describes the older
/// `cache_dir/bottles` directory that earlier brew-rs releases downloaded into.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct CleanupStats {
    pub bottles_removed: usize,
    pub bytes_freed: u64,
}
