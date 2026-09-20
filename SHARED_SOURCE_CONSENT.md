# Local consent for a continuing shared source

The original `SessionAgent` can now renew an independently approved
`shared_publisher::Publisher` while its viewers keep their own connection and
observation renewal owners. This is source consent, not a viewer heartbeat,
input grant, decoder acknowledgement, or evidence that pixels are fresh.

## Ownership and integration

At the original LOCAL sharing-policy decision, after the selected source/display
and its initial viewers have been authorized, call
`SessionAgent::attach_shared_source(&publisher)`. The source must have a live
cohort, independent authority, no peer-bound admission lease, and no attached
network renewal owner. Missing or unknown screen-capture permission refuses
without changing the existing publisher. The same source cannot attach to two
local agents, and cannot subsequently attach a network renewal owner: the
original authority's sticky renewal-ownership marker is used.

The agent keeps at most eight source registrations. Each contains a weak
publisher reference and fixed scope/cadence metadata, not a capture worker,
transport, picture, or cloneable permission grant. Retired slots can be reused;
old registrations cannot act on replacement sources. Publisher destruction,
last-viewer departure, source failure, or expired source authority is terminal.

The local event loop calls `service_shared_sources(fresh_nonce)`, servicing the
returned `Report::next_deadline()` and real permission/lifecycle changes. It must
supply its qualified unpredictable nonce provider, not peer-supplied material or
clock-derived values. The callback runs only when renewal is due and outside all
policy locks. Repeated maintenance does not consume entropy or shift a deadline.

Before renewal, the implementation verifies the original local agent is not
revoked, its OS session is unchanged/unlocked, capture permission remains granted,
and the exact publisher/source/display scope is still live. After entropy
creation it checks again, then uses the EXISTING authority challenge/response
machine. The issue-time deadline is retained. Cadence is at most one second and
shorter for shorter configured authority lifetimes; no policy lifetime is widened.
Remote viewers' leases/readiness/input tickets and all media deadlines remain
independent. A local event-loop stall lets the original source lease expire even
when remote viewers keep responding successfully.

## Revocation and cleanup

The agent installs one fixed-registry callback on its existing sharing indicator.
Indicator revoke, OS lock/session transition, approval-mode change, agent drop,
and the new `on_screen_capture_revoked` platform event fence the affected source
and all its subscribers. This works while capture is actually pending; no policy
mutex spans native IPC or an await. Revocation does not wait for a network round
trip, a capture response, or a maintenance turn.

`on_screen_capture_revoked` returns the existing revoke outcome and held-input
cleanup operations. The local caller must submit those cleanup releases; source
revocation is not OS rollback. The original Publisher still owns child termination
and confirmed `reap`. Expiring one source does not revoke a different source on
the same agent. Global local revoke deliberately closes the complete registry.

## Verification boundary

Twelve new tests run against the original publisher, authority, sender/receiver,
TLS/UDP attachments and supervised child IPC. They cover streaming past the
initial five-second fixture authority deadline on the SAME capture worker,
independent viewer departure, bounded eight-source admission/reuse, per-source
retirement isolation, duplicate ownership, missing/revoked permission, entropy
failure/repetition, 2,000 early maintenance calls, re-entrant local closure during
entropy, immediate indicator revoke while capture is pending, lock/session change,
agent drop, and source expiry despite live viewer renewals. Permission state and
codec payloads/completions are explicit fixtures, not native OS or HEVC proof.

The complete shared-startup target passes 37 tests (12 new), and the existing
pending-startup target passes nine. Strict daemon-library and complete
shared-startup-target Clippy, changed-file formatting and whitespace checks pass.
The broader late-join target records nine passes and one failure at
`stalled_newcomer_is_refused_before_next_capture_without_stalling_healthy_viewer`
(`slow.configuration.is_some()`); its source was not modified in this slice.
No complete workspace pass or resolution of that separate failure is claimed.

Verification rebuilt all eight relevant first-party libraries from the
checksum-verified f4ca58e558e6da05d5ca0ff382a6f2047e635b2a source archive plus
this slice using nightly-2026-08-31. External compiler/lock-matched libraries were
retained from CI source ba79119; their external package versions/checksums match
this source. This is not a cold external-dependency rebuild, GPU/display,
real-HEVC, live-tailnet, or complete-workspace qualification.

## Still separate work

The local adapter must supply genuine permission events/probes and the original
local source-approval decision. These methods do not themselves query TCC,
portals, X11 permission, or synthesize local approval. Automatic listener and
OS-session-registry wiring, live native permission-loss qualification, and shared
loss recovery remain open. Existing per-viewer SharedHost renewal must continue;
it cannot renew or revive this source scope.

Refs: plan 7, 11.2, 11.3 and 19; fr-p1-frame-pipeline-am1 remains open.

## Interrupted-renewal follow-up

A source-local scope guard now fences the original publisher if maintenance
unwinds (for example, through an entropy-provider panic), rather than leaving its
old permission live until ordinary expiry. The event loop may catch the panic,
but cannot use that as authority to keep the old source streaming. The actual
publisher regression fails before this guard and passes after it. Another test
verifies that capture-permission loss returns held Shift/button cleanup operations
while native cleanup remains unacknowledged; no OS effects are invented.

Final shared-startup verification passes 39 tests, including all 14 new consent
cases. The late-join target subsequently passes all ten tests without altering its
source or assertions; the earlier failure remains recorded, not declared fixed.
The existing SessionAgent target records nine passes and two failures (approval
retry after a denied scope, and cleanup-tracker acknowledgement). A separately
rebuilt untouched f4ca58e baseline reproduces both failures exactly. None of those
existing assertions or authority rules was changed to obtain a passing result.
The remaining native regression group passed shared capture 24, pending startup 9,
join-rate 3, recovery 9, authority 3, egress 7, response capacity 4 and supervision
12. These are scoped integration results, not a full-workspace or hardware gate.
