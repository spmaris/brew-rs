# brew-rs

`brewrs` is a Rust-native macOS package manager that uses Homebrew’s official repository and bottle archives. It manages its own prefix and shows compact tables of what will change. You don’t need Homebrew installed to use the native package commands.

## Install

With Rust 1.85 or newer, run from this repository:

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
brewrs info --deps ripgrep        # Read official formula metadata
brewrs deps --tree ripgrep        # Show declared runtime dependencies
brewrs deps --reverse pcre2       # Show dependents in the native prefix
brewrs analytics hello --period 90d
brewrs update                     # Refresh the local search catalogs
brewrs install --dry-run hello      # Preview a portable formula install
brewrs install hello               # Download, verify, and install it
brewrs list                        # List formulae in the isolated prefix
brewrs list --outdated             # Show available native upgrades
brewrs check-update                # Exit 100 when there are upgrades
brewrs upgrade --dry-run           # Preview dependency-safe upgrades
brewrs remove --dry-run hello       # Preview native removal
brewrs autoremove --dry-run         # Preview removal of unused dependencies
```

`install`, `upgrade`, `remove`, and `autoremove` show a native plan and ask before applying it. Use `--dry-run` (`-n`) for a preview or `--assumeyes` (`-y`) for an unattended run. Install and upgrade previews may query the repository. Removal is offline. `autoremove` removes dependencies no requested package needs, directly or indirectly. `remove --autoremove` removes only dependencies made redundant by that transaction, and preserves dependencies of packages left installed. Explicitly installing an already-installed dependency marks it as requested when the resolved bottle and dependency pins match.

`check-update` (also `check-upgrade`) exits `100` when it finds upgrades, `0` when none are available, and `1` on an operational failure. `list --outdated` shows the same plan with exit `0` on success. `brewrs update` refreshes search's catalog cache; upgrade checks query formula metadata directly.

Tables use aligned columns and one separator, wrap descriptions, and keep package identifiers intact. Narrow terminals fall back to stacked fields; redirected output stays plain. Use `--no-color` or `NO_COLOR` to turn off color. Search caches formula and cask catalogs for 24 hours. `brewrs clean` clears expired catalogs and native or legacy bottle archives; `--all` clears those entries regardless of age. Use `BREW_RS_CONFIG` to select a different config file.

## Bottle support and limits

```sh
brewrs plan ripgrep                  # Resolve versions and runtime dependencies
brewrs fetch ripgrep                 # Cache SHA-256-verified bottle archives
brewrs plan --target arm64_sequoia ripgrep
```

`plan` and `fetch` support `homebrew/core` formulae with stable bottles for the selected platform, or universal bottles. Verified archives go into the `native-bottles` cache; these commands never unpack or install packages.

Installation is currently limited to portable macOS bottles, including direct runtime dependencies whose bottles meet the same rules. It does not support relocation, fixed-prefix bottles, source builds, cask installation, third-party taps, recommended or macOS-provided dependencies, Homebrew hooks, or services. The extractor accepts simple relative symlinks within a keg and rejects link chains, escaping links, and hardlinks. A successful plan or fetch does not imply the bottle can be installed natively.

The store defaults to `~/Library/Application Support/brew-rs/native` on macOS; pass `brewrs --prefix /absolute/path …` for a different, empty or brewrs-owned directory. Binaries are linked into the prefix's `bin`. brewrs does not change your `PATH` or Homebrew's files.

Native removal needs no network and only acts on the selected prefix's receipts, kegs, and matching binary links. Receipts pin each direct dependency by version and bottle checksum. Removal refuses to leave a package without its dependencies; remove the dependent in the same command if you intend to remove both. Upgrades likewise refuse to break a pin, so related packages may need to be upgraded together. A newly required dependency must already be installed in the native prefix.

Native upgrade stages verified replacements before changing installed files, requires a strictly newer numeric dotted version (including Homebrew's optional `_revision`), and rejects unknown or nonzero version schemes, downgrades, and changed bottles at an equal version. Failed applies attempt to restore the previous installation; a crash during replacement may still require manual recovery. The old `--legacy` adapter has been removed. brewrs never invokes `brew` and does not adopt or manage an existing Homebrew installation.

`info` and per-package `analytics` read formula or cask metadata from the official repository. Forward `deps` reports declared formula runtime dependencies, even for formulae that can't be installed natively. `deps --reverse` uses only the installed dependency receipts in the selected brewrs prefix and works offline.

## Development

```sh
cargo test --locked
```

Offline tests use temporary prefixes and a sentinel `brew` on an isolated `PATH` to catch accidental subprocess calls. They do not touch real installed packages.
