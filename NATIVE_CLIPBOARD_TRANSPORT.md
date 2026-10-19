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

## Temporary view suspension

A stale view under a still-held controller lease pauses the original clipboard
lane. `State::Active` means the attachment remains owned, not that a paused turn
may admit payloads. The bridge preserves its sequence ledger, native worker and
stream pair, services the original connection's control renewals, and blocks new
clipboard enqueue and inbound dispatch until readiness returns. Previously
admitted transport records retain their original permits and deadlines.

`ControllerTransport::sample_maintenance` advances the conservative clock without
granting clipboard authority. Both the bridge and worker check retained operation
and handoff deadlines before treating a readiness refusal as a pause. A queued
item, three-second handoff, switch transition, cancellation or terminal authority
failure can therefore still retire the lane during a view pause. No deadline is
restarted on resume. Authenticated clock exchanges refresh only the conservative
clock model during ordinary suspension and negotiated reference recovery. They
do not renew lease proof or supply a resume ticket, and cannot revive an expired
clock or owner. Abandoning an input action that never entered transport removes
its obsolete receipt obligation; all other obligations retain their deadlines.

The native synchronizer retains its watcher and current OS selection. It does
not call the native disable operation for a view pause, read new text, or publish
incoming text while unready. It keeps at most the latest consumed change
notification, so a copy racing the post-observation check is not lost. Every
newly consumed native revision immediately invalidates older queued payload
permits before the next authority check can observe recovery, even when a Commit
has already left the sender buffer. The original publication provenance still
controls echo suppression. An unstarted candidate can resume; a partial read that already began is cancelled
and cannot be retried with a new lifetime. An unconsumed wire record is deferred
with its original ingress bound. A Commit that reached native preparation is a
consumed refusal if readiness lapses before publication.

A separately approved attachment may finish while its original controller is
temporarily paused. Creating the unique channel and opening the bounded native
owner does not read the clipboard; the first native observation still requires
fresh readiness. Fresh same-lease ticket proof and qualified visible presentation
must both return before the controller resumes, regardless of their arrival order.
Receiver close/drop and actual lease revocation remain terminal. The initial
host attachment offer also permits this bounded pause while exchanging metadata;
it does not close the parent connection merely because the view is unready.
Host deadline queries recheck native-owner revocation after reading authority,
so the first check at the suspension limit is already terminal.

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

Prepared transport admission checks the original route again after the final
connection authorizer, which may request an optional lane's retirement. A retired
stream retains its numeric tombstone to prevent reuse, but cannot acquire new
queued bytes or send accounting. Such a late retirement returns `WrongRoute`
without closing the original connection. Cancellation, lost connection authority,
retained-record expiry, and typed record refusals keep their existing precedence.
The authenticated QUIC regression retires the clipboard at this exact boundary
and then transfers control and input records in both directions on the same
connection.

An obsolete Begin or Chunk still owned exclusively by the worker outbox or
bridge pending slot has not entered QUIC. The owner discards that record and
releases its one handoff slot when the wire owner has already superseded the
item, preserving the existing Cancel-before-replacement ordering and sequence
floors. Host-side item expiry permits the same cleanup because the worker and
bridge use the same actual host timer. Controller-side expiry does not: its
independent conservative clock cursors may disagree about when the native
sender expires. A local handoff deadline alone also cannot authorize discarding
an otherwise live sender's record. These cases remain terminal.

Unsent Commit and Cancel records also retain the terminal fallback: the wire
sender may already have finished or consumed its one cancellation, so dropping
one could leave the peer's incomplete item unreleased. No consumed publication
or uncertain OS effect is retried. Between I/O turns, the bridge checks the
original lane's exact `ClipboardChannel::send_drained` witness before validating
old in-flight permits; acknowledged work no longer retires the lane just because
its retained permit expires later. Potentially transmitted invalid records
still require retirement, including failures first detected within an active
I/O operation.

The current network join remains conservative: a switch transition (including
an off/on cycle), queued work outside the safe cases above, or
superseded in-flight payload retires
the optional clipboard pair. It does not silently reopen the consumed channel
or replay buffered text when a switch is re-enabled. Input and viewing continue
when retirement is performed between I/O turns. A terminal clipboard loss first seen
inside an active QUIC future (its authority, an item's deadline, a newer local
copy, a switch, a stopped worker) retires the pair inside that same checked
operation, before its I/O: the stream reset drops queued bytes and
retransmissions as a closed connection would, and input and viewing continue.
Until 2026-10-14 it closed the whole connection, so on a 120 ms path a 40 KB
copy that outlived its deadline ended remote control. Task cancellation and a
missing clock still fence the connection, and dropping an unpolled bridge-drive
future still fences its original connection and worker. No operation touches
an equal-ID foreign connection.

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

On the viewer the lanes are authorized by the input view gate, and the
transport treats a refused authorization as lost authority: it closes the
connection. The gate also refuses in ordinary transient states, above all the
moment between submitting a picture and its visibility callback. Since
2026-10-14 the viewer pauses its clipboard (and file) lane for any turn in which
the gate refuses, instead of servicing it; records wait in their routes and the
lane's absolute deadlines still bound it. `expect_clipboard` likewise needs only
live control, not a picture confirmed visible at that instant: refusing it in
that gap retired the clipboard for the whole session. Both were found live (NVENC
1080p at 30 fps, ts2 to ts1): the session ended
`Control(Clipboard(Transport(Unauthorized)))` within seconds, or ran with no
clipboard at all. Regression tests
`an_attached_clipboard_pauses_in_the_presentation_gap_instead_of_closing_the_session`
and `negotiation_begun_in_the_presentation_gap_completes_once_the_picture_shows`
fail without the respective change.

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

The public native publisher/observer can now configure and own this lifecycle
through the [native application clipboard API](NATIVE_CLIPBOARD_APPLICATION.md),
without manually driving attachment or transporting worker seeds in UI callbacks.
