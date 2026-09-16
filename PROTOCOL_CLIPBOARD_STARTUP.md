# Clipboard running-session readiness profile

`native-clipboard-startup` version 1 is an additional, optional capability; it
requires `native-media-attachment`, `native-input-attachment`,
`native-clipboard-attachment` and `controller-text-clipboard`, all version 1,
and a controlling session with the original completed input attachment. Peers
without this extra selection retain the explicit attachment API and do not send
or expect the new record. Selection is not native clipboard consent.

After completing its native attachment, each endpoint sends one `ClipboardReady`
(kind `0x0054`) on the original reliable **control** stream, not on the clipboard
pair. The FRD0 header binding is the original control binding. Its fixed payload
is, in order, the 16-byte host-boot ID, 16-byte OS-session ID, 16-byte remote-session
ID, 16-byte controller-lease ID, 4-byte clipboard binding ID, and one consent byte
(`0` or `1`). Integers use network byte order. No payload suffix is permitted.
The codec validates every scope component and the entire selected record ceiling.
The authenticated stream establishes direction; readiness is allowed in either
direction and never supplies native or input authority.

Neither endpoint releases a native-worker seed until its own readiness is queued
and the peer's matching readiness is consumed. This is an application readiness
barrier, not merely a native packet acknowledgement: clipboard records cannot
race the peer's promotion of the old attachment framing. If either endpoint
withholds consent, both retire the completed optional pair without producing a
worker seed. The readiness records live on control, so optional-stream reset
cannot discard the final attachment/consent acknowledgement needed by the peer.
Control/input/viewing remain usable; no refused native payload is observed.

The entire descriptor, ticket, attachment and readiness exchange keeps the
original local deadline (at most two seconds), including time before the first
poll and time under backpressure. The viewer checks the offered display/view
against its real current media tuple; the host supplies qualified non-reusing
binding and ticket identities for the approved current scope. Records awaiting
negotiation stay bounded in their original transport owner. A second start on
that session cannot reset consumed IDs or sequence history. Failure or cancellation
of an incomplete exchange conservatively fences the session; successful optional
retirement is not a mechanism for releasing incomplete reservations.

See [PROTOCOL_CLIPBOARD.md](PROTOCOL_CLIPBOARD.md) for the bounded text-transfer
records and [NATIVE_CLIPBOARD_TRANSPORT.md](NATIVE_CLIPBOARD_TRANSPORT.md) for
running-session and worker lifecycle integration.
