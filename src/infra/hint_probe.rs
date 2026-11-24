//! Probes behind the not-found hints of a missing formula.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use crate::config::Config;
use crate::domain::plan_error::HintProbe;
use crate::infra::api::http_client::HttpClient;
use crate::infra::api::{API_BASE, ApiError};

const MACOS_DIRS: [&str; 4] = ["/usr/bin", "/bin", "/usr/sbin", "/sbin"];

/// [`HintProbe`] over the real macOS directories and the cask API.
pub(crate) struct SystemHintProbe(HttpClient);

impl SystemHintProbe {
    pub(crate) fn new(config: &Config) -> Result<Self, ApiError> {
        HttpClient::new(config).map(Self)
    }
}

impl HintProbe for SystemHintProbe {
    fn macos_builtin(&self, name: &str) -> Option<PathBuf> {
        let dirs: Vec<&Path> = MACOS_DIRS.iter().map(Path::new).collect();
        macos_builtin(name, &dirs)
    }

    async fn cask_exists(&self, token: &str) -> bool {
        let url = format!("{API_BASE}/cask/{token}.json");
        self.0.get_json::<serde_json::Value>(&url).await.is_ok()
    }
}

/// A regular, executable file named exactly `name` in one of `dirs`.
fn macos_builtin(name: &str, dirs: &[&Path]) -> Option<PathBuf> {
    if name.is_empty() || name == "." || name == ".." || name.contains(['/', '\\', '\0']) {
        return None;
    }
    dirs.iter().map(|dir| dir.join(name)).find(|path| {
        std::fs::metadata(path)
            .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
    })
}

#[cfg(test)]
mod tests;
