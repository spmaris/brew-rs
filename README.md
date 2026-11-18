# brew-rs

`brewrs` aims to be to Homebrew what `uv` is to pip: a fast CLI with good defaults and readable output. Search uses a local catalog cache, package tables adapt to the terminal, and Homebrew still owns installs, upgrades, and installed state. You don't need Fedora, DNF, or `uv`.

## Install

With Rust 1.85 or newer, run from this repository. Package operations and installed inventory also need a current Homebrew installation on your `PATH`; catalog search does not.

```sh
cargo install --path . --locked
brewrs --help
```

## Everyday commands

```sh
brewrs search ripgrep              # Search formulae and casks
brewrs search --cask firefox       # Search only casks
brewrs search --offline ripgrep    # Reuse the catalog with no network
brewrs search --refresh ripgrep    # Fetch current catalogs now
brewrs info --deps ripgrep         # Show package details and dependencies
brewrs list --dates                # Installed formulae and casks with recorded UTC dates
brewrs list --sizes                # Include Homebrew's disk usage report
brewrs update                     # Refresh Homebrew metadata; no package upgrades
brewrs list --outdated             # Show available versions, including pinned packages
brewrs check-update --greedy       # Include auto-updating and unversioned casks
brewrs upgrade --dry-run           # Preview upgrades without applying them
brewrs install ripgrep             # Install a formula
brewrs install --cask firefox      # Install a cask
brewrs remove --dry-run ripgrep     # Preview removal
brewrs autoremove --dry-run        # Preview removal of unneeded dependencies
```

By default, `install`, `upgrade`, `remove`, and `autoremove` show Homebrew's plan and ask before applying it. Use `--dry-run` (`-n`) to stop after the preview, or `--assumeyes` (`-y`) for an unattended run. Previews suppress Homebrew's auto-update but may still need to read or cache metadata. `remove` leaves unused dependencies in place unless you pass `--autoremove`.

`check-update` (also `check-upgrade`) exits `100` when it finds updates, `0` when none are available, and `1` on an operational failure. Use `list --outdated` if you want the same default list with exit `0` on success. Run `brewrs update` first if you want to refresh Homebrew itself.

Interactive tables use a compact frame, wrap descriptions, and keep package identifiers intact. Narrow terminals fall back to stacked fields; redirected output uses plain aligned columns. Use `--no-color` or `NO_COLOR` to turn off color. Search caches formula and cask catalogs for 24 hours. `brewrs clean` clears expired wrapper entries; `brewrs clean --all` clears the whole wrapper cache without touching Homebrew's cache. Use `BREW_RS_CONFIG` to select a different config file.

## Repository and engine

The catalog comes directly from the Homebrew API for `homebrew/core` and `homebrew/cask`; it doesn't require the `brew` binary. The repository and the installation engine are separate choices. Today, transactions still use `brew` for compatibility with existing installs, taps, and cask behavior. A native Rust engine could start with planning and verified bottle downloads against the same repository, before taking responsibility for dependency resolution, linking, receipts, or uninstalling.

## Development

```sh
cargo test --locked
```

CLI integration tests run against a mock `brew` on an isolated `PATH`. They do not modify installed packages.
