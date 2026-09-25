# OS-session registry joins and retirement

`SessionRegistry::register_publisher` records the actual, already-admitted
`Publisher`, not a copied permission flag or a new encoder. Eight fixed source
slots are bounded independently of viewer slots. The actual boot, OS session,
display geometry and codec generation must match the canonical registry. A source
can belong to only one registry. Re-registering the same owner is idempotent;
a different source with the same display scope refuses until explicit retirement.

`HostSession::join_registered` resolves its completed media attachments through
that registry and joins on its original connection and consent. The registry
lifetime lock is held through bounded join admission, never across native work
or an await. Neither source lookup nor the descriptive session records can grant
observation, input, or decoder readiness. The existing publisher startup/rate,
physical/logical retention, control-intent and connection checks remain in force.

The registry owns weak source references. Returned routing handles are weak,
serial-specific and cannot select a reused slot or another registry. Geometry,
codec and process changes, OS-session switching, teardown and registry Drop
synchronously revoke source and viewer authorities before changing generations or
releasing registry metadata. Canonical scope fields now have read-only getters;
changes must use the retiring lifecycle methods, not public-field assignment.
Explicit source retirement affects only that original publication. A failed or
exhausted generation advance still ends the old source lifetime.

Retirement stops further capture/send admission and cancels the original source
context. It does not execute native work under the registry lock, erase process
custody, or claim that a child exited. The original publisher task must handle
cancellation and retain the Publisher until `reap` confirms cleanup. New local
consent and fresh native ownership are required to register a replacement.

## Executed evidence

Six new tests exercise actual negotiated TLS/UDP sessions, source/receiver owners,
shared storage and supervised native source IPC. They cover live registered joins,
OS switching during continuous publication, every generation transition, teardown,
registry Drop, explicit retirement, stale/foreign routing handles, duplicate
source and registry ownership, wrong scope, and control-intent exclusion.
The selected 20-test in-crate group (six new, ten existing shared-session tests,
four existing registry tests) and 16 existing broker integration tests pass.
Production and complete daemon test-source strict pedantic Clippy and changed-file
formatting/diff checks pass. Full daemon runtime test generation exceeded the
execution limit; selected registrations run from a separate copy with production
code and all selected assertions unchanged. No test timeout was relaxed.

All eight first-party libraries were rebuilt from the checksum-verified baab9c62
source archive plus this change, with pinned nightly-2026-08-31 and unchanged,
compiler/lock-matched external libraries retained by CI source 72593290. Native
codec/identity/decoder replies in session tests are explicit fixtures, not
real-HEVC/GPU or live-tailnet evidence. No full-workspace or cold dependency
qualification is claimed.

The existing listener must still route its admitted sessions to this registry
entry point and supply the independently authorized first source. Independent
local source-consent renewal and shared failed-viewer recovery are not added by
these registry APIs. Source registration count is not an active GPU/encoder
measurement. Refs: plan sections 5, 7, 11.2 and 19; fr-p1-frame-pipeline-am1.
