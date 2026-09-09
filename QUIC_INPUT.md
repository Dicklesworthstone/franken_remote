# Native QUIC input and control isolation

The current `fr-transport` layer extends the initial exact-kind media routes in
[QUIC_RECORDS.md](QUIC_RECORDS.md). `Messages::InputActions` carries the supported
reliable key, button, relative-checkpoint, scroll, committed-text and mode records
on ONE client-initiated ordered stream. It does not accept pointer datagrams,
results, HeldState, unknown kinds or an arbitrary wildcard. Message payloads
still require their existing `fr-wire` codec and final native authorization.

An application binding may have one stream in each direction: input and results
share that binding. A pointer datagram route may share the input stream binding
in the client-to-host direction. Two parallel action streams for the same binding
are refused, as is a server-initiated action stream. Routes are authenticated
local state, not negotiated by trusting the record's flags.

Critical streams have separate bounded byte and record storage, plus a reserve
in the actual Asupersync connection send-credit balance. Bulk records cannot use
those reservations. The scheduler services critical prefixes first and skips
flow-blocked streams while preserving record/byte order within each stream.
Congestion and retransmission remain Asupersync's responsibility; this is not a
latency guarantee or a second congestion controller. Native retransmission copies
remain charged until their stream's exact empty-buffer witness matches. A stalled
bulk stream cannot pin already acknowledged critical receipts in the send budget.

`receive_ready` gates native reads before copying bytes out of an unavailable
consumer's lane. Other streams, datagrams and expiry continue to be serviced.
It never pretends that consuming network bytes means native input submission.

## Verification and unresolved upstream failure

Thirteen live localhost UDP/TLS tests pass on the pinned compiler with rebuilt
first-party sources and matching retained Asupersync TLS libraries. Five new
cases test mixed ordering, route refusal, independent storage exhaustion,
critical-credit reservation on an actual small native window, and repeated
critical credit reclamation while bulk remains blocked. Strict first-party and
test Clippy passes. Synthetic record bodies in the scheduler tests are framing
markers, not claims of native keyboard effects or tailnet admission.

A separate stress run exposed an unresolved Asupersync 0.4.10 receive-window bug:
`advance_bounded_recv_windows` advertises a higher MAX_STREAM_DATA after reads
without increasing the stream's local `recv_credit` enforcement limit. A peer
using the advertised credit then fails with `Flow(Exhausted)`. It reproduces with
a 1,024-byte stream window and four 1,024-byte bulk records, reading while driving
both endpoints. The adapter closes on that native error. This is NOT fixed by
priority scheduling, and sustained transfers beyond the initial native receive
limit are NOT qualified. No memory bound was widened or native error suppressed.
The original stress failure is retained separately from the passing readiness/
priority tests. A narrow upstream correction and a crossing-window rerun are
required, alongside the existing independent-peer and tailnet qualification work.

No public desktop listener, authentication bootstrap, second QUIC stack, runtime,
protocol kind, input grant, OS permission bypass or completed phase gate is added.

## Native effect integration

The canonical QUIC-to-native input bridge, exact returned receipts and peer
FIN/RESET fencing are implemented in `frd::input_quic`. See
[QUIC_NATIVE_INPUT.md](QUIC_NATIVE_INPUT.md) for ownership, native backpressure,
closing-drain behavior, actual X11/UDP tests and remaining qualification gates.
