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

A connection adapter must install this non-cloneable state only once per actual
connection lifetime, validate the negotiated capability and exact local routes,
and fence it on cancellation, stream closure, OS lifecycle change or replacement.
Historical copied correlations do not themselves prove a connection is live;
the enclosing session still closes its receiver and stops input on termination.
The codec/client increment does not launch a clock service or native listener.

## Executed checks

Four independent codec tests cover exact bytes, every truncation, full bindings,
directions, limits, optional and mandatory extensions, and zero-value semantics.
Six client tests cover queued delay, asymmetric uncertainty, expiry, refresh,
replay, host/client clock regression, overflow and terminal stop. These tests and
strict selected Clippy passed with the pinned compiler and rebuilt first-party
sources. Actual connection adapters and live UDP/TLS verification are the next
integration increment, not implied by these codec/state tests.
