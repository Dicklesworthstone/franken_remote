# Retiring the optional native clipboard lane

After the native clipboard worker has stopped, call
`ClipboardChannel::retire(&mut connection, &cx)` on its original connection,
between network I/O turns. Retirement is terminal for this lane but does not
revoke input, discard the visible stream, or require reconnecting the viewer.
It introduces no new application record, grant, or automatic reacquisition.

Retirement removes whole records waiting in the transport queue, resets the
native send stream (including its unsent and retransmittable STREAM data), and
sends STOP_SENDING for the peer direction. Private complete/partial record
buffers are cleared. The next checked I/O turn recognizes a peer RESET, FIN, or
STOP_SENDING on an active clipboard pair and retires the corresponding local
pair. Other control, input, media, and repair streams retain their queues and
reservations. Discarded receive bytes reclaim connection flow credit once;
subsequent RESET final-size accounting cannot repeatedly inflate that credit.
The underlying native QUIC receiver may retain bounded out-of-order fragments
until its peer's reset arrives or the connection closes.

The completed attachment and numeric native stream slots remain tombstones.
Neither retirement nor dropping a retired wrapper permits a second clipboard
attachment on the same connection or resets a controller's replay ledger.
Foreign connection objects are rejected before mutation, even with identical
numeric routes. A repeated retirement on the original object is idempotent.

This explicit operation is not rollback: bytes already delivered to the remote
application or submitted to its OS cannot be recalled. The native worker still
has to enforce original-owner authority, both clipboard switches, and transfer
deadlines before publication. A canceled/failed reset closes the original
connection conservatively. Abandoning an incomplete handshake or dropping a
completed channel without explicit retirement retains the existing whole-
connection fence; cancellation inside an active network future is not claimed
to be isolated from the other streams.

The `fr-transport` `media_attachment` integration target exercises real UDP/TLS
pairs, including queued and native-buffered prefixes, partial and complete
backpressured receives, preserved bidirectional input/control, crossed resets,
foreign-object refusal, and inability to reopen the retired clipboard lane.
Payloads are codec fixtures; these tests do not claim native OS publication,
GUI integration, or live-tailnet qualification. See
[PROTOCOL_CLIPBOARD.md](PROTOCOL_CLIPBOARD.md) for the transfer and authority
contracts used by the native worker.

## Authorization across a worker handoff

`ChannelSession::transport` is a read-only view of the original authority and
channel lifetime. Closing it fences the core's final publication check even if
the native worker is inside preparation; it never revokes unrelated input.
Controller sessions expose `ControllerTransport` to sample their existing
client-to-host projection rather than fabricate a host lease or clock.

`RecordSink::try_send_checked` carries an `Egress` permit with the original
absolute operation deadline. An asynchronous sink retains it beside the copied
record, rechecks it before enqueue, and keeps checking while QUIC retains bytes.
The permit is invalid after channel close, authority loss, a switch off/on cycle,
or supersession, including a commit whose sender has already finished. A Cancel
contains no payload and can release the peer while the switches are disabled;
its handoff still has a fixed one-second deadline and original-owner check.
The default sink method preserves immediate synchronous adapters, not permission
to ignore metadata in a queued adapter. A successful check is not delivery or OS
publication, and cannot recall an operation already performed by a peer.

## Native worker and network owner

`frd::clipboard_quic::Bridge` joins the completed `ClipboardChannel` to the
original host input owner (`Bridge::host`) or actual accepted controller
(`Bridge::controller`). Both factories consume the route and return a bridge
and a one-use `WorkerSeed`. The separate `granted` argument requires negotiated
clipboard support, local consent, and the approved native OS session. An input
grant alone is not clipboard permission.

Move the seed onto the interactive worker and call `open(native_factory)`, then
service `Worker::step(new_id)` during traffic and silence. Alternatively,
`WorkerSeed::spawn(native_factory, new_id)` runs this same owner on a named
foreign-call thread. The factory executes on that thread, only after checking
the original authority, and may return a thread-confined native clipboard.
The item-ID callback must supply qualified randomness. Neither factory installs
an async runtime, network listener, new input owner, or replacement grant.

The session services `Bridge::service` and `Bridge::drive` on its existing
connection with its existing admission callback; other routes remain the
containing session's responsibility. All transport dispatch callbacks copy
records only: no native preparation, read, or publication runs in a network
callback or under a mailbox or authority lock. Native and network policy checks
use independent monotonic viewer-clock cursors, so a concurrent network check
cannot replace the exact native sample awaiting its final publication check.
Every cursor remains bound to the same original controller and presentation.

There is one outbound handoff record and at most four records admitted to the
native transport. Every admitted record retains its original Egress permit until
the native empty-send witness accounts for queued, unsent, and retransmittable
bytes. A local copy into QUIC frees the handoff slot, not the authorization permit.
The fixed four-record batch and the existing bulk byte/record budget both apply;
control and input reservations are never borrowed. A contended handoff-release
lock cannot cause an already admitted record to be sent twice.

There is one incoming queued/executing/deferred/uncollected record. Taking that
buffer out of its mailbox does not free capacity. An incoming Begin carries its
original ingress deadline through worker queueing; deferral cannot restart its
three-second lifetime. These slots supplement, not replace, the already bounded
native item buffers and native QUIC stream windows.

A transport drive may flush up to eight 900-byte bulk prefixes before its single
receive wait. A critical prefix takes priority and yields back to receive/service.
Each prefix rechecks authority, original send deadlines, native congestion and
flow credit, including reserved critical credit. One 250 ms outer bound covers
the entire turn. The transport still relies on Asupersync for pacing, loss and
congestion control; this is not an unbounded bulk-write loop.

Collect `Bridge::take_received` regularly. A submitted, uncertain, or refused
publication result is terminal for that record and retained until collected;
subsequent incoming work is backpressured rather than overwriting the result.
A publication receipt is not evidence of an application paste. No clipboard
payload is included in cross-thread diagnostics.

Call `Bridge::retire` before dropping the bridge. It fences native publication
first, then retires the original optional stream pair without waiting for OS
cleanup. `WorkerTask::stop` requests a stop; `is_finished`/`finish` distinguish
actual thread completion from that request. `finish` never blocks on an ongoing
foreign call. A stuck OS call cannot be safely killed as a Rust thread; dropping
the handle does not claim successful native cleanup or rollback.

The current network join is conservative: a switch transition (including
an off/on cycle), expired queued work, or superseded in-flight payload retires
the optional clipboard pair. It does not silently reopen the consumed channel
or replay buffered text when a switch is re-enabled. Input and viewing continue
when retirement is performed between I/O turns. Cancellation or authority loss
inside an active QUIC future retains the existing whole-connection fail-closed
behavior; even dropping an unpolled bridge-drive future fences its original
connection and worker. No operation touches an equal-ID foreign connection.

`clipboard_network` runs two actual Xvfb desktops, separate native worker
threads, the production clipboard protocol, and encrypted UDP/TLS connections.
It covers both directions, empty/Unicode/one-MiB items with a 2-KiB record cap,
repeated copies, blocked real native preparation with live control traffic,
queued cancellation, foreign connections, and stop-before-open. Grants,
consent, presentation evidence, and IDs in these tests are explicit fixtures.
This is a real network/native handoff, not production GUI/session-startup or
live-tailnet qualification. Run alongside the native clipboard target:

```sh
FR_NATIVE_CLIPBOARD_REQUIRED=1 xvfb-run -a \
  -s '-screen 0 1280x1024x24 -noreset -nolisten tcp' \
  cargo test -p fr-native --features linux-clipboard \
  --test clipboard_x11 --test clipboard_network --locked
```

## Running controlling sessions

`ControlledHost::attach_clipboard` and `ControlledViewer::attach_clipboard`
consume a completed `MediaChannel::Clipboard` pair on their existing connection
and return the one-use `WorkerSeed`. The host takes a monitor of the original
input agent's worker-owned session; the viewer uses its actual decoder-backed
`PresentedInput`. Neither reconstructs authority from matching wire IDs.
The pair must be separately negotiated with the clipboard capabilities before
joining; `granted` still requires independent local clipboard/OS consent.
Declining consent retires the completed optional lane without opening the OS or
closing input/viewing. A channel cannot be replaced to reset sequence history.

After moving/spawning the seed on the native worker, the existing controlled
session `drive` methods service clipboard alongside input and media, including
idle turns and host admission refreshes. Application record callbacks never
consume the reserved clipboard route and never perform native clipboard work.
The containing QUIC driver's authorization also checks pending clipboard work;
retirement between turns preserves input, while cancellation or authority loss
inside an active I/O operation retains the conservative connection fence.
Dropping an unpolled controlled drive or closing its owner stops the clipboard
worker's authority before it can open or publish to the native clipboard.

Use `clipboard_switches`, `take_clipboard_received`, `clipboard_reason` and
`retire_clipboard` on the controlling owner. Results remain backpressured until
collected; retirement does not replace an uncertain OS receipt. `WorkerTask`
completion remains a separate native-cleanup witness, not an effect of calling
`close`. The running-session regression tests use real startup, native input
agent, clock exchange, decoder-backed grant and encrypted UDP/TLS, with explicit
fixture grant metadata and clipboard OS implementations. Production native UI
consent and GUI callback plumbing remain required; the optional negotiation API
below grants neither and does not claim live-tailnet qualification.

## Automatic attachment in the running session

When [native-clipboard-startup v1](PROTOCOL_CLIPBOARD_STARTUP.md) and its media/input/clipboard prerequisite
profiles are selected, the host calls `ControlledHost::offer_clipboard(request,
local_consent)` and the viewer calls `ControlledViewer::expect_clipboard(timeout,
local_consent)`. The host request uses the already approved current display/view,
a fresh auxiliary binding and a qualified unpredictable attachment ticket. Neither
call opens the OS. Missing profile selection and invalid timeouts refuse without
starting an exchange or disturbing the existing input session.

Continue the normal controlled-session `drive` methods; they service attachment,
clock exchange, media, input and lease renewal together. `clipboard_negotiating`
reports pending setup. Once the native pair is promoted on both ends, a fixed
`ClipboardReady` record on the original control lane exchanges independent local
consent. A worker is released only after local readiness was queued and peer
readiness consumed. This prevents early payloads from reaching an attachment-only
framer and prevents refusal from resetting away the final attachment receipt.
Handshake records are not forwarded to application callbacks.

After success, `take_clipboard_worker()` returns the one-use seed exactly once.
Move it to the interactive worker or use `seed.spawn(native_factory, new_id)` as
above. The factory and item-ID qualification remain the application's obligation.
Closing the session fences a seed whether it is uncollected, moved, or already
running, including before native open. Native thread completion is still distinct
from cancellation. The explicit pre-attached `attach_clipboard` API remains
available for callers that do not select the new startup profile.

Either endpoint may decline. The ready exchange still completes and both retire
only clipboard, report `ConsentRequired`, release no seed, and keep input/viewing
usable. Setup is one-use even after refusal: it cannot reset a replay ledger or
reopen a consumed native stream. The original timeout (at most two seconds) starts
at the API call, covers readiness, and is never renewed on a later first poll,
backpressure, or phase transition. Cancellation, expiry, or malformed traffic
while an attachment is incomplete conservatively closes the original session;
only completed optional lanes support isolated retirement.

The running-session tests exercise automatic negotiation without pre-attached
clipboard routes, independent consent/refusal, in-flight keyboard input, empty,
Unicode and full one-MiB transfers in both directions, retained-send bounds,
unpolled cancellation, original timeouts, and cancellation of uncollected seeds.
They use actual UDP/TLS, session/clock/input owners and native worker threads;
clipboard OS contents and initial grant/consent metadata are explicit fixtures.
The separate `clipboard_network` tests still exercise actual X11 desktops. Neither
test group substitutes for GUI startup consent or live-tailnet qualification.

The one-MiB success cases start each direction with an actual fresh host grant.
An operation started near the end of an existing lease may correctly expire
before completion, even if the session later renews. Faster queue progress does
not extend its captured authority, view, or transfer deadlines; the application
must report that terminal result rather than automatically replay the old copy.
