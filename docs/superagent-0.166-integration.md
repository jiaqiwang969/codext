# Superagent integration on Codext 0.166

Updated 2026-11-04. This is the integration contract and work ledger. Source,
candidate, and installed production versions are tracked separately.

Latest user direction (2026-11-04): Dell has rebooted; resume the Dell work
before discussing Agent command center integration. Source acquisition and the
0.166 implementation have now resumed. Production task replay remains separate
from candidate preparation.

## Source baselines and availability

| Component | Baseline | State |
| --- | --- | --- |
| Personal Codext | `caa8a6c684`, based on `rust-v0.166.0` | Installed daily Mac build |
| Organization source | build-27, `a3f27e2f846579cade91b098fcdb892b22e7cba0` | Exported from Dell; both source worktrees clean |
| Organization configuration | `430e784187d18f2c15e0bc547813d0a793b6f374` | Clean Dell `org` repository |
| Organization runtime | build-27 on 0.165 | Production service and watchdog stopped and disabled at acquisition |
| ATM gateway | `superagent/gateway-hardening`, `5101fea` | Separate gateway work; no gateway routing changes in this port |

Fleet verified the Dell LAN route and SSH host identity after the reboot. Source
was exported from `/home/codext-org/src/codext`, where both `superagent/a1` and
`superagent/integration` point to build-27. Mac imported the bundle as
`dell/superagent-integration`; `git bundle verify` passed. Its SHA-256 matches on
both machines:

`8335114cd3f24307aca618787d707c43b5b2cf1edf87f0cb95dab43649df0b97`

The initial inventory and source receipt are under
`233-super-agent/verification/dell-0.166-resume-20261104`. The earlier 11/03 audit
snapshot remains historical evidence and was not substituted for build-27.
Exported Git source contains no account credentials or conversation transcripts.

## Work ledger

| Work | State | Completion evidence |
| --- | --- | --- |
| Preserve PM binding at launcher entry | Installed on Mac | `d16bdb73f8`; explicit profile selection only |
| Acquire exact build-27 source | Complete | Full commit, verified bundle, clean worktree inventory |
| Reimplement organization extension on 0.166 | Implemented; Mac build passed | Optional extension, new host lifecycle, unchanged ledger migrations |
| Linux candidate | In progress | Separate build and runtime directory on Dell |
| Validate production account routing and recovery | Pending operational acceptance | No claim of live gateway or worker validation from compilation |
| Restore sealed deployment | Not performed | Production pointer, databases, stop markers, and service state retained |

## Implementation changes

The domain implementation comes from exact build-27: multi-PM/project routing,
intake, workers, assignment budgets, reports, outbox, effects, recovery, helper
cleanup, and the default-off model fallback. Its host is adapted to 0.166 rather
than replacing the newer request processors wholesale. Large domain modules
retain their source structure to keep this migration auditable; splitting them
and restoring test coverage belongs to later work after the reapply guardrail.

The host now performs actual account runtime replacement, preserves PM tool
policy on cold resume and account changes, and relies on the queue's existing
wake-up instead of requesting a second start. Startup registers the complete
host and reconciles durable state before dispatching effects. Shutdown serializes
with startup and drains active effects. Managed daemon mode rejects this feature
until its automatic continuation can be reconciled with assignment recovery.
The ledger opens only when the feature
is enabled and does not use the rebuild-on-corruption policy of cache databases.
`CODEXT_SUPERAGENT_ROOT` separates candidate workspace layout from production.

The build-27 delayed continuation lacked a fresh assignment check. The port uses
0.166's terminal turn-error lifecycle; stream retries alone cannot schedule a
continuation and explicit turn aborts clear eligibility. The delayed effect
captures the failed turn and a fingerprint of the ticket, rechecks the assignment,
inputs, report, running turn, helper/background work, and outstanding delivery,
and cancels stale work. Counts and consumed receipts are transactional and durable
across restarts. Legacy continuation rows without failed-turn evidence are dropped.
The capacity implementation is isolated in `executor/codext_thread/capacity.rs`.


Acceptance follows this branch's temporary rules: no test or snapshot changes,
no test targets, formatters, linters, or automatic fixes. The minimal CLI build
uses Rust 1.95.0 and the verified target-specific V8 artifacts. Config schema is
regenerated; Bazel lock update and check pass. The protocol schema writer is
exposed through a test target, so the unchanged experimental `superagent/status`
contract is copied from build-27 into the reviewed 0.166 precomputed export tree;
the stable wire export is unchanged. Mac runtime smoke checks pass with a local, non-model provider: the disabled
feature creates no organization ledger; the enabled service opens both unchanged
migrations, reports two registered PM groups, and returns healthy status after
process restart. All three app-server processes exit normally. No model turn was
submitted. These checks do not validate live gateway routing or worker execution.

## Launcher contract

Install with `python3 scripts/install-codex-account-tools.py`. The installer
preserves previous files under `~/.codex/backups/account-tools-*`.

`codext-org` and `CODEXT_ORG_PROJECT=<project> codext-org` reconnect to the saved
PM without selecting an account. Inherited `CODEX_ACCOUNT_PROFILE`,
`CODEX_HOME`, and `CODEX_SQLITE_HOME` must not import the personal session's
account or local state into that operation.

For an intentional direct-provider profile change, use
`CODEXT_ORG_ACCOUNT_PROFILE=<remote-profile> codext-org`. The target profile must
exist on the remote app-server. The existing resume path binds and verifies it;
a busy thread or failed switch must remain an error rather than opening a fresh
conversation. This variable does not pin ATM's actual outgoing account.

The launcher retains the existing SSH transport and `CODEXT_ORG_HOST` override.
Its historical default address is not live discovery evidence: verify the
current route with Fleet before reconnecting. Replacing this transport with
Fleet's complete route, port, and jump options is a separate remaining item.

## Runtime responsibilities to preserve

1. **Organization engine:** preserve multi-project PM routing, busy-PM intake,
   root worker threads, helper cleanup, ended-thread guards, follow-up ownership,
   transactional effects/outbox, report deduplication, and assignment budgets.
   The old audit findings are not a current bug list; later builds fixed many.
2. **Thread account runtime:** direct-provider changes go through the real
   profile-switch lifecycle, including cold resume and loaded-thread service
   replacement. Writing a profile JSON file alone is insufficient. Adapt the
   host to 0.166 thread extensions, configuration loading, and shutdown draining.
3. **ATM account routing:** the gateway owns the actual outgoing account,
   cooldown, and per-account concurrency. Distinguish the thread's saved profile
   from the gateway route in status. `/pool/accounts` describes pool state; it
   is not a per-thread routing receipt. `/v1` thread affinity and API-key overflow
   remain separate, unfinished features.
4. **Credential ownership:** choose one refresher per credential. Shared OAuth
   credential copies on Dell, personal Codext, and ATM do not become coordinated
   merely because the host moved to 0.166. Resolve ownership before deleting or
   moving any credentials; no credentials were migrated by this launcher fix.
5. **Recovery:** retain resume-before-enqueue for unloaded threads. Queue start
   still rejects unloaded threads with "resume the thread before starting a
   queued message". An `inflight` record with uncertain delivery must be
   reconciled, not blindly resent.
6. **Retries:** Codext retries the existing turn; ATM can change accounts before
   exposing a response; the organization may create a continuation only after a
   terminal failed turn, no report, and no surviving active work. Deduplicate by
   assignment/turn and cancel on new user input, stop, or report. Do not let an
   intermediate retry event trigger a second turn. Preserve the default-off
   model fallback from build-27 until its gateway interaction is validated.
7. **Budget:** use the executor's per-assignment token delta, including cached
   input under the existing contract. Do not restore the obsolete design that
   used a live Goal budget and thereby enabled a competing continuation loop.

## Candidate acceptance

Respect the branch's reapply guardrails: implementation and necessary docs only;
no test/snapshot changes or lint/format/autofix. Build the candidate with:

```sh
cd codex-rs
CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=0 cargo build -p codex-cli
```

After source acquisition and implementation, use a separate app-server, socket,
state directory, and scratch worktree. The later operational acceptance must
cover a new task, the same old task after an explicit account change, interrupted
delivery without duplicate work, two PMs without cross-project reports, helper
cleanup, and capacity failure without multiplied continuations. A successful
build alone does not validate these organization behaviors. Keep existing
production tasks and the sealed state untouched during candidate preparation.
