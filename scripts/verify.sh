#!/bin/sh
# Run the local release gates without changing the repository configuration.
set -eu

repo=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd -P)
cd "$repo"

git diff --check HEAD --
cargo fmt --all --check
cargo clippy --locked --offline --all-targets -- -D warnings
cargo test --locked --offline --quiet
RUSTFLAGS="${RUSTFLAGS:+$RUSTFLAGS }-D warnings" cargo build --release --locked --offline
