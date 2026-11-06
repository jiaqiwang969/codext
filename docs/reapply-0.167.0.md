# Candidate reapply on rust-v0.167.0

This candidate starts at the official OpenAI tag `rust-v0.167.0`, peeled commit
`086ccc833bc9cf01024ad2dd6af7dbd421b6bf9a` (tag object
`cb41599ccbe371272892f156286a236aa097bada`). Both IDs were checked with
`git ls-remote` against `https://github.com/openai/codex.git` before fetching.
The previous fork is the unchanged local and remote `main` at
`31ed25924830e51d471ef2e6cc660cab3270929b`, based on `rust-v0.166.0`.
The candidate branch is `feat/rust-v0.167.0`; no old fork history was merged,
rebased, or cherry-picked, and no remote ref has been written.

## Behavior and adaptation decisions

- Preserve `codex-rs/ext/superagent/`, including all tests, byte for byte. Restore
  its state migrations, status API, extension hooks, automatic-turn ownership,
  host actions, startup reconciliation, and shutdown drain.
- Preserve account runtimes, durable per-thread selection, account overview,
  auth reload, quota recovery, original-request overload retries, draft copy,
  inline rename, the status header, and the expanded discovery budgets.
- Implement the storage reload against 0.167's managed credential loader so the
  new allowed-mode, workspace and cached-credential safeguards remain effective.
  Keep refresh-relevant change reporting and preserve cached credentials on failure.
- Use the new TUI request handle for account operations. Preserve 0.167's backend
  switching, unread markers, draft retention, agent center in embedded mode,
  attachment thumbnails, command checkpoints and thread section notifications.
- Match rename completion to backend as well as thread/name; identical thread IDs
  served by separate backends must not update each other's rows. Cover this with a
  behavioral test and rename editor snapshots. Auth reload UI code lives in its
  own module, keeping the main App module focused on coordination.
- There is only one TUI crate in this tag. The existing upstream rate-limit refresh
  supplies the header cadence with the fork’s 15-second maximum interval (5 seconds
  near exhaustion); no duplicate poller is added. Centered dialogs hide the header
  so its text cannot show through dialog margins.
- Keep receipt deduplication scoped to the turn and submission, including portable images.
  Identical text in separate turns remains visible; repeated receipts for one submission
  render once. Quota pauses cooperate with 0.167's model fallback and preserve sparse
  rolling quota metadata. The header yields to editable input in a short terminal and
  stays out of question editors and centered dialogs.
- Resolve the status header’s Git executable and PATH through the upstream system-command
  helper. The interactive path-safety regression caught workspace PATH execution before
  trust; the header now also disables optional Git locks and applies the safe-bare policy.
- Open the optional ledger lazily under the new SQLite validation policy. Corruption is
  reported without rebuilding away authoritative coordination records; the state tests
  cover this non-reconstruction behavior.
- Omit the unused engineering prompt/context scaffolding and the Dell-only OS
  user-agent override, as explicitly allowed in the previous seam inventory.
  All other 302 old-fork paths are accounted for by restored behavior, generated
  interfaces, retained release policy, or the documented omissions.

## Release review

Inputs are official `rust-v0.166.0..rust-v0.167.0` and the old fork workflow at
`31ed259248`. Review includes install-context, daemon package validation,
linux-sandbox, the canonical package builder and npm staging.

| Decision | Evidence and outcome |
| --- | --- |
| APPLIED / already satisfied | Upstream now releases Cargo artifacts only. The fork already builds the CLI, code-mode host and exec server together using Cargo and the canonical package builder. The runtime package contract has no change between the tags. |
| APPLIED | Local candidate packages now record `0.167.0-<commit>` rather than stable `0.167.0`, keeping seeded daemons outside the upstream stable auto-update channel. Build temporary files stay in the lab. |
| OVERRIDE | Retain manual-only `workflow_dispatch`, the existing reduced workflow set, complete packages and `codext` executable aliases. No release is dispatched or published. |
| OVERRIDE | Disable automatic TUI update offers and direct CLI package-manager updates. Link manual update guidance and release notes to this fork, preventing accidental replacement with a build without Router. |
| NOT_APPLIED | Upstream's temporary empty BuildBuddy key for separate macOS voice/Bazel jobs and removal of Bazel artifacts from its release download pattern have no corresponding fork job or artifact path. |
| NOT_APPLIED | DotSlash manifests, signed DMGs, separate app-server distributions and the independent responses proxy remain outside the fork's product set. No newly required CLI companion was found. |

The dormant manual npm release configuration still uses its historical namespace;
this task builds a local candidate and does not establish or publish an npm channel.
The skill files are preserved unchanged. Their historical npm identity is not an
instruction to install that third-party package in the laboratory or daily environment.

## Verification and acceptance boundary

The minimal-disk CLI build, stable and experimental schema generation, config schema
generation, Bazel lock update/check, and release artifact structural preflight passed.
The refreshed Python SDK is derived from the new stable schema, rather than copying
the old generated API. Scoped checks include:

| Area | Result |
| --- | --- |
| Unchanged engine | 146 passed |
| TUI | 5,952 unit and 25 integration checks passed; 8 upstream ignores |
| CLI | 554 native checks passed; 1 system-proxy-dependent check covered separately below |
| App-server protocol / host | 322 / 425 passed |
| State | 243 passed, including preservation of a corrupt coordination ledger |
| Core automatic-turn ownership | 24 focused `turn_input_submission` checks passed |
| Login | 269 native checks and 4 isolated proxy/redirect scenarios passed |
| Other changed crates | Daemon, config, async utilities, features, guardian, history, protocol, skills, thread store, CLI utilities and web search checks passed |

The host's macOS system proxy prevents the upstream login and MCP fixtures from
reaching their loopback servers after those tests explicitly remove `NO_PROXY`.
Rather than changing system networking or weakening assertions, isolated local
servers exercised the original login child scenarios and MCP discovery with explicit
loopback bypass. Both `respect_system_proxy` modes returned `not_logged_in`; OAuth
error/drop cases did not replay POSTs, and 302/307 redirects preserved the expected
GET/POST behavior. The original environment failures and replacement evidence are retained.

TUI integration fixtures now advertise the fork's enabled auth-reload capability.
Package tests use a local prerelease manifest so a stable upstream tag cannot put
their test daemon on the automatic stable update channel. Visible snapshots were
reviewed and accepted, including the tag version, status header, rename controls and
account picker. Two fixed-width snapshots intentionally retain trailing padding.

Checks use isolated homes, a 16 MiB Rust test stack and a short lab temporary directory
outside the repository. A stale, zero-byte Git index lock with no open file or Git
process was preserved in the evidence scratch area before staging. The complete Rust
workspace suite was not run: AGENTS requires separate confirmation and none was received.
Required scoped lint and formatting precede the final package build; Rust tests are
not rerun after those commands, as instructed by AGENTS.

`just fix` completed for all 23 changed host/base crates. It retains diagnostics for
the existing profile-lock expectations, intentionally serialized async auth transitions,
large host signatures, the prescribed yellow header segments, and a debug-test-only
unused version helper; this is not a warning-free Clippy result. Test-target compilation
also caught and fixed two core TUI config fixtures and one inherited-settings tuple
destructure that still used the previous host interface.
`just fmt` completed. Its engine-only formatting edits were reverted to preserve the
required byte-identical engine; unrelated formatter churn and mandatory unchanged
packaging/helper carry-overs were also restored.

Validation uses only laboratory state and test sessions. The user removed the fixed
account constraint: select the eligible paid profile with the earliest reset, at least
5% remaining and no active cooldown; on quota/capacity failure use the engine's cooldown,
account switching and retry policy. Record the actually used profile(s).

`/Users/jqwang/.AGENTS.md` was absent when checked; the user's supplied instructions
and explicit account policy were applied. `codex-account-next list` was run with
explicit laboratory `--home` and `--binary` arguments, preventing its default daily
home from being used. The initial eligible profile was `JannLinegarA0k`, Pro, 95%
remaining, resetting at `2026-11-08T06:55:04Z`, with no cooldown. The live query refreshed
at `2026-11-06T07:53:38Z` selected the same profile with the same quota.

## Completed laboratory candidate

- Packaged code commit: `f4acb77c9d7b88acfb5053e5811dfefbbdf00ae2`, directly atop
  `rust-v0.167.0`; candidate branch `feat/rust-v0.167.0`.
- Package version: `0.167.0-f4acb77c9d`, macOS ARM64 development profile, incremental
  compilation and debug information disabled. The canonical workspace invocation built
  `codex`, `codex-code-mode-host` and `codex-exec-server` together successfully.
- Package: `/Users/jqwang/242-superagent-codext-lab/build/pkg-f4acb77c9d`.
  The lab's current package points there. Its managed daemon matches the full SHA-256
  of the entrypoint, both companions, ripgrep and package manifest. Auto-update remains
  disabled; Superagent status is healthy before and after the live run.
- Evidence: [local run index](../../tests/runs/rust-v0.167.0-f4acb77c9d/README.md).
  It includes checks, package build/install logs, manifest/hashes, account selection,
  mock facts, live records and boundary checks.
- Mock end-to-end: ordinary start/report/say guards, anchored audit/delivery, worker
  capacity failover/replay and Router capacity failover/replay passed. The two logged
  ERROR entries are the deliberately rejected jargon and unfounded `builds_on` calls.
  Anchors in this mock are synthetic fixtures, not a new project-management structure.
- Successful real-model run: `tests/h/r167-live2`, `gpt-6-astra` with low reasoning.
  Worker `01a1afe0-6c88-7b90-a35b-43d488a2fa3c` and Router
  `01a1afe0-7982-7b83-8c51-7f7aed5dbf47` both report `JannLinegarA0k` as their runtime
  profile. Exactly two worker turns have `drive` attribution. A running turn received
  the correction; both reports reached the test Router; `say` supplied the recorded
  basis and root request; the file contains exactly `candidate-ready`, `steered`,
  `verified`. The Router stopped the drive, and its ledger record is absent.
- The first real attempt is retained as unsuccessful evidence: appending the test
  Router left the older lab Router first, so that Router received notifications and
  advanced the same bounded test goal using its existing `kalliesallerwu` profile.
  Thus the actual accounts across both attempts are **JannLinegarA0k and kalliesallerwu**.
  The corrected run temporarily placed the test Router first, stopped the prior drive,
  and restored the original registry exactly. A missing required `folder` in the test
  start arguments was also corrected; no production code change was needed.
- No real quota/capacity failure occurred and no cooldown was created. Cooldown until
  reset, capacity cooldown of 15 minutes, account selection and retry are covered by
  the unchanged engine tests and injected mock failures, not claimed as real failures.
- Final boundary checks: existing drive records unchanged; no live test drive remains;
  no live `.engineering` directory; engine Git tree identical to old main; `main` and
  `origin/main` still `31ed25924830e51d471ef2e6cc660cab3270929b`; the daily daemon's
  process identity and start time are unchanged. Subsequent branch commits only add
  this evidence and validation guidance; the packaged code commit above is fixed.

The candidate remains subject to user acceptance. Do not advance/push `main`, publish,
or install/restart anything in the daily environment before that acceptance.
