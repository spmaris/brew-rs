use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::config::Config;
use crate::domain::plan::{PackageSource, PlannedFormula};
use crate::domain::progress::DownloadProgress;
use crate::infra::download::Fetched;
use crate::infra::store::{Dependency, Prepared, Receipt, Store};
use crate::infra::{archive, download, portable_fetch};

use super::shellenv::posix_quote;

pub(in crate::commands) async fn fetch_formula(
    config: &Config,
    formula: &PlannedFormula,
    progress: &dyn DownloadProgress,
) -> Result<Fetched> {
    let download = match formula.source {
        PackageSource::Upstream => portable_fetch::fetch(config, formula, progress)
            .await
            .map_err(anyhow::Error::from),
        PackageSource::Homebrew => download::fetch(config, &formula.url, &formula.sha256, progress)
            .await
            .map_err(anyhow::Error::from),
    };
    download.with_context(|| format!("Failed to fetch {} {}", formula.name, formula.version))
}

#[derive(Debug)]
pub(super) struct Candidate<'a> {
    pub(super) formula: &'a PlannedFormula,
    pub(super) receipt: Receipt,
}

pub(super) fn stage_formula(
    store: &Store,
    candidate: Candidate<'_>,
    bottle: &Path,
) -> Result<Prepared> {
    let stage = store.stage()?;
    let receipt = candidate.receipt;
    match candidate.formula.source {
        PackageSource::Upstream => portable_fetch::stage(candidate.formula, bottle, stage.path())
            .with_context(|| {
            format!(
                "Failed to stage upstream {} {}",
                receipt.name, receipt.version
            )
        })?,
        PackageSource::Homebrew => archive::unpack(
            bottle,
            stage.path(),
            &receipt.name,
            &receipt.version,
            &receipt.sha256,
        )
        .with_context(|| format!("Failed to stage {} {}", receipt.name, receipt.version))?,
    }
    if receipt.source == PackageSource::Homebrew && receipt.name == "nvm" {
        relocate_nvm_wrapper(store.prefix(), stage.path(), &receipt.version)
            .context("Failed to relocate the staged nvm wrapper")?;
    }
    Ok(Prepared { receipt, stage })
}

// Homebrew's portable nvm bottle still ships a wrapper with these markers.
// Match the reviewed wrapper, not a general shell grammar. The source is tied
// to this keg; the managed opt path lets user-created nvm helper links survive
// future upgrades, as the original wrapper intended.
pub(super) fn relocate_nvm_wrapper(prefix: &Path, stage: &Path, version: &str) -> Result<()> {
    const LIMIT: u64 = 4096;
    let wrapper = stage.join("nvm.sh");
    if !fs::symlink_metadata(&wrapper)?.is_file() {
        bail!("nvm.sh must be a regular staged file");
    }
    let mut text = String::new();
    fs::File::open(&wrapper)?
        .take(LIMIT + 1)
        .read_to_string(&mut text)?;
    if text.len() as u64 > LIMIT {
        bail!("nvm.sh exceeds the wrapper size limit");
    }
    if !text.contains("@@HOMEBREW_") {
        return Ok(());
    }
    let cellar = format!("@@HOMEBREW_CELLAR@@/nvm/{version}/libexec/nvm.sh");
    const NVM_SH: &str = "@@HOMEBREW_PREFIX@@/opt/nvm/libexec/nvm.sh";
    const NVM_EXEC: &str = "@@HOMEBREW_PREFIX@@/opt/nvm/libexec/nvm-exec";
    let expected = format!(
        "# $NVM_DIR should be \"$HOME/.nvm\" by default to avoid user-installed nodes destroyed every update\n\
         [ -z \"$NVM_DIR\" ] && export NVM_DIR=\"$HOME/.nvm\"\n\
         \\. {cellar}\n\
         # \"nvm exec\" and certain 3rd party scripts expect \"nvm.sh\" and \"nvm-exec\" to exist under $NVM_DIR\n\
         [ -e \"$NVM_DIR\" ] || mkdir -p \"$NVM_DIR\"\n\
         [ -e \"$NVM_DIR/nvm.sh\" ] || ln -s {NVM_SH} \"$NVM_DIR/nvm.sh\"\n\
         [ -e \"$NVM_DIR/nvm-exec\" ] || ln -s {NVM_EXEC} \"$NVM_DIR/nvm-exec\"\n"
    );
    if text != expected {
        bail!("nvm.sh contains an unreviewed Homebrew wrapper");
    }
    let libexec = stage.join("libexec");
    if !fs::symlink_metadata(&libexec)?.is_dir()
        || !fs::symlink_metadata(libexec.join("nvm.sh"))?.is_file()
        || !fs::symlink_metadata(libexec.join("nvm-exec"))?.is_file()
    {
        bail!("nvm wrapper requires regular staged libexec scripts");
    }
    let prefix = prefix.canonicalize()?;
    let quote = |path: PathBuf| -> Result<String> {
        let value = path.to_str().context("nvm requires UTF-8 paths")?;
        if value.chars().any(char::is_control) || value.contains("@@HOMEBREW_") {
            bail!("unsupported path characters in nvm wrapper");
        }
        Ok(posix_quote(value))
    };
    let helpers = prefix.join("opt/nvm/libexec");
    let fixed = text
        .replace(
            &cellar,
            &quote(
                prefix
                    .join("Cellar/nvm")
                    .join(version)
                    .join("libexec/nvm.sh"),
            )?,
        )
        .replace(NVM_SH, &quote(helpers.join("nvm.sh"))?)
        .replace(NVM_EXEC, &quote(helpers.join("nvm-exec"))?);
    fs::write(wrapper, fixed)?;
    Ok(())
}

pub(super) fn receipt_for(
    formula: &PlannedFormula,
    plan: &[PlannedFormula],
    requested: bool,
) -> Result<Receipt> {
    let mut dependencies = Vec::new();
    for name in &formula.dependencies {
        let dependency = plan
            .iter()
            .find(|entry| entry.name == *name)
            .with_context(|| {
                format!(
                    "The plan for '{}' omitted dependency '{name}'",
                    formula.name
                )
            })?;
        dependencies.push(Dependency {
            name: dependency.name.clone(),
            version: dependency.version.clone(),
            sha256: dependency.sha256.clone(),
        });
    }
    dependencies.sort_by(|left, right| left.name.cmp(&right.name));
    let version_scheme = formula.version_scheme.with_context(|| {
        format!(
            "'{}' does not declare a Homebrew version scheme",
            formula.name
        )
    })?;
    Ok(Receipt {
        name: formula.name.clone(),
        version: formula.version.clone(),
        version_scheme,
        source: formula.source,
        sha256: formula.sha256.clone(),
        requested,
        keg_only: formula.keg_only,
        conflicts_with: formula.conflicts_with.clone(),
        dependencies,
    })
}
