# Superagent integration on Codext 0.166

Updated 2026-11-04. This is the integration contract and work ledger, not a claim
that the organization engine is included in the current build.

## Source baselines and availability

| Component | Baseline | State |
| --- | --- | --- |
| Personal Codext | `caa8a6c684`, based on `rust-v0.166.0` | Built and installed on Mac in the preceding upgrade |
| Organization Codext | build-27, `a3f27e2f8`, on the 0.165 fork | Last recorded on Dell; complete source not available locally |
| Local organization snapshot | `verification/dell-superagent-audit-20261103` in the Superagent project | Earlier source only; preserve as historical evidence |
| ATM gateway | `superagent/gateway-hardening`, `5101fea` | Local source in `240-atm-gateway`; sealed deployment |

The organization seal at `239-jtufem/org-run/SEAL.md` records that Dell was shut
down, organization services were disabled, and ATM and its tunnel were stopped.
The current check found no verified Dell route after two attempts. GitHub
`origin` advertises only `main`, `feat/rust-v0.166.0`, and the pre-upgrade backup;
it does not advertise an organization branch. The local object database does not
contain `a3f27e2f8`.

The required source lives in the Git repository at
`/home/codext-org/src/codext` on Dell. Inspect its worktrees and refs to find the
exact build-27 commit; do not assume that `main` or an old audit snapshot is the
organization implementation. Export the committed source and record any dirty
files separately. Reading/exporting Git source does not require starting the
organization service, ATM, a PM, or a worker.

The current 0.166 tree has no `ext/superagent` or `superagent_host.rs`. Installing
the plain binary over the organization candidate would omit its engine.

## Work ledger

| Work | State | Completion evidence |
| --- | --- | --- |
| Preserve PM binding at launcher entry | Installed on Mac | `scripts/codext-org.sh`; no default or inherited personal profile |
| Version and install the launcher | Installed on Mac | Account-tools installer backs up and installs `codext-org`; installed bytes match source |
| Refresh integration design | Prepared | This contract and `233-super-agent/plan/design/t05-codext-0.166-upgrade.md` |
| Acquire exact build-27 source | Waiting for source access | Full commit ID, source bundle, dirty-state inventory |
| Reimplement organization extension on 0.166 | Pending source | Feature coverage and minimal CLI build |
| Validate account routing and recovery | Pending candidate | Isolated candidate evidence; no production task replay |
| Restore sealed deployment | Not performed | Separate operational step after candidate validation |

Local validation: the shell script and Python installer parse successfully;
the minimal CLI build passes with the pinned Rust 1.95.0 toolchain and the
already-verified V8 artifacts from the preceding upgrade. No Rust implementation
changed, and no test/snapshot or lint/format/autofix work was performed. No
organization end-to-end validation or service restart was performed. Local
receipts are in `233-super-agent/verification/codext-0.166-preparation-20261104`.

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
