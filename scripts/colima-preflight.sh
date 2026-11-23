#!/bin/sh
# Opt-in live install and read-only checks; never starts a daemon or VM.
set -eu

repo=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd -P)
if [ "$(uname -s)" != Darwin ] || [ "$#" -gt 1 ]; then
    printf '%s\n' 'Usage on macOS: sh scripts/colima-preflight.sh [/absolute/path/to/brewrs]' >&2
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
trial=$(mktemp -d "$repo/target/colima-preflight.XXXXXXXX")
mkdir -p "$trial/home" "$trial/docker-config/cli-plugins"
prefix="$trial/prefix"
system_path=/usr/bin:/bin:/usr/sbin:/sbin
run_brewrs() {
    env -i HOME="$trial/home" PATH="$system_path" NO_COLOR=1 \
        "$brewrs" --prefix "$prefix" "$@"
}
run_docker() {
    env -i HOME="$trial/home" NO_COLOR=1 PATH="$prefix/bin:$system_path" \
        DOCKER_CONFIG="$trial/docker-config" DOCKER_HOST="unix://$trial/no-daemon.sock" "$@"
}

printf '\nTrial: %s\n' "$trial"
run_brewrs install --assumeyes colima docker docker-buildx docker-compose docker-credential-helper
run_brewrs services list
run_brewrs services plan colima
for plugin in docker-buildx docker-compose; do
    test -L "$prefix/lib/docker/cli-plugins/$plugin"
    ln -s "$prefix/lib/docker/cli-plugins/$plugin" "$trial/docker-config/cli-plugins/$plugin"
done
run_docker "$prefix/bin/docker" --version
run_docker "$prefix/bin/docker" buildx version
run_docker "$prefix/bin/docker" compose version
run_docker /bin/sh -c 'command -v docker-credential-osxkeychain && command -v docker-credential-pass'
test ! -e "$trial/no-daemon.sock"
test ! -e "$prefix/.brewrs/services"
test ! -e "$prefix/.brewrs/service-trials"
run_brewrs remove --assumeyes --autoremove colima docker docker-buildx docker-compose docker-credential-helper
run_brewrs list
printf '\nPreflight passed. No VM started and no daemon or credential store contacted.\n'
printf 'Trial directory (including the now-dangling test plugin links): %s\n' "$trial"
