# Changes in This Fork

This file captures the fork-specific behavior reapplied on top of the current upstream tag, `rust-v0.166.0`. The reapply and release decisions are recorded in [docs/reapply-0.166.0.md](docs/reapply-0.166.0.md). Superagent's requirements are the last section; its seams into codex are listed in [docs/superagent/seams.md](docs/superagent/seams.md).

## TUI composer draft clipboard shortcut

- Added `Ctrl+Shift+C` in the TUI composer to copy the current draft to the system clipboard when the input contains text.
- Existing `Ctrl+C` behavior stays unchanged.
- When the composer has no copyable text, `Ctrl+Shift+C` falls back to the existing `Ctrl+C` clear/interrupt/quit path.
- On WSL2, composer draft copy reuses the existing Windows clipboard fallback so copies still land in the Windows system clipboard.
- `Ctrl+Shift+C` now takes its own composer-copy path instead of falling through to the existing `Ctrl+C` clear/interrupt/quit behavior when draft text is present.
- Added footer shortcut help text for the new draft-copy binding.

## TUI resume picker rename

- Added `Ctrl+R` in the `/resume` picker to rename the highlighted session inline, reusing the picker search line as the editor. The draft starts empty so a new name can be typed directly.
- `Enter` submits through the existing `thread/name/set` app-server request; `Esc` cancels back to search mode.
- Empty or whitespace-only names are rejected inline; failed requests keep the editor open so the rename can be retried.
- On success the row updates in place, the search filter re-applies, and selection stays on the renamed thread when possible.
- The shortcut only claims `Ctrl+R` when no existing list keymap or custom binding uses it; archived sessions are read-only and cannot be renamed.

## TUI status header and polling

Implementation must follow the status-header skill .agents/skills/status-header/SKILL.md

- Added a status header above the composer in the app-server-backed `codex-rs/tui` surface. Segment order is fixed as model + reasoning effort, current directory, git branch/ahead/behind/changes, rate-limit remaining/reset time, then account identity.
- Status header account identity is the last segment without an icon: ChatGPT accounts render as `user@example.com(Pro)` and API-key auth renders as `API key`.
- Status header layout no longer adds its own top inset; it uses `Insets::tlbr(/*top*/ 0, …, /*bottom*/ 1, …)` and relies on the existing outer bottom-section gap above it so the spacing between `Working` and the header stays compact.
- Git status is collected in the background (15s interval, 2s timeout) and rendered when available.
- The directory segment represents the session/thread `cwd`, not a one-off tool `workdir`.
- When the session `cwd` changes (for example after switching into a new worktree), the git-status poller now rebinds to that new `cwd`, clears stale git state, and ignores late results from the previous `cwd`.

## TUI auth.json watcher

- The running TUI watches the selected profile’s `accounts/<name>/auth.json`, or `CODEX_HOME/auth.json` for the default account, and reloads auth when that file changes.
- Watch notifications are now trailing-debounced so reload happens after writes settle, reducing partial-file reads.
- If `auth.json` changes while the TUI still has an active task/turn running, auth reload is deferred until that work fully finishes; Codex does not hot-swap auth in the middle of the running task.
- Auth reload failures no longer clear cached auth (so transient parse/read errors do not appear as a logout).
- On auth reload failure, the TUI retries every 5 seconds for up to 3 attempts before surfacing a final warning.
- When the account identity changes, the TUI surfaces a warning in the transcript (including old/new emails when available).
- Auth change warnings now show the account plan type (e.g., Plus/Team/Free/Pro) instead of the generic ChatGPT label.
- Rate-limit state and polling are refreshed after auth changes so the header reflects the new account.
- That post-task auth refresh also resets cached rate-limit warning/prompt state for the new auth snapshot, so stale usage-limit/UI state from the previous auth context does not keep re-triggering after the reload.
- The TUI now supports `[tui].usage_limit_resume_prompt` for the synthetic recovery user turn sent after `UsageLimitExceeded`. If the field is unset, Codext uses the built-in default recovery prompt; if the field is set to an empty string, Codext disables the automatic recovery turn.
- When a turn hits `UsageLimitExceeded`, the TUI now queues that synthetic recovery turn ahead of other queued user input. If an `auth.json` reload is also pending, the reload still runs first, and only then does Codext submit the recovery turn before draining later queued inputs.
- After a turn stops on `UsageLimitExceeded`, Codext now keeps that synthetic recovery turn parked until the next `auth.json` reload that actually changes account identity, so switching accounts can continue the interrupted task without a manual resend.
- If the user manually submits a new message before that auth reload arrives, Codext clears the parked usage-limit recovery turn instead of replaying the stale synthetic prompt later.

## TUI queued messages after usage-limit exhaustion

- When a turn ends because quota/rate limit is exhausted, Codext pauses queued-message autosend instead of draining already queued Tab follow-ups into more failed turns.
- While autosend is paused, pressing Tab still queues new messages even when no turn is currently running.
- When a later Codex rate-limit snapshot shows quota available again, Codext resumes autosend and submits exactly the first queued user message; any additional queued messages remain queued for normal FIFO draining after that turn completes.
- If both a parked usage-limit recovery prompt and user-queued follow-ups exist when quota recovers, the user-queued follow-up wins and the stale synthetic recovery prompt is cleared.

## Server-overload retry

- When a turn fails with `ServerOverloaded`, Codext retries the original turn instead of submitting a synthetic `Continue` user turn.
- Retries honor the server's `Retry-After` advice and otherwise use bounded backoff of 15s → 30s → 60s → 120s → 240s, subject to the provider's stream retry limit.
- Controlled by `[tui].server_overloaded_resume` (default `true`; set to `false` to disable automatic retries).
- After retries are exhausted, the error remains visible and normal queued-input handling resumes.

## App-server auth.json account switching

- The app-server now reloads auth from storage before `thread/start`, `thread/resume`, and `turn/start` when no turn is running.
- This change supports Codex App account switching through [Loongphy/codex-auth#103](https://github.com/Loongphy/codex-auth/pull/103), allowing the app-server to pick up the newly selected account at the next safe request boundary.
- Auth is still not hot-swapped in the middle of an active turn; reload is skipped while `running_turn_count` is nonzero and the next request boundary gets the new auth.
- ChatGPT account/workspace switches inside the same auth mode are treated as auth changes by comparing the refresh-relevant auth snapshot, not only the top-level auth mode.
- When a reload changes auth, loaded threads invalidate their cached model transport state so a reused WebSocket session created under the previous account is not used for the next turn.
- The app-server also refreshes cloud requirements/default residency state and emits `AccountUpdated` after a changed reload so app UI account state follows the new snapshot.
- The TUI drives its `auth.json` watcher through a dedicated `account/reload` RPC (response: account snapshot + `authChanged`), which performs the same idle-guarded storage reload. Earlier reapplies extended `account/read` with a `reloadAuthFromStorage` param and an `authChanged` response field; the dedicated RPC keeps upstream `GetAccountParams`/`GetAccountResponse` wire shapes untouched.
- `ReloadAccountResponse` carries `authReloadSkipped`: it is `true` when the request reached the app-server but the storage reload was skipped because `running_turn_count` is nonzero (a turn running in *any* client of a shared daemon). Callers must not treat `authChanged=false` alone as "no account change".
- When the TUI sees `authReloadSkipped`, it retries the reload every 5s for up to 24 attempts instead of dropping the pending account switch; if the daemon is still busy after that, the next request-boundary `reload_auth_from_storage_if_idle` still applies the switch.
- The fork feature `codext_auth_reload` (Stable, default on) marks an app-server that implements `account/reload`. The TUI daemon compatibility check (`tui/src/daemon_startup.rs`) requires it whenever the feature is enabled, so attaching to a daemon that does not report it — a stock upstream daemon or a pre-reapply codext daemon — is treated as incompatible: implicit socket attach falls back to embedded mode, and the managed-daemon path offers the existing restart/embedded recovery. This prevents the silent "account switch has no effect" failure when a foreign daemon owns `app-server-control.sock`. Disabling the feature also disables the auth.json watcher entirely.
- `codext app-server daemon reload-auth` asks the running daemon on `app-server-control.sock` to perform the idle-guarded `account/reload` without a TUI. It reports whether the daemon reloaded (and whether auth changed), was skipped-busy, or does not implement the RPC (foreign daemon). `codex_app_server_daemon::request_auth_reload` is the library entry point; external account switchers such as codex-auth invoke the subcommand after replacing `auth.json`.
- Reapply notes: keep `reload_auth_from_storage_if_idle` wired into all three request entry points, preserve the idle guard, preserve the `auth_reload_skipped` response field and its busy-retry path in `handle_auth_file_changed`, keep `Feature::CodextAuthReload` in the feature registry and daemon compatibility handshake (0.166 no longer uses the old `SERVER_FEATURES` allowlist), and preserve the invalidation chain `ThreadManager::invalidate_model_transport_caches` -> `CodexThread::invalidate_model_transport_cache` -> `ModelClient::invalidate_cached_transport_state`.

## TUI account-pool switcher

- Added `/profile` to the TUI command palette. With no argument it opens an account picker; `/profile <name>` switches directly to an account-pool entry.
- The picker uses `CODEX_HOME/accounts/<name>/auth.json` through a profile-scoped runtime without rewriting the shared `auth.json`.
- `/profile` switches the current idle conversation as well as the connection default. The server replaces account-aware services and model transports, preserves history/settings, and acknowledges the target thread before the TUI reports success or releases a parked usage-limit recovery prompt.
- Explicit thread selections survive cold resume, daemon recovery, revert, and fork via `CODEX_HOME/thread-auth-profiles/<threadId>.json`; creator-account metadata remains historical. Other conversations keep their accounts.
- `codex-account-next <profile> resume <session>` treats the launcher's account as an explicit thread selection: bind before resume and verify the actual profile before accepting input. Unloaded threads can be rebound without the previous account's credentials or runtime. Unarchive retries retain the selection; ordinary resume, reconnect, and navigation preserve saved thread bindings. Both CLI and daemon must support account selection before cold resume.
- The installed `codext-org` launcher clears inherited personal account selections and preserves the remote PM's saved binding. Only `CODEXT_ORG_ACCOUNT_PROFILE` explicitly requests rebinding; it does not select an ATM gateway account. The launcher keeps local state in `~/.codext-org/home` and rejects missing or malformed PM IDs. The installer backs up and updates it alongside the account tools. Superagent is described in the Superagent Router section below.
- Thread reads, resume, and the account overview report the actual bound profile/account. Resuming a profiled thread also aligns the TUI connection's account reads and quota polling with that runtime.
- CLI `login`, `login status`, and `logout` honor `CODEX_ACCOUNT_PROFILE` and use that profile's file-backed credentials while retaining shared settings. Browser/device-code login must not replace the default account's `auth.json`.
- Named-profile login stages credentials before replacing the file. Existing ChatGPT profiles require the same account and user identity; failed, cancelled, or wrong-account logins retain the previous credentials. Successful login reports the actual account and path. Concurrent CLI logins are rejected, and a completed login/logout detected during the flow prevents replacement.
- Profile startup renews expired credentials and picks up a newer login for the same account before checking account identity. A permanently failed login produces an actionable error instead of sending the user through shared-account onboarding into an unauthenticated conversation.
- `codex-account-next list` includes every profile, sorts paid accounts by their soonest quota reset (earliest window first, missing times after known times), and places free-plan accounts last. Exhausted paid accounts and unknown plans remain visible. Live queries share the daemon's profile credential managers and refresh locks; they never start independent app-server credential refreshers. An unavailable daemon produces explicit errors with local metadata, and `--local` works offline.
- The list's `*` means the default account in the shared `auth.json`, matched by account/user identity rather than file equality. Token rotations preserve the marker, and the label makes clear that conversations may use other accounts. JSON adds `default` while retaining `active` as a compatibility alias.
- The account-pool launchers share one entry point pinned to `~/.codex` for login, status, listing, and interactive sessions; inherited home variables cannot redirect credentials. The installer updates compatibility aliases together. Enabled profiles in the deadline ledger remain listed as `missing` if their main credential directory disappears.
- Account-aware services belong to the live session and are inherited by internal workers, Guardian, and forked agents. The 0.166 shutdown-drain watch and conditional runtime removal keep delayed cleanup from removing a replacement session. Workspace-routing caches distinguish credential managers, even when profiles share a workspace.
- Account switching changes credentials only. Sessions, memory, SQLite state, logs, and other shared `CODEX_HOME` data remain shared across the account pool.

## TUI exit resume command

- Added a fork requirement that user-facing resume hints use `codext resume <session>` / `codext resume <thread-name>` instead of `codex resume ...`.
- This includes the final resume hint shown after exiting the TUI and other resume guidance surfaced inside the TUI.

## Update channel points at Codext releases

- Latest-version discovery queries `https://api.github.com/repos/Loongphy/codext/releases/latest` and the `@loongphy/codext` npm registry entry instead of `openai/codex` / `@openai/codex`.
- Codext release tags are `codext-v<base>-<sha>`; `extract_version_from_latest_tag` accepts both `codext-v` and `rust-v` prefixes so upstream tests and tooling stay valid.
- Update prompts compare only the base version triple (`is_newer_release` strips the `-<sha>` suffix), so pushes that rebuild the same upstream base do not retrigger the prompt.
- `codext update` and the in-TUI "Update now" action run `npm/bun/vp/pnpm install -g @loongphy/codext`; release-note and install links point at `Loongphy/codext`.
- `codext doctor` update diagnostics report the Codext channel too (latest-release probe, tag parsing, and npm-family update labels).
- Homebrew cask, standalone installer, and daemon update variants still reference upstream artifacts; they are unreachable for Codext installs because no Codext cask or standalone installer exists.
- Reapply notes: the Windows-only "absolute update command" error keeps the upstream docs URL because an existing test pins the message; snapshot files under `tui/src/snapshots` still contain upstream update URLs on purpose.

## Release artifact parity

- Release builds ship `codex-code-mode-host` and `codex-exec-server` beside the CLI. GitHub archives include the standalone executor; npm staging removes the duplicate executor and its optional manifest field, using the CLI’s `exec-server` command as upstream does.
- The launcher uses `execve` on supported Unix Node runtimes and preserves signal exit status on older Node and Windows. Release tooling uses Rust 1.95.0 and the Node version from `.nvmrc`.
- The upstream release matrix is audited during reapply instead of assuming that copying the previous fork workflow preserves all companion binaries.
- The upstream `codex-responses-api-proxy` package/binary is intentionally not shipped: it is a standalone debugging proxy, not a companion required by `codext` or `codex-code-mode-host`.

## Canonical package layout

- Every npm platform package and GitHub release archive vendors a complete codex-package root per target: `bin/codex` + `bin/codex-code-mode-host`, `codex-path/rg`, `codex-resources/` (`bwrap` on Linux, `codex-command-runner.exe` + `codex-windows-sandbox-setup.exe` on Windows), and `codex-package.json` metadata. This is the layout the app-server daemon requires when seeding its managed install; anything less fails startup with "this CLI has no complete local package" on fresh installs.
- Upstream dropped the bundled `zsh` resource from the package contract in `rust-v0.163.0`; Codext follows the new contract and no longer fetches or ships a zsh payload.
- Package trees are assembled with upstream's `scripts/build_codex_package.py`, so npm staging, release archives, and `codex-cli/scripts/install_native_deps.py` all produce the same layout.
- `codex-cli/bin/codex.js` executes `vendor/<target>/bin/codex` and prepends `vendor/<target>/codex-path` to `PATH`.
- Linux release builds compile `--bin bwrap`, strip it, and export `CODEX_BWRAP_SHA256` before building `codex`, so the digest embedded in the CLI matches the shipped `codex-resources/bwrap` bytes.
- `codex-package.json` records the codext release version (`<base>-<sha>` prerelease), which keeps the seeded daemon release off upstream's latest-channel auto-update path.
- Unix release archives ship `bin/codext` as a hardlink to `bin/codex`; the Windows zip adds a `bin/codext.exe` alias. Keeping the alias inside `bin/` preserves package-layout detection, and the hardlink extracts on Windows without the symlink privileges a root symlink would require. A root-level executable copy would not be recognized as a package entrypoint.

## Skill discovery and filesystem walk budgets

- Skill discovery permits 50,000 visited entries per root and loads at most 64 skill documents concurrently, retaining the existing depth and directory limits.
- Filesystem walk permits depth 64, 10,000 directories, 50,000 entries, and an 8 MiB response. Responses retain bounded traversal and error reporting rather than silently omitting a large skill tree.

## Superagent Router

The engine (`codex-rs/ext/superagent/`) is carried over whole on each reapply, tests included. Every seam in [docs/superagent/seams.md](docs/superagent/seams.md) must be reconnected on the new upstream code. Required behavior:

- Off by default. `[features] superagent = true` enables it inside the app-server, including the managed daemon. The ledger is `superagent_1.sqlite` in the SQLite home; a corrupt ledger or a failed migration is reported, never replaced with an empty one.
- `$CODEX_HOME/superagent/pm-threads` registers the Router thread. `$CODEX_HOME/superagent/router-mode` sets its level: `observe` (the default), `drive`, or `org` (the build-27 ticket organization beside driving).
- The Router reads every session without loading it. Its context carries its rules, an index of recently active sessions, and the sessions waiting for the person (an error, an approval, an answer).
- Driving goes through `superagent_drive`: start, say, interrupt, switch_account, feedback, pause, resume, stop, history. Start can create a folder and a new session on the Router's account; the first message is the Router's own words.
- Each finished step's conclusion (the session's last answer) reaches the Router as a `[通知]` notice card. `say` must quote what it builds on (the worker's conclusion, the person's words or corrections) and what it serves (the person's words, corrections or acceptance list). It is refused for method names, skill names, item ids, or repeating the last two steps. During a running turn, `say` is steered into that turn.
- Simple drives have one folder, no anchor, and the person accepts the result. Anchored drives use the project's `.engineering`; every step is judged by oev2, and the drive stops when it is deliverable, drifts, or makes no progress.
- Driven steps run with full access (`DangerFullAccess`, approval `never`).
- When a step fails on `usageLimitExceeded` or `serverOverloaded` ("Selected model is at capacity"), the account cools down (until its reset, or 15 minutes) and the session moves to the next account by soonest reset, skipping free accounts, accounts with under 5% left and accounts still cooling; then the step is sent again. A failed Router turn moves the Router once per failure and is delivered again.
- One driver owns a thread's automatic turns: Goal and daemon-recovery turns on engine-driven threads are declined (`DeclinedByDriver`), and the daemon snapshot skips those threads.
- After a daemon restart, drives are reconciled; a step that was handed over but never seen starting pauses with a notice instead of being replayed.
