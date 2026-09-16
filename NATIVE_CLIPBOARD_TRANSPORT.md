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
