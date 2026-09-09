# Session-bound clock correlation

The positively negotiated `clock-correlation` capability, version 1, adds two
native v0 records on the already bound reliable session-control pair. They are
not zero-binding bootstrap records, authority renewals or source observations.
Do not advertise this capability until the connection implements both directions.

## Record layout

After the standard 24-byte FRD0 header, both records contain the exact 16-byte
host boot, 16-byte OS session, 16-byte remote session, and an 8-byte nonzero
sequence. All fields are big-endian. `ClockProbe` (0x0084) is viewer-to-host and
80 bytes total. `ClockReply` (0x0085) is host-to-viewer and appends an 8-byte
host-monotonic microsecond sample, for 88 bytes total. A zero clock sample is
valid; a zero binding, generation identity or sequence is not. Normal extension,
framing and negotiated message-size limits remain enforced.

## Measurement contract

The viewer records its local start BEFORE trying to enqueue the probe. The host
samples its retained session clock only after receiving that exact probe. The
viewer records its local finish after the actual reply arrives. Client timestamps
are never sent to or trusted by the host. Sequence numbers distinguish exchanges
within one connection; they are not authentication or unpredictable credentials.

The whole start-to-finish interval, including queueing, transit, host work and
delayed callbacks, belongs to uncertainty. No symmetric-network or half-RTT
assumption is made. The existing `fr-media::freshness::ClockCorrelation` performs
the checked cross-clock arithmetic and drift accounting. A sample does not
establish visibility, source freshness, observation permission or input authority.

`fr-client::clock::ClockExchange` retains one exact probe across backpressure.
Transport admission never resamples its start. Only a matching, queued response
can complete it. Unsolicited, replayed, foreign-binding and regressing replies
are terminal refusals. Pending expiry is exclusive; later traffic cannot restart
that owner. Refresh does not extend the previous correlation's original validity.

## Native session integration

`frd::media::clock::ClockSync` implements both native endpoints over the existing
Asupersync QUIC connection. The host entry `ClockSync::from_opened` takes the
actual `OpenedSession` binding, selected capability, approved observation owner,
installed admission and control routes. The viewer entry takes its authenticated
server connection, accepted startup selection and dedicated session context.
Lower-level host construction remains for separately qualified compositions and
explicit test fixtures; matching numeric IDs alone is not admission or consent.

The connection records one clock attachment for its lifetime. That
claim stays consumed after the previous attachment is dropped, preventing a new
sequence-one exchange from accepting an earlier reply. Both directions must be
the exact installed, reliable, critical-priority session-control routes, with
room for the reply and limits no larger than the negotiated selection. Missing
capability, bootstrap binding, wrong roles or routes refuse construction.

The adapter samples retained runtime clocks itself: before viewer enqueue,
after actual host receive, and after actual viewer receive. The host retains one
88-byte reply including its ORIGINAL sample through backpressure. Its send bound
is at most one second, capped by the observation and tailnet deadlines. The
viewer reuses the shared `ClockExchange` implementation rather than a second
state machine. It starts a bounded refresh after successful receipt, at most once
per second with default policy. The host refuses sequenced requests arriving
inside a 100-millisecond minimum interval. A native viewer requires correlation
validity of at least 400 milliseconds; unusually short policies explicitly refuse.

Service and receive turns delegate unrelated records to the enclosing session.
They never discard an authority challenge, input result or media record to make
clock progress. `receive` may return an unrelated record as blocked for another
handler; all callbacks must be bounded and nonblocking. The connection is driven
by one owner at a time. This path adds no thread, task, runtime or listener.

Every operation checks its original connection identity and live session
context. Pending expiry, malformed or replayed replies, FIN/RESET, cancellation,
failed/abandoned I/O and owner drop terminate sampling. Host termination revokes
observation and shared admission; viewer termination cancels its dedicated
session context. Even dropping an unpolled drive future invokes the failure
guard. An obsolete attachment cannot close a replacement connection.

A successful sample is not a source observation, visible-frame acknowledgement,
permission renewal, control grant or input-validity ticket. `correlation` returns
only a still-valid measured result after checking the native connection. Copied
historical values cannot themselves prove a connection is live: the enclosing
session must still close its receiver and stop input on termination. Real OS
suspend/visibility callbacks and the complete application loop remain separate
integration work, not implied by this library attachment.

## Executed checks

Four independent codec tests cover exact bytes, every truncation, full bindings,
directions, limits, optional and mandatory extensions, and zero-value semantics.
Six client tests cover queued delay, asymmetric uncertainty, expiry, refresh,
replay, host/client clock regression, overflow and terminal stop. Published
codec/client source `344ea32` passed clean full workspace and native QUIC
verification in GitHub run 34381762350.

Eleven native tests in `crates/frd/tests/clock_sync.rs` exercise actual localhost
UDP/TLS with the existing Asupersync endpoint. They cover both directions of
send backpressure, original timestamp retention, delayed reply receipt, missing
capability and wrong routes, sticky ownership, replay/wrong boot, lost-response
expiry, FIN/RESET and dropped unpolled I/O. A combined test runs clock sampling
and observation renewal on the same control pair beyond the original three-
second authorization. Another supplies the measured correlation to the existing
view tracker and verifies that independent visibility evidence is still needed.
Its media bytes and visibility completion are explicitly local fixtures, not
actual HEVC decoding or physical-display evidence.

All eleven passed three repeated four-thread runs. A negative control changed
ONLY the viewer enqueue-success handler to reset the request start timestamp.
The unchanged real-backpressure test failed its uncertainty assertion with that
variant, demonstrating that its queue-delay check detects the incorrect behavior.
The correct sources pass a broader 328-test selection with zero failures or
ignored tests, strict selected Clippy, formatting and documentation checks. Local
runtime checks rebuild first-party sources with the pinned compiler and matching
retained Asupersync libraries; they are not fresh dependency rebuilds.

Before publication, the exact seven-file runtime candidate passed the clean
full-workspace `fast` and documentation lanes in GitHub run 34384442755 on
`9236c26`, after applying the reviewed patch with checked before/after hashes.
The verifier exported all seven source objects only after successful checks and
verified their Git hashes again. Publication reuses those exact objects, not a
reconstructed patch. Later committed-revision CI is a separate result.

The earlier run 34383579002 failed during package-index setup, before staging or
compilation. The successful run used the runner's existing package indexes;
no repository configuration, integrity check or source/test assertion was changed.

Reproduction with a provisioned pinned-toolchain checkout:

```sh
cargo test -p fr-wire -p fr-client --test clock --locked
cargo test -p frd --test clock_sync --locked -- --test-threads=4
cargo clippy -p frd -p fr-transport -p fr-client -p fr-wire --all-targets --locked -- -D warnings
./scripts/verify.sh fast
./scripts/verify.sh docs
```

The Asupersync dependency pin and release process are unchanged. No claim of
live Tailscale qualification, independent clock hardware, bounded physical
latency, complete controller renewal or an installable desktop is made. This
advances the existing client and transport beads without closing their broader
acceptance criteria.
