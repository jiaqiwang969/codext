# Authentication

For information about Codex CLI authentication, see [this documentation](https://developers.openai.com/codex/auth).

## Account profiles

Set `CODEX_ACCOUNT_PROFILE` to a single account directory name to use a named
profile. For example:

```sh
CODEX_ACCOUNT_PROFILE=adantruden4b codext login
CODEX_ACCOUNT_PROFILE=adantruden4b codext login status
CODEX_ACCOUNT_PROFILE=adantruden4b codext
```

`login` (including browser, device-code, and supplied-token flows), `login status`,
and `logout` use `CODEX_HOME/accounts/<profile>/auth.json` with file credential
storage. Settings are still loaded from the shared configuration. Without the
environment variable, these commands retain their default account behavior.

Named-profile logins stage new credentials in a private temporary directory
inside the profile. For an existing ChatGPT profile, the account/workspace ID
and user ID (or email when no user ID is available) must match the stored login
before `auth.json` is atomically replaced. A failed, cancelled, or mismatched
login keeps the previous credentials. The check reads the credential files,
not an inherited access-token environment variable. A new profile accepts its
first login; successful ChatGPT login prints the actual account and destination.
Use a different profile name to add a different account.

Concurrent CLI logins to the same profile are rejected. If another completed
login or logout changes the profile while a login is in progress, that login
does not overwrite it. Normal token rotations of the existing login are allowed.

Profile startup refreshes expired credentials before reading the account. A
profile whose refresh token is permanently invalid must be logged in again;
the TUI reports this instead of opening onboarding against the shared account.
An account launcher must allow `login` even when the profile has no `auth.json`
yet, including after `logout`.

Install the account-pool launchers and status helper together:

```sh
python3 scripts/install-codex-account-tools.py
```

`codex-account-next` pins the pool to `~/.codex` for listing, login, logout, and
interactive sessions. Inherited `CODEX_HOME` and `CODEX_SQLITE_HOME` cannot redirect
these commands. Credentials belong in `~/.codex/accounts/<profile>/auth.json`;
configuration and conversation history stay in the shared home. For example,
`codex-account-next adantruden4b login` writes the same credentials that
`codex-account-next adantruden4b` reads. It defaults to `~/.local/bin/codex` and
accepts `CODEX_ACCOUNT_NEXT_BIN` to select another candidate binary.

An account explicitly selected for CLI resume overrides the conversation's saved
account:

```sh
codex-account-next IrmaCattrel resume <session-id>
```

Startup binds that conversation to `IrmaCattrel` before resuming it and verifies
the actual binding before accepting input. The session ID, history, and settings
are preserved. This also works for an unloaded conversation whose previous
profile has been removed; only the selected profile needs usable credentials.
The CLI and running daemon both need this update for account selection before
cold resume. Switching a task with running work or agents returns an error.

Ordinary resume without an explicit account, automatic reconnect, and navigation
to other conversations retain their saved bindings. A later `/profile` selection
therefore survives reconnect instead of being replaced by the original launcher
account. The explicit startup account also survives an unarchive-and-resume retry.

The installer backs up existing launchers and makes `codex-account`,
`codex-account-pool`, and `codex-account-next-1` aliases of `codex-account-next`.
`codex-account <profile> status` remains an alias for `login status`. The old
`codex-account-next-1` isolated daemon is no longer started by that command.
Direct `codext` invocations still honor an explicitly chosen `CODEX_HOME`.

`codex-account-next list` includes every directory in the main account pool and
every enabled Codex profile in the deadline ledger. Names are matched without
case sensitivity. Missing credentials remain visible as `missing` with a login
hint. The ledger supplies expected names and subscription dates, never a second
credential location. Old homes and backup directories are not active sources.

Paid accounts sort by their nearest quota reset time, soonest first, in table and
JSON output. When several quota windows exist, the earliest known reset is used.
Missing reset times sort after known times, with account names breaking ties.
Free-plan accounts remain at the end. Live plan data takes priority over local
token claims; `--local` uses those cached claims. Paid accounts with exhausted
quota and accounts with an unknown plan remain visible.

Live queries use a dedicated connection to the running shared daemon for each
profile. They share its profile credential manager and refresh lock instead of
starting independent app-server processes that can compete to rotate the same
refresh token. These query connections never select an account for an existing
conversation or change the default account. The CLI and status helper must be
updated together with this version.

When the daemon is unavailable, accounts remain listed with an error and local
metadata. Open an account session to start the daemon, or use `list --local` for
an offline inventory. The helper does not fall back to independently refreshing
credentials.

`*` identifies the default account stored in the shared `auth.json`, matched by
account ID and user ID (or email), rather than by the credential file's bytes.
Token rotation therefore does not remove the marker. Individual conversations
may use other profiles. JSON exposes this as `default`; the existing `active`
field is retained as a compatibility alias with the same meaning.
