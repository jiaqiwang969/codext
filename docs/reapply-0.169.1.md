# Candidate reapply on rust-v0.169.1

This candidate starts at the official OpenAI stable tag `rust-v0.169.1`
(`3f7d3da04eb2`), selected from the official upstream remote. The previous fork
is `main`, based on `rust-v0.167.0`. Branch: `feat/rust-v0.169.1`.
No previous fork history was merged or rebased onto the new tag.

## Account and Superagent integration

The account pool, safe-boundary auth reload, `/profile`, thread bindings,
subscriber account-change notifications, composer/status behavior and Router
seams are reapplied to the new app-server and TUI. This upstream has one TUI;
there is no parallel `tui_app_server` implementation.

The new upstream owns provider-aware model catalogs, runtime credentials,
plugin/MCP services and analytics. Named profiles now enter that startup path
through the selected `AuthManager` and a credential-owner-specific runtime slot.
Account APIs use the same runtime owner as their corresponding threads.

Two previous fork mechanisms are removed because upstream supplies them:

- `ThreadAuthManager` and the extra account-auth branches in git attribution,
  Guardian, history notes, image generation and web search: upstream's
  `ProviderAuthContext` carries the thread's selected credentials.
- `ExtraConfig.creator_account_id` and its thread-store reconstruction:
  upstream's native `StoredThread.creator_account_id` supplies the historical
  creator. Explicit current profile bindings remain fork behavior.

Superagent and its existing tests are carried forward. The only engine source
adaptation maps upstream's new `InferenceHookDenied` error to its wire name.
The host accepts upstream's new thread-list fields and rejects a user input
submission that was accepted without a started or steered turn. The existing
four seam-test additions are carried unchanged; no new test logic or snapshots
are introduced.

## Protocol exports

Stable and experimental precomputed exports retain all new upstream schemas
and add only the fork's profile/reload/status methods, notification and
`ThreadExtra` fields. The public JSON and TypeScript exports match the stable
bundle. Python artifacts are regenerated with the pinned generator and
`--no-format`; unchanged formatting is retained after checking AST equivalence.
No schema-fixture test target, formatter or linter is run.

## Release decisions

| Upstream change / artifact | Decision | Result |
| --- | --- | --- |
| musl release linker flags | APPLIED | Enabled for the musl setup and preserved when sanitizer flags are cleared. |
| libcap download fallback | APPLIED | Bounded GitHub download with Debian fallback and the existing checksum. |
| Safe GitHub release lookup / publication | APPLIED | Lookup failures abort; new manual releases target the workflow SHA, upload into a draft, then publish. |
| Tag-driven prerelease publication and cleanup | OVERRIDE | Codext manually computes its release tag and publishes a completed draft; it does not create a public prerelease before asset upload. |
| BuildBuddy credential changes in upstream Bazel release jobs | NOT_APPLIED | Codext's five-platform release matrix uses Cargo; those jobs and credentials are absent. |
| CLI, code-mode host and exec server | APPLIED | Built in one workspace invocation, uploaded, and assembled by the upstream package builder. |
| Linux bwrap | APPLIED | Built, stripped and hashed before compiling the CLI; the same bytes enter `codex-resources`. |
| Windows command runner and sandbox setup | APPLIED | Included in the Windows workspace build and canonical resource layout. |
| npm identity and manual dispatch | OVERRIDE | Existing `@loongphy/codext` identity remains; npm publication is opt-in and off by default. |

The install-context, daemon, linux-sandbox, canonical package builder and npm
launcher changes between tags were reviewed. The required package layout and
companion set did not change. The upstream standalone responses proxy remains
outside the fork's shipped runtime. No override-manifest change was needed.

## Validation and promotion

- Official locked dependencies, Rust 1.95.0, minimal-disk CLI build: passed.
- Release artifact structural preflight: passed.
- `just bazel-lock-update` and `just bazel-lock-check`: passed; the module lock
  needed no content change. Upstream direct-module-version warnings remain.
- `cargo test -p codex-superagent-extension`: 209 passed, zero failures, using
  official locked dependencies.
- Canonical macOS arm64 package `0.169.1-cc60f08ee1`: built and installed in
  the laboratory; its CLI SHA256 is
  `a3f59d1e8e5471a030e8478e69c5f069f6da66fe8eae43997e12f7403a8e422e`.
- Existing packaged-binary `drive_e2e.py`: passed simple and anchored drives,
  grounded `say`, notifications, account failover and Router retry. Its two
  logged errors are the expected rejected `say` inputs.
- Dedicated real-model Router and worker: passed profile binding and subscriber
  notification, in-flight steering, conclusion delivery, a grounded second step,
  result readback and stopping the drive. The account passed the user's isolated
  image check before selection. The first operational attempt lacked a worker
  subscription in the driver; that driver was corrected, with both attempts
  retained. The candidate executable was unchanged.
- The original laboratory Router list is restored; test drives are released.
  The daily daemon package path and CLI hash are unchanged. Laboratory quota
  watching is disabled (`CODEXT_SUPERAGENT_QUOTA_WATCH_SECS=0`).

Private local evidence: `~/242-superagent-codext-lab/tests/runs/rust-v0.169.1-cc60f08ee1/`.
Multi-platform CI and manual TUI visual acceptance were not run. The validated
artifact is the local macOS arm64 package, not a cross-platform release.

The V8 dependency is the upstream-locked `90a65f887a0c0f048f3784afbcb320b373c8a2e0`.
A temporary diagnostic path override was removed before final validation;
matching local submodule caches were verified against their gitlinks.

`main` and the daily daemon stay on the previous version until user acceptance.
Promotion uses the accepted package, following [updating.md](superagent/updating.md).
