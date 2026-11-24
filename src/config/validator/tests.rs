use super::*;

#[test]
fn test_valid_config() {
    let config = Config::default();
    assert!(ConfigValidator::validate(&config).is_ok());
}

#[test]
fn test_invalid_parallel_downloads() {
    let mut config = Config::default();
    config.downloads.parallel_downloads = 0;
    assert!(ConfigValidator::validate(&config).is_err());

    config.downloads.parallel_downloads = 100;
    assert!(ConfigValidator::validate(&config).is_err());
}

#[test]
fn test_invalid_ui_style() {
    let mut config = Config::default();
    config.ui.style = "invalid".to_string();
    assert!(ConfigValidator::validate(&config).is_err());
}
