//! The per-prefix writer lock.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use super::LOCK_FILE;
use super::paths::{monotonic_nonce, state_dir};

/// Serialize writers for one prefix with an exclusive lock file.
pub(crate) struct InstallLock {
    path: PathBuf,
    token: String,
}

impl InstallLock {
    pub(crate) fn acquire(prefix: &Path) -> Result<Self> {
        let path = state_dir(prefix).join(LOCK_FILE);
        let token = format!("pid={} nonce={}\n", std::process::id(), monotonic_nonce());
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut file) => {
                if let Err(error) = file
                    .write_all(token.as_bytes())
                    .and_then(|()| file.sync_all())
                {
                    let _ = fs::remove_file(&path);
                    return Err(error)
                        .with_context(|| format!("failed to write lock '{}'", path.display()));
                }
                Ok(Self { path, token })
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                let owner = describe_lock(&path);
                bail!(
                    "another brewrs install holds the lock '{}' ({owner}); if no brewrs process is running, the lock is stale: remove '{}' and retry",
                    path.display(),
                    path.display()
                )
            }
            Err(error) => {
                Err(error).with_context(|| format!("failed to create lock '{}'", path.display()))
            }
        }
    }
}

impl Drop for InstallLock {
    fn drop(&mut self) {
        // Only remove our own lock, and never follow a symlink planted here.
        // A replaced/stale lock is left for the explicit advice in `acquire`.
        match fs::symlink_metadata(&self.path) {
            Ok(metadata) if metadata.is_file() => {}
            _ => return,
        }
        if let Ok(contents) = fs::read(&self.path)
            && contents == self.token.as_bytes()
        {
            let _ = fs::remove_file(&self.path);
        }
    }
}

fn describe_lock(path: &Path) -> String {
    match fs::symlink_metadata(path) {
        Ok(metadata) if !metadata.is_file() => "lock path is not a regular file".into(),
        Ok(_) => {
            let mut contents = String::new();
            match File::open(path).and_then(|file| file.take(4096).read_to_string(&mut contents)) {
                Ok(_) => {
                    let contents = contents.trim();
                    if contents.is_empty() {
                        "unreadable lock".into()
                    } else {
                        contents.to_string()
                    }
                }
                Err(_) => "unreadable lock".into(),
            }
        }
        Err(_) => "unreadable lock".into(),
    }
}
