#!/bin/sh
# Open a project's existing PM on the organization's remote app-server.
# CODEXT_ORG_ACCOUNT_PROFILE is an explicit remote profile change, not an ATM
# gateway account selector. Ordinary launches preserve the PM's saved binding.
set -eu
umask 077

HOST="${CODEXT_ORG_HOST:-codext-org@192.168.0.134}"
DIR="$HOME/.codext-org"
SOCK="$DIR/app.sock"
CTL="$DIR/ssh.ctl"
REMOTE_SOCK=/run/user/1002/codext-org/app.sock
PM_CWD=/home/codext-org/org/pm-codext
KNOWN="$HOME/.codex/skills/fleet/assets/studio-known-hosts"

# An account selected for the caller's personal session is not an instruction
# to rebind an organization PM. A dedicated variable makes that choice explicit.
unset CODEX_ACCOUNT_PROFILE
if [ -n "${CODEXT_ORG_ACCOUNT_PROFILE:-}" ]; then
  case "$CODEXT_ORG_ACCOUNT_PROFILE" in
    .|..|*/*)
      printf '%s\n' 'codext-org: invalid CODEXT_ORG_ACCOUNT_PROFILE' >&2
      exit 64
      ;;
  esac
  export CODEX_ACCOUNT_PROFILE="$CODEXT_ORG_ACCOUNT_PROFILE"
fi

case "${CODEXT_ORG_PROJECT:-}" in
  .|..|*[!a-zA-Z0-9_.-]*)
    printf '%s\n' 'codext-org: invalid CODEXT_ORG_PROJECT' >&2
    exit 64
    ;;
esac

ssh_() {
  ssh -o BatchMode=yes -o StrictHostKeyChecking=yes -o UserKnownHostsFile="$KNOWN" \
      -o HostKeyAlias=fleet-dell-7920 "$@"
}

mkdir -p "$DIR/home"
chmod 700 "$DIR"
if [ ! -f "$DIR/home/config.toml" ]; then
  cat > "$DIR/home/config.toml" <<'EOF'
# Private config for the remote organization TUI.
approval_policy = "never"
sandbox_mode = "danger-full-access"
check_for_update_on_startup = false

[features]
codext_auth_reload = false
EOF
fi

if ! ssh_ -S "$CTL" -O check "$HOST" 2>/dev/null; then
  rm -f "$SOCK" "$CTL"
  ssh_ -f -N -M -S "$CTL" -o ExitOnForwardFailure=yes -o StreamLocalBindUnlink=yes \
      -o ServerAliveInterval=30 -L "$SOCK:$REMOTE_SOCK" "$HOST"
fi

# Project names select files written by dell/add-pm.py. Fail before launching
# the TUI if the PM record is absent or malformed, rather than opening a new PM.
PM_FILE=".pm-thread${CODEXT_ORG_PROJECT:+-$CODEXT_ORG_PROJECT}"
OUT=$(ssh_ -S "$CTL" "$HOST" "cat '$PM_CWD/$PM_FILE' && python3 -I /opt/codext-org/bin/watchdog.py --summary")
PM=$(printf '%s\n' "$OUT" | head -n 1)
PM_HEX=$(printf '%s' "$PM" | tr -d '-')
if [ "${#PM_HEX}" -ne 32 ]; then
  PM=
fi
case "$PM" in
  *[!a-fA-F0-9-]*) PM= ;;
esac
case "$PM" in
  ????????-????-????-????-????????????) ;;
  *)
    printf '%s\n' 'codext-org: missing or invalid PM session ID; no conversation opened' >&2
    exit 65
    ;;
esac
ALERT=$(printf '%s\n' "$OUT" | tail -n +2)
if [ -n "$ALERT" ]; then
  printf '%s\n' "$ALERT"
fi
export CODEX_HOME="$DIR/home"
export CODEX_SQLITE_HOME="$DIR/home"
exec "$HOME/.local/bin/codex" --remote "unix://$SOCK" --cd "$PM_CWD" resume "$PM" "$@"
