use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::commands::install::prefix_path;
use crate::config::Config;
use crate::infra::store::{Receipt, Store, validate_dependencies_with_retained};
use crate::ui::DnfStyleFormatter;

use super::executable::{ensure_fresh_trial, managed_binary};
use super::launchd::{displayable_path, local_label, prefix_hash, render_argv};
use super::observe::SERVICE_TRIAL_ROOT;

// Flags were checked against the Colima 0.10.3 start command. The preview is
// deliberately separate from the upstream daemon definition: it uses a new
// state directory, does not activate a container context or set up SSH config,
// and does not mount the user's home directory.
pub(super) const COLIMA_ARGS: &[&str] = &[
    "start",
    "--foreground",
    "--activate=false",
    "--ssh-config=false",
    "--mount=none",
    "--template=false",
];

pub(super) const COLIMA_VERSION: &str = "0.10.3";

pub(super) fn preview_colima(config: &Config, prefix: Option<&Path>) -> Result<()> {
    let store = Store::open_existing(&prefix_path(prefix)?)
        .context("Colima service preview requires an initialized brewrs prefix")?;
    let prefix = store.prefix().canonicalize()?;
    let retained = store.retained()?;
    let plan = colima_plan(&prefix, &store.installed()?, &retained)?;
    let ui = DnfStyleFormatter::new(config);
    let args: Vec<String> = COLIMA_ARGS.iter().map(|arg| (*arg).to_string()).collect();
    println!("{}", ui.section("Colima service trial (preview only)"));
    for (label, value) in [
        ("Proposed label", plan.label),
        ("Colima", plan.colima.display().to_string()),
        ("Lima", plan.lima.display().to_string()),
        ("Arguments", render_argv(&args, 0)),
        ("Trial HOME", plan.home.display().to_string()),
        ("Working directory", plan.home.display().to_string()),
        ("COLIMA_HOME", plan.colima_home.display().to_string()),
        (
            "COLIMA_CACHE_HOME",
            plan.home.join("cache").display().to_string(),
        ),
        ("COLIMA_PROFILE", plan.profile),
        ("LIMA_HOME", plan.lima_home.display().to_string()),
        ("PATH", plan.path),
    ] {
        println!("{}", ui.field(label, &value));
    }
    println!(
        "\nNothing was started or registered. This plan does not import existing VM data or change your Docker context."
    );
    println!("VM startup and socket paths have not been tested by this preview.");
    println!(
        "The trial HOME, working directory, and environment above are proposals for a future launch; this preview enforces no sandbox or isolation boundary."
    );
    Ok(())
}

pub(super) struct ColimaPlan {
    pub(super) label: String,
    pub(super) profile: String,
    pub(super) colima: PathBuf,
    pub(super) lima: PathBuf,
    pub(super) home: PathBuf,
    pub(super) colima_home: PathBuf,
    pub(super) lima_home: PathBuf,
    pub(super) path: String,
}

pub(super) fn colima_plan(
    prefix: &Path,
    installed: &[Receipt],
    retained: &[Receipt],
) -> Result<ColimaPlan> {
    validate_dependencies_with_retained(installed, &[], retained)
        .context("Cannot preview a service with unsatisfied receipt pins")?;
    let colima = installed
        .iter()
        .find(|receipt| receipt.name == "colima")
        .context("Install colima in this brewrs prefix before previewing its service")?;
    // Match the actual command-line contract reviewed for the initial lane.
    if colima.version != COLIMA_VERSION {
        bail!(
            "A Colima service trial has not been reviewed for the installed version {}",
            colima.version
        );
    }
    let pin = colima
        .dependencies
        .iter()
        .find(|pin| pin.name == "lima")
        .context("The Colima receipt does not pin a managed Lima keg")?;
    let colima_bin = managed_binary(prefix, &colima.name, &colima.version, "colima")?;
    let lima_bin = managed_binary(prefix, &pin.name, &pin.version, "limactl")?;
    let trial = prefix.join(SERVICE_TRIAL_ROOT).join("colima");
    ensure_fresh_trial(&trial)?;
    let home = trial.join("home");
    let colima_home = home.join(".colima");
    // Colima itself can place Lima's state under its own configuration tree.
    let lima_home = colima_home.join("_lima");
    let lima_bin_dir = lima_bin.parent().context("Lima executable has no parent")?;
    let path = format!(
        "{}:/usr/bin:/bin:/usr/sbin:/sbin",
        displayable_path(lima_bin_dir)?
    );
    for path in [prefix, &home, &colima_bin, &lima_bin] {
        displayable_path(path)?;
    }
    let hash = prefix_hash(prefix)?;
    Ok(ColimaPlan {
        label: local_label(&hash, "colima"),
        profile: format!("brewrs-{}", &hash[..8]),
        colima: colima_bin,
        lima: lima_bin,
        home,
        colima_home,
        lima_home,
        path,
    })
}
