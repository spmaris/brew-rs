# brewrs

**A native macOS package manager in Rust, backed by Homebrew’s official repositories.** Install verified bottles and supported casks into a separate prefix, preview changes in compact transaction tables, and audit what could migrate from an existing Homebrew setup. `brewrs` does not invoke `brew`.

This is an experimental package manager, not a drop-in Homebrew replacement.

## Try it in a disposable prefix

On macOS with Rust and the Apple Command Line Tools:

```sh
cargo build --release --locked
sh scripts/demo.sh
```

The live demo previews, installs, and removes ripgrep using current official bottle metadata. It tests PCRE2 search and manpage discovery while keeping Homebrew off `PATH`. It uses a private home and prefix under `target/`, never starts a service, and prints where to inspect its files. Network access is required; it does not inherit proxy or config environment variables from the caller.

Verified on Apple Silicon with macOS 15: ripgrep `16.0.0` and its PCRE2 dependency installed, a PCRE2 search matched, `man` found `rg.1`, and removal left no managed packages in the trial prefix.

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
brewrs install --cask keepingyouawake # Install a supported app under <prefix>/Applications
brewrs list                        # List formulae and casks in the isolated prefix
brewrs list --outdated             # Show available native formula and cask upgrades
brewrs check-update [--greedy]     # Exit 100 when there are upgrades
brewrs upgrade --dry-run [--greedy] # Preview dependency-safe upgrades
brewrs remove --dry-run hello       # Preview native removal
brewrs remove --cask firefox        # Remove a brewrs-managed cask cleanly
brewrs autoremove --dry-run         # Preview removal of unused dependencies
brewrs shellenv                     # Print PATH and BREWRS_PREFIX exports
```

`install`, `upgrade`, `remove`, and `autoremove` show a native plan and ask before applying it. Use `--dry-run` (`-n`) for a preview or `--assumeyes` (`-y`) for an unattended run. Install and upgrade previews may query the repository. Removal is offline. `autoremove` removes dependencies no requested package needs, directly or indirectly. `remove --autoremove` removes only dependencies made redundant by that transaction, and preserves dependencies of packages left installed. Explicitly installing an already-installed dependency marks it as requested when the resolved bottle and dependency pins match.

`check-update` (also `check-upgrade`) exits `100` when it finds upgrades, `0` when none are available, and `1` on an operational failure. `list --outdated` shows the same plan with exit `0` on success. `brewrs update` refreshes search's catalog cache; upgrade checks query formula and cask metadata directly.

Tables use aligned columns and one separator, wrap descriptions, and keep package identifiers intact. Narrow terminals fall back to stacked fields; redirected output stays plain. Use `--no-color` or `NO_COLOR` to turn off color. Search caches formula and cask catalogs for 24 hours. `brewrs clean` clears expired catalogs and native bottle/cask or legacy bottle archives; `--all` clears those entries regardless of age. Use `BREW_RS_CONFIG` to select a different config file.

## Bottle and cask support and limits

```sh
brewrs plan ripgrep                  # Resolve versions and runtime dependencies
brewrs fetch ripgrep                 # Cache SHA-256-verified bottle archives
brewrs plan --target arm64_sequoia ripgrep
```

`plan` and `fetch` support `homebrew/core` formulae with stable bottles for the selected platform, or universal bottles. Verified archives go into the `native-bottles` cache; these commands never unpack or install packages.

Installation supports portable macOS bottles (`:any_skip_relocation`) and general macOS relocatable (`:any`) bottles. For `:any` bottles, brewrs rewrites verified thin 64-bit Mach-O images (including `LC_ID_DYLIB`, `LC_LOAD_*DYLIB`, `LC_RPATH`, `@loader_path`, and `@rpath` validated against pinned keg `LC_RPATH`s) and UTF-8 text files (`lib/pkgconfig/*.pc`, `share/pkgconfig/*.pc`, CMake/config scripts, and `bin/pcre2-config`) in private staging, verifies all rewritten targets stay inside version-pinned kegs (or `<prefix>/lib` for prefix rpaths), and ad-hoc signs and verifies modified binaries with Apple's tools. Relocation requires the Apple Command Line Tools. Planning also accepts optional macOS `service` declarations (noting they are not started automatically), build/test-only and `no_linkage` `uses_from_macos` entries, verified macOS-supplied `uses_from_macos` system libraries (`bzip2`, `cups`, `curl`, `expat`, `krb5`, `libedit`, `libffi`, `libiconv`, `libxml2`, `libxslt`, `ncurses`, `openldap`, `python`, `zlib`) with empty or satisfied `since` bounds, and `macos`, `arch`, or build/test-only `requirements`. `info`, `deps`, `plan`, `fetch`, `install`, and `upgrade` fall back to the formula catalog cache to resolve formula aliases and old names (such as `kubectl` -> `kubernetes-cli` and `nvim` -> `neovim`) when the per-formula endpoint returns 404.

Native cask management (`install --cask`, `list --cask`, `upgrade --cask [--greedy]`, `remove --cask`, `check-update [--greedy]`) supports official `homebrew/cask` macOS casks with mandatory 64-character SHA-256 verification (`sha256: no_check` is refused) over public HTTPS URLs. Verified downloads are cached under `native-casks`, safely unpacked across naked binaries, ZIP archives (Central Directory preflighted before `/usr/bin/ditto` to preserve Apple code signatures), bounded `.tar.gz`/`.tar.bz2`/`.tar.xz` archives, and read-only `.dmg` disk images (`/usr/bin/hdiutil attach` with RAII `detach`). Supported cask artifacts are `.app` bundles (defaulting to `<prefix>/Applications`, or an explicit existing safe `--appdir`), `bin/` symlinks, safe `command_wrapper` scripts, and static `manpage`/`bash_completion`/`zsh_completion`/`fish_completion` links; `.pkg` installers, `installer` scripts, `font`, `preflight`/`postflight`, and `nested` containers remain blocked. Casks with `auto_updates: true` are skipped during `upgrade` and `check-update` unless `--greedy` is passed, and `old_tokens` fallback resolves renamed casks when querying the API.

Fixed-prefix bottles, source builds, `.pkg`/scripted casks, Homebrew Ruby taps, recommended dependencies, Homebrew hooks, and service activation are still unsupported. A service declaration does not prevent its formula from being installed when the declaration and bottle are otherwise supported. The bottle extractor accepts bounded acyclic keg-contained relative symlink chains (including forward-referenced links, directory symlinks, and keg-contained executable symlinks in `bin/` and `sbin/`) and rejects escaping links, cycles, over-deep chains, and hardlinks. A successful plan or fetch does not imply the package can be installed natively.

The store defaults to `~/Library/Application Support/brew-rs/native` on macOS; pass `brewrs --prefix /absolute/path …` for a different, empty or brewrs-owned directory. Active kegs are linked at `opt/<name>`. Non-keg-only formulae also link supported executables (`bin`, `sbin`), manpages (`share/man/man1` through `man8`), Bash/Zsh/Fish completions, and Docker CLI plugins into the same directories in the prefix. `keg_only` formulae are linked only under `opt/<name>`. Conflicting formulae (`conflicts_with`) are checked symmetrically on install and upgrade.

Run `eval "$(brewrs shellenv)"` (or pass `--shell sh|bash|zsh|fish`) to add the prefix's `bin` and `sbin` to `PATH`. To use the installed manpages explicitly, run `man -M "$BREWRS_PREFIX/share/man" <command>`. Completions are installed under `etc/bash_completion.d`, `share/zsh/site-functions`, and `share/fish/vendor_completions.d`; configure your shell to load that directory if needed. `shellenv` does not load completions or edit shell configuration files.

Native formula removal is offline and only acts on the selected prefix's receipts, kegs, and exact matching links. Receipts pin each direct dependency by version and bottle checksum. An upgrade can retain an older keg if existing receipts still reference it, without rewriting those pins. The new keg owns the active links; new installs and upgrade replacements pin only active dependencies. `deps --reverse` shows active and retained versions separately. Retained-keg garbage collection is not implemented: old kegs can continue to occupy disk space, and removal refuses to strand their recorded dependencies.

Native upgrade stages verified replacements before changing installed files, compares Homebrew's `version_scheme` ahead of numeric dotted versions (including optional `_revision`), and rejects unknown version schemes, downgrades, and changed bottles at an equal version. Failed applies attempt to restore the previous installation; a crash during replacement may still require manual recovery. The old `--legacy` adapter has been removed. brewrs never invokes `brew` and does not adopt or manage an existing Homebrew installation.

`info` and per-package `analytics` read formula or cask metadata from the official repository. Forward `deps` reports declared formula runtime dependencies, even for formulae that can't be installed natively. `deps --reverse` uses only the installed dependency receipts in the selected brewrs prefix and works offline.

## Declarative third-party taps

Use a reviewed local manifest for a specific release and architecture. Tap packages have a separate namespace and store; they never replace official formula metadata or participate in ordinary `brewrs upgrade`. The current lane supports pinned, standalone macOS Mach-O executables only, not Ruby formulae or source builds.

```sh
trial="$HOME/.brewrs-tap-trial"
brewrs --prefix "$trial" tap add pproenca/tap --manifest examples/taps/agent-tui.toml
brewrs --prefix "$trial" tap list
brewrs --prefix "$trial" tap plan pproenca/tap/agent-tui
brewrs --prefix "$trial" tap install pproenca/tap/agent-tui --dry-run
# After reviewing the publisher and digest, install without --dry-run.
# To clean up an installed package, then unregister the tap:
# brewrs --prefix "$trial" tap uninstall pproenca/tap/agent-tui
# brewrs --prefix "$trial" untap pproenca/tap
```

[`examples/taps/agent-tui.toml`](examples/taps/agent-tui.toml) pins the `1.1.0` artifacts for Apple Silicon and Intel. The Apple Silicon install and cleanup were verified without executing the downloaded binary. Registration and preview do not execute the manifest or artifact. Installation checks its declared digest and Mach-O architecture, not independent publisher trust. Use `tap list` and `tap uninstall` for these packages; ordinary `list`, `remove`, and `upgrade` manage official packages. To update a tap package, uninstall it, unregister the tap, and register a reviewed replacement manifest.

## Service trial previews

```sh
brewrs services list                # Offline inventory of local brewrs service state
brewrs services plan zellij         # Preview pinned official service metadata
brewrs services plan colima         # Preview a reviewed Colima 0.10.3 trial
```

For general formulae, `services plan` reads current official macOS service metadata without writing a cache. The formula version, version scheme, and bottle digest must match the installed receipt. The preview verifies that the declared executable is a regular executable in the pinned keg, then displays its arguments and supported declaration fields. If upstream metadata has moved on, the preview refuses rather than substituting a different version.

The Colima `0.10.3` preview works offline. It verifies the installed Colima and pinned Lima receipts and both managed executables, then proposes a separate trial home, profile, managed Lima `PATH`, and flags (`--foreground --activate=false --ssh-config=false --mount=none --template=false`). It does not test VM startup or socket paths.

`services list` reports local brewrs state directories and the reviewed Colima receipt if present, not running services or a complete inventory of formula service declarations. Neither command writes a plist or service state, calls `launchctl`, starts a process or VM, or enforces a runtime sandbox. Service activation and data migration are not implemented.

## Audit a Homebrew migration

```sh
brewrs migrate                          # Offline inventory; no changes
brewrs migrate --check                  # Check formula plans against official metadata
brewrs migrate --brew-prefix /opt/homebrew --prefix "$HOME/.brewrs-test"
```

The audit reads Cellar receipts, Caskroom records, local tap directories, and Homebrew-named launchd plists. It does not run `brew` or `launchctl`, parse services, install anything, or change either prefix. Cask records do not prove an app is present; plist filenames do not prove a service is running. Use `--service-dir <absolute-directory>` to override the usual launchd locations.

`--check` checks current metadata and dependency plans, prioritizing explicitly requested formulae and assessing recorded casks against official metadata. **Candidate** means the plan passed metadata checks, not that the same version is available or that the package will install and run. **Blocked** gives a known limitation. **Unresolved** means no reliable assessment was possible. The audit does not import Ruby tap definitions or migrate services; declarative tap registration is a separate opt-in step.

For a reversible trial, choose a fresh prefix and use absolute binary paths to test replacements:

```sh
trial="$HOME/.brewrs-test"
brewrs --prefix "$trial" install --dry-run ripgrep
brewrs --prefix "$trial" install ripgrep
"$trial/bin/rg" --version
```

This leaves the original `rg` and your shell defaults alone. If anything fails, run the original command as before and keep Homebrew’s services and data in place. Don’t remove Homebrew or switch your default `PATH` until package behavior, app data, and service rollback have been tested separately.

### Rehearse the Docker CLI without a daemon

In a fresh brewrs prefix, install `docker`, `docker-buildx`, `docker-compose`, and `docker-credential-helper`. Buildx and Compose are available as standalone binaries on the prefix’s `bin` path, but Docker does not find CLI plugins on `PATH`. To test `docker buildx` and `docker compose`, use a new, disposable Docker config, with the absolute paths substituted below:

```json
{
  "cliPluginsExtraDirs": [
    "/absolute/trial/prefix/lib/docker/cli-plugins"
  ]
}
```

Save that as `config.json` in the new directory; do not edit `~/.docker/config.json`. With `trial` set to your brewrs prefix and `config` set to the new Docker config directory, run:

```sh
export DOCKER_CONFIG="$config"
export DOCKER_HOST="unix://$config/no-daemon.sock"
export PATH="$trial/bin:/usr/bin:/bin:/usr/sbin:/sbin"
"$trial/bin/docker" --version
"$trial/bin/docker" buildx version
"$trial/bin/docker" compose version
command -v docker-credential-osxkeychain docker-credential-pass
```

These commands verify CLI and plugin discovery, not container, registry, credential-store, or Colima VM behavior. Use a temporary shell or a subshell so the environment changes do not persist.

## Development

```sh
cargo fetch --locked   # First run only, or when the lockfile changes
sh scripts/verify.sh
```

The offline suite uses temporary prefixes and checks that native commands do not call `brew`. It does not touch real installed packages.

Licensed under the [MIT License](LICENSE).
