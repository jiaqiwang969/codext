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

Free-plan accounts sort last in table and JSON output. Live plan data takes
priority over local token claims; `--local` uses those cached claims. Paid accounts
with exhausted quota and accounts with an unknown plan remain visible.

Live queries use isolated configuration and the profile's actual credential
directory, so refreshed tokens are retained in the main account pool.
