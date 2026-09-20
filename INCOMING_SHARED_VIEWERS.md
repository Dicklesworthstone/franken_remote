# Incoming shared observation sessions

`shared_viewers::Admission::admit_host` accepts the original fresh `Host` before
application negotiation or local approval. The same bounded slot now owns
ClientHello/selection, approval, display selection, three fresh media attachments,
decoder startup and continuous shared viewing. A caller no longer has to drive a
late viewer to `HostSession` outside the running shared-desktop service.

The input must already come through `Host::from_admitted`: verified native
TLS/ALPN, protected tailnet ingress, installed-Tailscale admission and the original
dedicated session context. This API does not bind a socket, discover peers, accept
an unauthenticated Initial, or create identity. It serves observation only and
refuses control intent before notification or observation; it never downgrades a
control request to an apparently successful read-only session. A partially driven
Host is refused instead of resetting its phase or spending.

Opening, Starting and Serving distinguish application consent, shared-display
startup and driver ownership. None is a visibility or input-authority assertion.
Parked negotiations and pending approvals count against the existing three-viewer
default/eight-slot maximum immediately. One original absolute Host deadline covers
all phases through first decode. The shorter existing join allowance still wins;
turning approval into a fresh relative startup budget is explicitly avoided.

A bounded nonblocking local callback receives the existing one-use Approval and
requested role exactly once. A successful callback only acknowledges notification.
It runs outside registry/source locks; the local UI may decide later. Admission
and source authority are checked before and after it and throughout the original
UDP and admission-refresh operations. Source revoke or peer revoke cannot be
reversed by a late local Allow. No observation is published while approval waits.

The cancellation receipt changes from the original dedicated negotiation context
to its original observation authority in a serialized handoff. Closing an unpolled
attempt invalidates its future prompt. Closing an old ticket after slot/session-ID
reuse cannot affect a replacement. Errors retire only their original attempt;
local service cancellation and panic remain terminal for that service. A per-turn
owner fences and releases a task taken out of the registry even on unwind. Neither
notification nor task destruction executes while holding the registry lock.

`SessionAgent::serve_shared_desktop` services incoming Hosts directly alongside
source consent, capture and established viewers. No extra task runner, transport,
encoder, queue, dependency or runtime is introduced. The already-approved
`Admission::admit` path remains available and uses the same reservation rules.

## Executed verification

Twelve new tests exercise actual TLS/UDP and original production owners. Coverage
includes delayed approval, unattended admission, control refusal, capacity before
polling, duplicate/foreign scope, stale-ticket reuse, denial, failed notification,
peer/source revocation, original queue/attachment deadlines and caught notification
unwinding with the terminal future still retained. Managed-service tests keep an
established viewer renewing beyond its original three-second lifetime while a new
viewer waits for approval, then join it using the same capture child. Local
permission loss fences unpolled negotiation before its notification can run.

All 66 canonical shared-session tests pass (12 new, 54 unchanged), including a
final-source rerun. The group uses its previously required 16 MiB test stack; all
12 new tests also pass separately on the default test stack. Production and
complete Linux daemon unit-test source pass strict pedantic Clippy. Formatting
and whitespace checks pass. Initial fixture errors are retained in the evidence:
the client helper used the wrong metadata accessor; the panic test incorrectly
attempted to resume a terminal service; and peer revoke was first observed by the
transport authorization callback rather than the outer Denied check. The final
checks preserve terminal cleanup, no-observation and original-deadline assertions.
The permission test accepts only those two existing authorization-refusal layers.

First-party libraries were rebuilt from checksum-verified 92f9c7c2 source plus
this slice using nightly-2026-08-31. Upstream inputs match the compiler and every
external lockfile version/checksum retained by run 35461102442. The focused runtime
harness omits unrelated test registrations in an external copy; production and
retained test bodies are unchanged. The complete actual Linux daemon test source
is independently linted. Identity, permissions, monitor replies, coded pictures
and decode receipts are explicit fixtures, not native OS/HEVC, hardware, physical
presentation, independent-wire or live-tailnet evidence. This is not a cold/full
workspace qualification claim.

The native multi-client listener, platform approval UI/event adapters, and first
source/first-viewer orchestration remain separate integration work. No pre-TLS
admission bound, OS permission or listener qualification is inferred from this
post-authentication service. Broader beads remain open. Refs: plan 7/17/19,
fr-p1-frame-pipeline-am1 and fr-p2-viewer-admission-e62. See
[shared desktop service](SHARED_DESKTOP_SERVICE.md) and
[viewer ownership](SHARED_VIEWER_SERVICE.md).
