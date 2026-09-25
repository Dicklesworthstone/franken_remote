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

## Removing one viewer

The existing `SessionRegistry::remove_session` now revokes matching actual shared
subscribers before removing descriptive metadata. It visits every registered
source with bounded work, including multiple display subscriptions of one remote
session. Failed member slots remain reserved until their original Subscriber
owners drop, preventing old network tasks from addressing a replacement slot.
An unknown session is non-mutating. Removing one viewer preserves other viewers'
source and renewal; removing the last admitted viewer ends source consent and
leaves its original child available for confirmed reap. This is an explicit local
registry command, not a generationless network or worker callback endpoint.

## Executed evidence

Nine new tests exercise actual negotiated TLS/UDP sessions, source/receiver owners,
shared storage and supervised native source IPC. They cover live registered joins,
OS switching during continuous publication, every generation transition, teardown,
registry Drop, explicit retirement, stale/foreign routing handles, duplicate
source and registry ownership, wrong scope, and control-intent exclusion. Additional removal cases exercise independent
renewal beyond three seconds, cancellation of a waiting join without forcing
an IDR, last-viewer shutdown, and unrelated registries reusing numeric IDs.
The final selected 23-test in-crate group (nine new, ten existing shared-session
tests, four existing registry tests) and 109 existing integration tests pass
(132 unique tests). The integration group covers broker16, shared-startup25,
pending-startup9, late-join10, shared-capture24, fanout6, native-recovery9, egress7
and authority3. These are unique results; repeated runs are not added twice.
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

## Publication retry — September 25, 2026

The saved registry changes were reconciled with later shared-source recovery,
selected-display, cursor and native-control work, rather than restoring their
older whole-file snapshots. Source preimages for every modified existing file
were matched by Git blob hash against main at 284b3d6734be2ca3b3f1988c1b34c57d51d89229.
The current exhaustive native error dispatcher explicitly treats registry errors
as host faults; removal uses the stable installed view even while recovery has
retired the media attachments. Existing cursor/control/recovery code is preserved.

All nine saved registry integration tests were rerun and passed on checksum-verified
4b156d9 source plus the reconciled registry changes, using the pinned compiler
and matching retained external libraries. All eight first-party libraries rebuilt,
and the complete daemon test-source strict Clippy check passed for the first slice.
Runtime selection used a separate copy changing only unselected test registrations;
production code and the nine tests' assertions were unchanged. The additional
current-main changes after 4b156d9 are source-reconciled, not a new full-workspace
runtime qualification. The original 132-test result above is historical and was
not rerun in full during publication. No new hardware or live-tailnet claim is made.
