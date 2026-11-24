//! Conservative comparison of cask version strings.

use std::cmp::Ordering;

use super::super::version;

/// Ordering result when comparing an available cask version with an installed one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CaskVersionOrder {
    Same,
    Upgrade,
    Downgrade,
}

/// Compare candidate and installed cask version strings conservatively.
pub(crate) fn compare_cask_versions(candidate: &str, installed: &str) -> CaskVersionOrder {
    if candidate == installed {
        return CaskVersionOrder::Same;
    }
    let (cand_base, cand_build) = split_cask_version(candidate);
    let (inst_base, inst_build) = split_cask_version(installed);
    match version::compare(cand_base, inst_base) {
        Ok(Ordering::Greater) => CaskVersionOrder::Upgrade,
        Ok(Ordering::Less) => CaskVersionOrder::Downgrade,
        Ok(Ordering::Equal) => match (cand_build, inst_build) {
            (Some(cb), Some(ib)) => match version::compare(cb, ib) {
                Ok(Ordering::Greater) => CaskVersionOrder::Upgrade,
                Ok(Ordering::Less) => CaskVersionOrder::Downgrade,
                Ok(Ordering::Equal) => CaskVersionOrder::Same,
                Err(_) => {
                    if cb == ib {
                        CaskVersionOrder::Same
                    } else {
                        CaskVersionOrder::Upgrade
                    }
                }
            },
            (Some(_), None) => CaskVersionOrder::Upgrade,
            (None, Some(_)) => CaskVersionOrder::Downgrade,
            (None, None) => CaskVersionOrder::Same,
        },
        Err(_) => CaskVersionOrder::Upgrade,
    }
}

fn split_cask_version(version: &str) -> (&str, Option<&str>) {
    match version.split_once(',') {
        Some((base, build)) => (base, Some(build)),
        None => (version, None),
    }
}
