#!/bin/sh
# Live, opt-in smoke test of the official ripgrep bottle on macOS.
set -eu

repo=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd -P)
if [ "$(uname -s)" != Darwin ]; then
    printf '%s\n' 'This demo needs macOS and the Apple Command Line Tools.' >&2
    exit 1
fi
if [ "$#" -gt 1 ]; then
    printf '%s\n' 'Usage: sh scripts/demo.sh [/absolute/path/to/brewrs]' >&2
    exit 2
fi
brewrs=${1:-"$repo/target/release/brewrs"}
case "$brewrs" in
    /*) ;;
    *) printf '%s\n' 'Pass an absolute brewrs executable path.' >&2; exit 2 ;;
esac
if [ ! -f "$brewrs" ] || [ ! -x "$brewrs" ]; then
    printf '%s\n' 'Build brewrs first: cargo build --release --locked' >&2
    exit 1
fi

mkdir -p "$repo/target"
trial=$(mktemp -d "$repo/target/portfolio-demo.XXXXXXXX")
mkdir "$trial/home"

system_path=/usr/bin:/bin:/usr/sbin:/sbin
run_brewrs() {
    env -i HOME="$trial/home" PATH="$system_path" NO_COLOR=1 \
        "$brewrs" --prefix "$trial/prefix" "$@"
}

printf '\nTrial: %s\n' "$trial"
run_brewrs install --dry-run ripgrep
test ! -e "$trial/prefix"
run_brewrs install --assumeyes ripgrep
run_brewrs list
printf 'brewrs trial\n' | env -i HOME="$trial/home" PATH="$system_path" \
    "$trial/prefix/bin/rg" -P 'brewrs(?= trial)'
env -i HOME="$trial/home" PATH="$system_path" \
    /usr/bin/man -M "$trial/prefix/share/man" -w rg
run_brewrs remove --assumeyes --autoremove ripgrep
test ! -L "$trial/prefix/bin/rg"
test ! -L "$trial/prefix/share/man/man1/rg.1"
run_brewrs list
printf '\nDemo passed. Trial directory and cache left for inspection: %s\n' "$trial"
