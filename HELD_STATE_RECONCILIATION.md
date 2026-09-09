# Release-only held-state reconciliation

Held-state reconciliation now connects the viewer's physical snapshot to the
canonical native input owner over the existing ordered QUIC input stream. It
corrects missed key-up and button-up events without synthesizing presses,
replaying actions, renewing a ticket, or granting control.

## Published representation and compatibility

The canonical core/wire implementation already published in `48ba07a` is retained,
including its `fr_core::held_state::{HeldState, HeldStateRequest}` types. The
private, unpublished `3487bdc` / `2a4a845` patch series is integrated by behavior,
not blindly cherry-picked over that implementation.

The canonical `HeldState` record (`0x0046`) is **105 bytes**, including the normal
24-byte FRD0 header. Its 81-byte payload contains a 16-byte remote-session ID,
16-byte input-lease ID, 8-byte reconciliation sequence, 8-byte next-action
checkpoint, 32-byte keyboard bitmap and 1-byte button bitmap, in that order.
Integers are big-endian. Bit `usage % 8` in byte `usage / 8` identifies a keyboard
usage; button `b` uses bit `b - 1`. Only the existing `PhysicalKey` usages and
buttons 1 through 5 are accepted. The keyboard page is implicit in this record,
not an extra field. The earlier private draft's 107-byte layout is not published.

Zero sequence/checkpoint values are valid; zero session/lease/channel bindings,
undefined keys, high button bits, trailing payload, datagram delivery and the
wrong direction refuse. The negotiated message ceiling and FRD0 extension
checks remain in force. Parsing and the held bitset are allocation-free.

## Client and ordered transport

`InputClient::reconcile_held` accepts the actual local platform-held set. It keeps
only the intersection with this client's remembered, previously sent presses.
Successful encoding clears missing local holds; failed encoding changes neither
holds nor sequence counters. The separate reconciliation sequence does not
consume action or pointer identities or pending action-receipt slots.

The client limits periodic snapshots to one every 250 milliseconds. A platform
event loop must supply those samples and send each encoded snapshot unchanged on
the SAME reliable input stream, after its preceding actions and before later
ones. Backpressure must not move a snapshot past a newer press. A failed terminal
send stops the session rather than recreating the snapshot with another identity.
The API does not start a polling timer. `PresentedInput::reconcile_held` also
checks media-derived view freshness; a stopped, hidden, unfocused or stale view
cannot use reconciliation to resume control.

`Messages::InputActions` admits `HeldState` alongside the other ordered input
records, not pointer datagrams, results, unrelated media or unknown kinds. The
existing native mailbox still contains at most one command. An outstanding action
or unsent action receipt backpressures reconciliation, preserving its original
request binding and native result. No second input executor or queue is added.

## Release, authority and results

`InputSession::reconcile_held` releases only remotely owned keys/buttons absent
from the snapshot. An obsolete snapshot, old action checkpoint or foreign lease
cannot release a newer hold. A future action checkpoint fences input rather than
skipping an unknown action. Reconciliation sequence exhaustion does not wrap.

Every native release still performs preparation followed by final parent
cancellation, shared admission, current control and host-clock checks. Input-ticket
expiry alone does not prevent release-only reconciliation; control-lease expiry
remains terminal. Native calls never run while holding the authority mutex.

The retained `Reconciliation` reports confirmed releases and remaining uncertain
releases. A native panic uses `Reply::ReconciliationPanic` with the retained
report; it is not an action `InputResult`. Cancellation or an uncertain effect
preserves the actual confirmed prefix, fences input and invokes existing
release-only cleanup. Controller handoff waits for confirmed core/native cleanup
and native destruction. Local physical input attribution limits are unchanged.

`QuicInput` dispatches the record to `Agent::reconcile_held`, retains the local
outcome in `last_reconciliation`, and reports `Progress::Reconciliation`. Existing
connection-identity, FIN/RESET and cancellation guards continue to apply. A
snapshot has no fabricated peer acknowledgement and cannot acknowledge another
action. Already submitted effects are never represented as rolled back.

## Publication checks

Client source `683783e` and native/QUIC source `fe1121f` reuse the thirteen
exact source objects that passed GitHub run `34414296902` against `31c0f97`,
with before/after hashes and the full repository `fast` and documentation lanes.
Its route-family test incorporates
the saved series' positive ordered-delivery case for `HeldState`, retaining
negative checks for pointer traffic, results, unrelated media and unknown kinds.
The concurrently published decoder-reply route is preserved byte-for-byte.

A fresh local rebuild of first-party sources with the pinned compiler and exact
retained Asupersync libraries passed strict Clippy and 60 targeted tests: 9 core,
3 wire, 6 client, 11 native-owner/cancellation, 16 transport and 15 real X11/QUIC
cases. These local checks are not a fresh Cargo dependency rebuild. The saved
mailbox regression is also retained against the canonical API: a rejected
snapshot enqueue cannot replace the uncollected action's session, lease,
sequence or submitted outcome. Its three-test cancellation suite passes, making
the final selected total 61.
Two separate negative-control copies removed only final preparation authorization
or the stale-action checkpoint. The unchanged canonical regressions failed in
each case; the production sources and assertions were not relaxed.

The six held-state X11/QUIC integrations use actual UDP/TLS, the canonical native
thread and private Xvfb servers. They exercise missed modifier/drag release,
subsequent presses, retained keys, obsolete and foreign snapshots, action-result
backpressure, malformed state and expired-ticket cleanup. The three injected
native faults cover cancellation after preparation, uncertain submission and
panic while preserving a confirmed prefix. Grants and presentation inputs are
explicit fixtures, not live Tailscale, physical-display or process-death proof.

```sh
cargo test -p fr-core -p fr-wire --test held_state --locked
cargo test -p fr-client --test held_state --locked
cargo test -p frd --test input_agent_results --test input_agent_cancellation --locked
cargo test -p fr-native --all-features --test input_quic --locked
cargo test -p fr-transport --test native_quic --locked
./scripts/verify.sh fast
./scripts/verify.sh docs
```

No dependency, runtime, Asupersync release pin, listener or permission policy is
changed. Periodic platform sampling, controller renewal and the complete desktop
application lifecycle remain separate work; this does not close their broad
qualification gates.
