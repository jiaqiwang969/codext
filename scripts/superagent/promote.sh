#!/usr/bin/env bash
# Builds a local Codext package from this checkout and promotes it: the lab daemon first, then
# the daily daemon and the daily `codex` / `codext` binaries.
#
#   scripts/superagent/promote.sh build            build pkg-<rev> for the current HEAD
#   scripts/superagent/promote.sh lab <account>    point the lab at pkg-<rev>, update its daemon
#   scripts/superagent/promote.sh daily            update the daily daemon and ~/.local/bin
#
# Packages go to $CODEXT_BUILD_DIR (default ~/242-superagent-codext-lab/build); PKG=<dir> picks
# another package. The lab is $CODEXT_LAB (default ~/242-superagent-codext-lab). Try a package
# in the lab before the daily environment. Updating a daemon restarts it.
set -euo pipefail

repo=$(cd "$(dirname "$0")/../.." && pwd)
lab=${CODEXT_LAB:-$HOME/242-superagent-codext-lab}
build_dir=${CODEXT_BUILD_DIR:-$lab/build}
rev=$(git -C "$repo" rev-parse --short=10 HEAD)
pkg=${PKG:-$build_dir/pkg-$rev}
toolchain=${CODEXT_RUST_TOOLCHAIN:-1.95.0}

die() { printf 'promote: %s\n' "$*" >&2; exit 1; }
sha() { shasum -a 256 "$1" | cut -c1-12; }
need_pkg() { [ -x "$pkg/bin/codex" ] || die "no package at $pkg; run: promote.sh build"; }
# The binary a home's daemon runs: <home>/packages/app-server-daemon/current/bin/codex.
daemon_sha() { sha "$(cd "$1/packages/app-server-daemon/current" && pwd -P)/bin/codex"; }

case "${1:-}" in
build)
    rg_bin=${CODEXT_RG_BIN:-$lab/build/current/codex-path/rg}
    [ -x "$rg_bin" ] || rg_bin=$(command -v rg) || die "rg not found"
    cargo_bin=$(rustup which --toolchain "$toolchain" cargo) \
        || die "Rust $toolchain is not installed (rustup toolchain install $toolchain)"
    target=$("$(dirname "$cargo_bin")/rustc" -vV | sed -n 's/^host: //p')
    [ -z "$(git -C "$repo" status --porcelain --untracked-files=no)" ] \
        || die "uncommitted changes; a package is named after its commit"
    mkdir -p "$build_dir/tmp"
    base_version=$(python3 -c 'import pathlib, sys, tomllib; print(tomllib.loads(pathlib.Path(sys.argv[1]).read_text())["workspace"]["package"]["version"])' "$repo/codex-rs/Cargo.toml")
    log=$build_dir/package-$rev.log
    echo "building $pkg ($target); log: $log"
    (
        cd "$repo"
        PATH="$(dirname "$cargo_bin"):$PATH" CODEX_REPO_ROOT="$repo" \
            CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=0 TMPDIR="$build_dir/tmp" \
            nice -n 10 python3 -I scripts/build_codex_package.py --target "$target" \
            --cargo-profile dev --strip none --package-version "$base_version-$rev" \
            --package-dir "$pkg" --force --rg-bin "$rg_bin"
    ) >"$log" 2>&1 || { tail -20 "$log" >&2; die "build failed"; }
    echo "built $pkg (codex $(sha "$pkg/bin/codex"))"
    ;;
lab)
    account=${2:-}
    [ -n "$account" ] || die "usage: promote.sh lab <account>"
    need_pkg
    [ -x "$lab/bin/codext-lab" ] || die "no lab launcher at $lab/bin/codext-lab"
    ln -sfn "$pkg" "$lab/build/current"
    "$lab/bin/codext-lab" "$account" app-server daemon update --from-cli --yes | tail -1
    [ "$(daemon_sha "$lab/.codex")" = "$(sha "$pkg/bin/codex")" ] \
        || die "the lab daemon does not run $pkg"
    echo "lab daemon runs $(basename "$pkg")"
    ;;
daily)
    need_pkg
    (cd "$HOME" && env -u CODEX_HOME -u CODEX_SQLITE_HOME -u CODEX_ACCOUNT_PROFILE \
        "$pkg/bin/codex" app-server daemon update --from-cli --yes) | tail -1
    sleep 4
    [ "$(daemon_sha "$HOME/.codex")" = "$(sha "$pkg/bin/codex")" ] \
        || die "the daily daemon does not run $pkg"
    bin=$HOME/.local/bin
    mkdir -p "$bin"
    for name in codex codext; do
        # One step back: the previous binary stays as <name>.bak.
        if [ -e "$bin/$name" ]; then cp -p "$bin/$name" "$bin/$name.bak"; fi
        cp "$pkg/bin/codex" "$bin/$name.new"
        chmod 755 "$bin/$name.new"
        mv "$bin/$name.new" "$bin/$name"
    done
    echo "daily daemon and $bin/{codex,codext} run $(basename "$pkg") (codex $(sha "$bin/codex"))"
    ;;
*)
    sed -n '2,11p' "$0" >&2
    exit 64
    ;;
esac
