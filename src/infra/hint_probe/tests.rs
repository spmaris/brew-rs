use super::*;

fn executable(dir: &Path, name: &str, mode: u32) {
    let path = dir.join(name);
    std::fs::write(&path, "#!/bin/sh\n").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
}

#[test]
fn macos_builtin_requires_plain_executable_files() {
    let dir = tempfile::tempdir().unwrap();
    executable(dir.path(), "tool", 0o755);
    executable(dir.path(), "data", 0o644);
    std::fs::create_dir(dir.path().join("folder")).unwrap();
    let dirs = [dir.path()];
    assert!(macos_builtin("tool", &dirs).is_some());
    assert!(macos_builtin("data", &dirs).is_none());
    assert!(macos_builtin("folder", &dirs).is_none());
    assert!(macos_builtin("missing", &dirs).is_none());
    assert!(macos_builtin("../tool", &dirs).is_none());
    assert!(macos_builtin("..", &dirs).is_none());
    assert!(macos_builtin("", &dirs).is_none());
}
