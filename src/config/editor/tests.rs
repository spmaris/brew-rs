use super::*;

#[test]
fn test_set_parallel_downloads() {
    let mut config = Config::default();

    assert!(ConfigEditor::set_parallel_downloads(&mut config, 10).is_ok());
    assert_eq!(config.downloads.parallel_downloads, 10);

    assert!(ConfigEditor::set_parallel_downloads(&mut config, 0).is_err());
    assert!(ConfigEditor::set_parallel_downloads(&mut config, 100).is_err());
}

#[test]
fn test_set_ui_style() {
    let mut config = Config::default();

    assert!(ConfigEditor::set_ui_style(&mut config, "apt").is_ok());
    assert_eq!(config.ui.style, "apt");

    assert!(ConfigEditor::set_ui_style(&mut config, "invalid").is_err());
}

#[test]
fn test_set_value_by_path() {
    let mut config = Config::default();

    assert!(
        ConfigEditor::set_value_by_path(&mut config, "downloads.parallel_downloads", "10").is_ok()
    );
    assert_eq!(config.downloads.parallel_downloads, 10);

    assert!(ConfigEditor::set_value_by_path(&mut config, "ui.style", "apt").is_ok());
    assert_eq!(config.ui.style, "apt");

    assert!(ConfigEditor::set_value_by_path(&mut config, "invalid.path", "value").is_err());
}
