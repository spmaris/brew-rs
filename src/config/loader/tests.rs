use super::*;

#[test]
fn test_load_default_config() {
    let config = Config::default();
    assert_eq!(config.downloads.parallel_downloads, 5);
    assert!(config.general.parallel);
    assert_eq!(config.ui.style, "dnf");
}
