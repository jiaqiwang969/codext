#!/bin/sh
# Launch an account-pool profile with the installed development candidate.
set -eu
umask 077

codex_home=${CODEX_HOME:-$HOME/.codex}
codex_bin=${CODEX_ACCOUNT_NEXT_BIN:-$HOME/.local/bin/codex}
status_bin=${CODEX_ACCOUNT_STATUS_BIN:-$HOME/.local/bin/codex-account-status}

usage() {
    printf '%s\n' \
        'Usage: codex-account-next <account> [codex arguments...]' \
        '       codex-account-next list [options]     show live account status' \
        '       options: --local, --json, --timeout SECONDS, --workers N'
}

if [ "$#" -ge 1 ] && [ "$1" = list ]; then
    shift
    if [ ! -x "$status_bin" ]; then
        printf 'codex-account-next: missing status helper: %s\n' "$status_bin" >&2
        exit 69
    fi
    exec "$status_bin" --home "$codex_home" --binary "$codex_bin" "$@"
fi

if [ "$#" -lt 1 ]; then
    usage >&2
    exit 64
fi

account=$1
shift
case "$account" in
    ""|.|..|*/*)
        printf 'codex-account-next: invalid account name: %s\n' "$account" >&2
        exit 64
        ;;
esac

# Credential commands must also work before the first login or after logout.
# The CLI scopes their storage to accounts/<account>/auth.json. Interactive
# sessions keep shared configuration/history and select the daemon profile.
exec env \
    CODEX_HOME="$codex_home" \
    CODEX_SQLITE_HOME="${CODEX_SQLITE_HOME:-$codex_home}" \
    CODEX_ACCOUNT_PROFILE="$account" \
    "$codex_bin" "$@"
