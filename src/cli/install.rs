use std::io::{self, Write};

use anyhow::Result;

use crate::brew::{Brew, require_confirmation_input};
use crate::config::Config;
use crate::ui::DnfStyleFormatter;

use super::{PackageKind, Transaction};

pub(super) async fn install_or_upgrade(
    config: &Config,
    brew: Brew,
    subcommand: &str,
    mut options: Vec<&str>,
    packages: &[String],
    transaction: Transaction,
) -> Result<()> {
    let confirm = !transaction.dry_run && !transaction.yes && config.general.confirm_operations;
    if confirm {
        require_confirmation_input()?;
    }
    let action = if subcommand == "install" {
        "Install"
    } else {
        "Upgrade"
    };
    show_transaction(config, action, packages, transaction.dry_run, confirm)?;
    if transaction.dry_run || confirm {
        let mut preview = options.clone();
        preview.push("--dry-run");
        append_targets(&mut preview, packages);
        brew.run(subcommand, &preview, true).await?;
    }
    if transaction.dry_run {
        complete(config, true);
    } else if !confirm || confirmed()? {
        options.push("--yes");
        append_targets(&mut options, packages);
        // A confirmed transaction should not refresh metadata after showing its preview.
        brew.run(subcommand, &options, confirm).await?;
        complete(config, false);
    }
    Ok(())
}

pub(super) async fn remove(
    config: &Config,
    brew: Brew,
    packages: &[String],
    kind: PackageKind,
    transaction: Transaction,
    autoremove: bool,
) -> Result<()> {
    let confirm = !transaction.dry_run && !transaction.yes && config.general.confirm_operations;
    if confirm {
        require_confirmation_input()?;
    }
    let action = if autoremove {
        "Remove with unused dependencies"
    } else {
        "Remove"
    };
    show_transaction(config, action, packages, transaction.dry_run, confirm)?;
    let mut options: Vec<_> = kind.flag().into_iter().collect();
    if transaction.dry_run || confirm {
        let mut preview = options.clone();
        preview.push("--dry-run");
        append_targets(&mut preview, packages);
        brew.uninstall(&preview, true, autoremove).await?;
    }
    if !transaction.dry_run && (!confirm || confirmed()?) {
        append_targets(&mut options, packages);
        brew.uninstall(&options, confirm, autoremove).await?;
        complete(config, false);
    } else if transaction.dry_run {
        complete(config, true);
    }
    Ok(())
}

pub(super) async fn autoremove(
    config: &Config,
    brew: Brew,
    transaction: Transaction,
) -> Result<()> {
    let confirm = !transaction.dry_run && !transaction.yes && config.general.confirm_operations;
    if confirm {
        require_confirmation_input()?;
    }
    show_transaction(config, "Autoremove", &[], transaction.dry_run, confirm)?;
    if transaction.dry_run || confirm {
        brew.run("autoremove", &["--dry-run"], true).await?;
    }
    if !transaction.dry_run && (!confirm || confirmed()?) {
        brew.run("autoremove", &[], confirm).await?;
        complete(config, false);
    } else if transaction.dry_run {
        complete(config, true);
    }
    Ok(())
}

fn append_targets<'a>(args: &mut Vec<&'a str>, packages: &'a [String]) {
    if !packages.is_empty() {
        args.push("--");
        args.extend(packages.iter().map(String::as_str));
    }
}

fn show_transaction(
    config: &Config,
    action: &str,
    targets: &[String],
    dry_run: bool,
    confirm: bool,
) -> Result<()> {
    let formatter = DnfStyleFormatter::new(config);
    let mode = if dry_run {
        "Preview only"
    } else if confirm {
        "Preview and confirm"
    } else {
        "Apply"
    };
    let default_scope = if action == "Autoremove" {
        "Unused dependencies"
    } else {
        "All installed packages"
    };
    let rows: Vec<_> = if targets.is_empty() {
        vec![vec![action.into(), default_scope.into(), mode.into()]]
    } else {
        targets
            .iter()
            .map(|package| vec![action.into(), package.clone(), mode.into()])
            .collect()
    };
    println!("{}", formatter.header("Transaction summary"));
    println!(
        "{}\n",
        formatter.table(&["Action", "Package scope", "Mode"], &rows)
    );
    io::stdout().flush()?;
    Ok(())
}

fn confirmed() -> Result<bool> {
    eprint!("Is this ok [y/N]: ");
    io::stderr().flush()?;
    let mut answer = String::new();
    io::stdin().read_line(&mut answer)?;
    let yes = is_affirmative(&answer);
    if !yes {
        println!("Transaction cancelled.");
    }
    Ok(yes)
}

fn is_affirmative(answer: &str) -> bool {
    matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

fn complete(config: &Config, preview: bool) {
    let formatter = DnfStyleFormatter::new(config);
    if preview {
        println!("\nPreview complete. No packages changed.");
    } else {
        println!("\n{}", formatter.success("Complete!"));
    }
}

#[cfg(test)]
mod tests {
    use super::is_affirmative;

    #[test]
    fn only_explicit_yes_confirms_a_transaction() {
        for affirmative in ["y", "yes", " YES\n"] {
            assert!(is_affirmative(affirmative));
        }
        for negative in ["", "\n", "no", "anything else"] {
            assert!(!is_affirmative(negative));
        }
    }
}
