# Reapply on rust-v0.166.0

The new branch starts at official `rust-v0.166.0` (`6ff1e1ca3b30`), with no merge, rebase, or cherry-pick of the old fork history. The previous fork is retained as `backup/pre-0.166.0-20261104` (`95a0539ce1cc`). Loongphy's `84266ff063df` is a reference for its shared 0.166 adaptations; our account-pool behavior remains part of this fork.

## Preserved behavior and adaptations

| Requirement | 0.166 implementation |
| --- | --- |
| Shared home, isolated account credentials | Launchers still pin `~/.codex`; profile login/status/logout use `accounts/<name>/auth.json`. The new browser/device login coordinator retains staging, identity validation, and atomic replacement. |
| Current-thread profile switching | The idle transition replaces account-aware services and waits for the new upstream shutdown-drain watch before replacement. The acknowledgement includes the target thread. |
| Explicit account on cold resume | The launcher binds the thread before resume; durable bindings take precedence over historical creator metadata. Old-profile credentials are not needed to select a replacement. |
| Runtime authentication and tools | Session extension data owns the account runtime; model, plugin, MCP, web, image, history, git attribution, and Guardian clients use that runtime. Children inherit it. There is no separate runtime map to race with upstream removal/replacement. |
| Routing and login isolation | Profile routing uses its own credential manager, including 0.166 permanent refresh-token diagnostics. Routing cache keys include the credential manager. New workspace-session mutations cannot run under a selected profile and overwrite shared credentials. |
| Account listing | All configured profiles remain visible; paid accounts sort by the nearest quota reset, free accounts follow. Live reads share daemon refresh locks, with local fallback metadata and an offline mode. |
| TUI | Header, git-cwd tracking, copy draft, rename, account picker, agent overview, and quota recovery are preserved on the app-server-backed TUI. There is no parallel `tui_app_server` crate in this tag. |
| Overload recovery | Retry the original request with bounded delays; do not restore Loongphy's synthetic `Continue` retry. |
| Discovery limits | Preserve the enlarged skill-entry and filesystem-walk budgets. |

Upstream's periodic rate-limit reads already provide the header's refresh cadence; no independent fork poller is added. Upstream thread shutdown, daemon recovery, token accounting, and request error diagnostics remain in place. The obsolete `SERVER_FEATURES` reference is removed from the fork notes.

## Release review

Inputs: official `rust-v0.165.0` → `rust-v0.166.0`, with `95a0539ce1cc:.github/workflows/rust-release.yml` as the existing fork release baseline.

| Decision | Change and reason |
| --- | --- |
| APPLIED | Rust 1.95.0; Node version from `.nvmrc` (24.22.1). |
| APPLIED | Retain `setup-rusty-v8` on every release target. The 0.166 action selects OpenAI's sandbox/pointer-compression archive and matching bindings, verifies the release manifest against committed SHA-256 values, then exports both Cargo overrides. |
| APPLIED | Build `codex`, `codex-code-mode-host`, and `codex-exec-server` together. Trace the executor through artifact upload/download, symbols, canonical packages, and GitHub archives. Windows also retains both sandbox helpers. |
| APPLIED | npm strips the duplicate standalone executor and the optional `execServer` manifest field; the entrypoint's `exec-server` command supplies it, following upstream. |
| APPLIED | Unix launcher uses `execve` when available; older Node/Windows retain child-process signal forwarding and signal-derived failure status. Windows release checkout enables long paths. |
| OVERRIDE | Keep `@loongphy/codext`, the `codext` command, continuous `main` publication, existing musl support, package aliases, and the fork's intentionally reduced workflow set. `release-overrides.json` is unchanged. |
| NOT_APPLIED | Upstream macOS signed/DMG verification accepts an absent entitlement blob when the expected plist is empty. This fork has no corresponding signed/DMG release job, so there is no applicable verification step. |
| NOT_APPLIED | Independent `codex-app-server` bundle and responses proxy remain outside this fork's product set; the CLI includes its app-server command and all required companions. |
| ALREADY_SATISFIED | Upstream removed the old V8 libc++ Bazel configuration helper; the fork's retained staging helper already does not invoke it. |

The npm and GitHub paths both consume upstream's canonical package builder. Linux still compiles, strips, and hashes the exact shipped `bwrap` before compiling the CLI.

## Interface artifacts and acceptance

The three fork RPC types and `ThreadExtra` fields retain their existing wire contract. Stable and experimental JSON/TypeScript bundles are refreshed against the 0.166 generated definitions, including the new `Account.workspaceName`. This also restores `account/reload` to the experimental embedded exports, where the old bundle omitted it. The Python SDK is regenerated without formatters; unchanged declarations retain their existing text.

The upstream app-server schema writer is now exposed only through a test target. The reapply guardrail prohibits compiling test targets, so this update refreshes the reviewed, unchanged fork schema additions directly against the new upstream export trees instead. No test or snapshot sources are changed.

Acceptance requires `CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=0 cargo build -p codex-cli`, release artifact preflight, and dependency lock verification. No tests, formatters, linters, or automatic fixes are run under the reapply guardrail. GitHub Actions monitoring is a separate follow-up.

Local companion-binary builds also need the V8 artifact overrides from `.github/actions/setup-rusty-v8/action.yml`: `RUSTY_V8_ARCHIVE` and `RUSTY_V8_SRC_BINDING_PATH`. Use the target-matching `ptrcomp_sandbox_release` pair from OpenAI's `rusty-v8-v152.2.0` release and verify its checksums as that action does. Without the overrides, Cargo attempts a default Deno URL that has no sandbox artifact for this version. A legacy non-sandbox archive is not an ABI-compatible replacement.

Verified on macOS ARM64 with Rust 1.95.0:

- The workspace invocation building all three shipped binaries succeeds with incremental compilation and debug information disabled. Local packaging consumes that binary set.
- The required separate `cargo build -p codex-cli` invocation succeeds with `CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=0`.
- `just write-config-schema`, `just bazel-lock-update`, `just bazel-lock-check`, and the release artifact preflight succeed. The Bazel lockfile has no resulting change.
- CLI version output reports `0.166.0`; read-only login status recognizes the selected named profiles. Offline account discovery returns all 11 profiles, with the free account last.
- No tests or snapshots are added, modified, or run. These checks establish build, packaging structure, and passive account discovery; they do not replace interactive account-switching or cross-platform runtime coverage.
