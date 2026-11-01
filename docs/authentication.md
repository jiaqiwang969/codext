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

The development launcher is tracked in `scripts/codex-account-next.sh`. Install
it as `~/.local/bin/codex-account-next`; it defaults to `~/.local/bin/codex` and
accepts `CODEX_ACCOUNT_NEXT_BIN` to select another candidate binary. For example,
`codex-account-next adantruden4b login` uses the same profile as
`codex-account-next adantruden4b`.
