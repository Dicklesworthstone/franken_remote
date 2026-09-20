# Shared subscribers on their original running sessions

`HostSession::into_shared` consumes the original observation session and its
original `Publisher` subscriber. It requires opaque connection identity, the
same observation authority, and matching boot/OS/remote session scope. Neither
numeric identifiers nor decoder readiness grant observation or input authority.
Control-requesting sessions refuse. `SharedHost` owns no source or native worker.

`SharedHost::drive` joins the existing session's UDP driver, installed-Tailscale
refresh, clock service and challenge renewal with shared decoder startup and
media sending. Exact decoder replies and repair lanes are serviced before the
general session dispatcher. Optional presentation and solicited receiver metrics
use the existing verifiers; unseeded joins cannot invent source progress. Each
maintenance pass admits at most sixteen subscriber records. `serve` repeats
bounded turns with a cooperative yield; capture remains on `Publisher::serve`.
The original qualified nonce source is still supplied by the host, not the peer.

The final native network guard checks the shared source as well as the original
viewer authority, including during admission refresh and idle waits. Cancellation
at call time (even an unpolled drive/serve), a failed session turn, member failure
or source revocation closes this session and releases its membership. Observation
is revoked before retained media is released. Another viewer's encoder and
connection are not closed. Last-member source shutdown and confirmed child
reaping remain the publisher's responsibility.

## Direct late-join admission and feedback ordering

`HostSession::join_shared(queue, media, policy, timeout)` uses that session's
actual observation authority and original connection to enter the existing
bounded `JoinQueue`. The caller supplies completed channel attachments, not a
replacement authority or connection. The publisher keeps the original eight-slot
bound, two-second maximum startup deadline, source IDR rate admission and buffer
reservations. Admission refuses control intent, foreign media, invalid timeouts
or expired ownership; failure retires only the moved session.

The same session owner drives initial/pending and late-join decoder replies,
selective repair, source observations and renewal. Optional receiver-load queries
begin only after the first-decode gate, so an unconfigured peer never receives
steady-state telemetry ahead of startup. Unnegotiated feedback refuses rather
than being mistaken for a renewal response. Load remains advisory and grants no
view readiness or control.

## Executed scope

Ten new tests use actual TLS/UDP session negotiation, completed media attachments,
production publisher and receiver code, and supervised source IPC. They cover
observation renewal beyond its original three-second grant, continuing static
source observations after one viewer leaves, unpolled cancellation, foreign
connection refusal with identical numeric IDs, invalid turn bounds, and source
revocation before further media admission. The additional tests drive live joins
through their original sessions, renew beyond three seconds after the original
viewer departs, repair actual missing video datagrams while another viewer remains
healthy, exchange solicited load without blocking renewal, and refuse foreign or
unnegotiated traffic. No configuration or load report grants input readiness.

Final selected runtime results: ten new session tests, eleven unchanged session
renewal/refresh tests, and 93 existing shared-startup/capture/fanout/recovery/egress/
authority integration tests pass (114 unique tests). Test identity, encoded
parameters and decode completions are explicit fixtures, not live-tailnet or
HEVC/GPU evidence.

All eight relevant first-party libraries were rebuilt from checksum-verified
72593290 source with nightly-2026-08-31 and unchanged, compiler/lock-matched
external libraries retained from CI source 38f1b0c. Complete production and daemon
test-source strict pedantic Clippy and changed-file formatting pass. Full daemon
test-binary generation exceeded the local execution limit; runtime verification
uses a separate source copy with only the selected twenty-one in-crate tests registered, preserving
all selected assertions and production code. The initial manually joined fixture
needed an explicit cooperative yield matching the production `serve` boundary;
its original failed scheduling assertion is retained in local logs. A later
unnegotiated-feedback injection initially assumed an empty critical send queue;
the corrected fixture retains the same bytes/deadline through backpressure, and
the complete selected 21-test runtime group passes. No assertion or timeout was
relaxed. No full workspace, cold dependency rebuild, hardware or live-tailnet pass is claimed.

Automatic listener/OS-registry ownership, independent source-consent renewal and
shared failed-viewer recovery remain separate integration. This owner does not
renew source consent using a viewer's challenge or silently reacquire control.
