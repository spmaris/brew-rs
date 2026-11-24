//! Read-only service previews for brewrs-managed packages.
//!
//! This module has no start, stop, registration, supervision, filesystem-write,
//! launchd-query, process-query, or service-state operation. It renders the
//! reviewed Colima 0.10.3 trial contract from local receipts and, for other
//! installed formulae, an official `service` declaration that has been pinned
//! to the exact installed receipt. [`list`] only reports what it can observe in
//! two brewrs-owned state roots; it never claims that a service is running.

use std::path::Path;

use anyhow::Result;

use crate::config::Config;
use crate::domain::plan;

use self::colima::preview_colima;
use self::preview::preview_formula;

mod colima;
mod executable;
mod launchd;
mod list;
mod observe;
mod preview;

pub(crate) use self::list::list;

/// Preview or list a service without starting or registering anything.
pub(crate) async fn preview(config: &Config, prefix: Option<&Path>, package: &str) -> Result<()> {
    let requested = plan::parse_target(package)?.to_string();
    if requested == "colima" {
        return preview_colima(config, prefix);
    }
    preview_formula(config, prefix, &requested).await
}

#[cfg(test)]
mod tests;
