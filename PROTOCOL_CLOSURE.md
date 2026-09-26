# Session closure records (v0)

Implementation contract under [PROTOCOL.md](PROTOCOL.md) sections 4 and 6,
bead `fr-rc-protocol-refusal-closure-5dx`. This slice implements the
session-close scope of CloseRequest (0x001d) and the Closed (0x001e)
codec, not control-release-only. LeaseRevoked has its separate contract. It is not a platform qualification report.

## CloseRequest

Use the ordinary 24-byte FRD0 header on the existing reliable connection-control
stream, Viewer to Host, with the installed nonzero compact control binding.
The fixed payload is `remote_session:16 bytes, scope:u8, reason:u16`, all
integers big-endian. The complete record without extensions is 43 bytes.
The session must match the original admitted remote session, independently of
compact binding equality. Zero identities, wrong direction, datagrams, unknown
reason/scope values, leftovers and invalid extensions refuse. All bytes,
including extensions, count toward the existing negotiated record ceiling.

Scope 1 closes the entire remote session. Scope 0 reserves control-release-only;
it currently returns `UnsupportedKind`, never silently becoming session close.
Reasons: 1 requested, 2 client stopping, 3 client failure, 4 inspection complete.
No request includes credentials, free-form text, or an assertion about external
effects. Repetition cannot resurrect a session, and a fresh session is not
addressable by reusing a numeric compact binding.

Independent fixture: inspection complete, binding 7, remote session 13:

```text
46 52 44 30 00 00 00 1d 00 00 00 00 00 00 00 13
00 00 00 07 00 00 00 00
00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 0d
01 00 04
```

## Host dispatch and ownership

The existing observation-renewal dispatcher recognizes the request on its exact
control route. A valid session-close immediately revokes the original
ObservationControl and its authority, discards outstanding renewal state and
returns the existing `PeerClosed` lifecycle result. The receive batch terminates
before any following application record can dispatch. Malformed close requests
also fail closed through the original I/O guard; they never reach an application
handler as a permissive fallback.

The enclosing session's existing teardown retains responsibility for held-input
release, worker shutdown and final receipts. This dispatcher does not mark those
effects complete and does not cancel the OS share's capture region. Another
session's observation owner is not revoked, including under deliberate numeric
ID reuse in tests. Receipt of this request is not a `Closed` acknowledgement;
absence of a final cleanup report remains unknown at the client.

Wire regressions cover exact independent bytes, all stream splits/truncations,
invalid identity/direction/channel/scope/reasons and short buffers. Native host
regressions exercise the production host/viewer drivers over local TLS/QUIC/UDP
with private fixture admission, including a queued record behind the close,
malformed requests and independently owned sessions. These tests are not
installed-Tailscale or hardware qualification.

## Shipped display-inspection client

`fr displays` calls the fresh `Viewer::inspect_displays` path. After receiving
the complete validated catalog, that path stops renewal/clock service and
attempts one CloseRequest with reason InspectionComplete. It uses the original
reliable control stream, original destination guard, and original pending-send
deadlines. Backpressure retains the same prepared bytes. A 100-ms outer timer,
16-turn ceiling, and the shorter original inspection/silence budget bound the
send-only drain. It never dispatches new observations or attaches channels.

The catalog remains a successful snapshot if the best-effort close cannot be
delivered. Local teardown still runs, including on cancellation and unpolled
abandonment. Transport acknowledgement or a dropped connection is not a final
host-cleanup report. The CLI explicitly labels remote cleanup as unconfirmed.

This drain is deliberately not shared with active-desktop teardown. Nor does it
run for `ViewerSession::inspect_displays` on a caller-owned existing session:
that caller might already have queued auxiliary work through its public I/O
loan. Only fresh Viewer startup owned throughout the inspection is eligible;
existing-session inspection retains its immediate local-close behavior.


## Closed (0x001e)

A final host-to-viewer session report on the ORIGINAL reliable control stream,
with its installed nonzero compact binding and matching 128-bit remote session.
Authority must be fenced and ordered teardown entered before reporting; the
codec itself does not perform cleanup. No report is admitted before SessionOpened.
The payload is exactly 28 bytes (52 including the FRD0 header):

| Field | Bytes | Meaning |
|---|---:|---|
| remote_session | 16 | Original session, big-endian |
| reason | 2 | Stable final reason below |
| cleanup | 1 | 1 unconfirmed, 2 complete, 3 incomplete |
| effects_known | 1 | 0 unknown, 1 counts supplied by the original input owner |
| pending_actions | 4 | Actions still awaiting a final receipt |
| uncertain_actions | 4 | Final receipts with unknown/partial external effects |

Final reasons: 1 client requested, 2 host stopping, 3 authority expired,
4 permission lost, 5 view invalidated, 6 protocol error, 7 host failure,
8 session replaced. These codes are independent of CloseRequest's reasons.
Unknown effects MUST encode both counters as zero; this is distinct from known
zero counts. Unknown flags, reasons, cleanup values and noncanonical unknown
counts refuse. The ordinary header-extension and negotiated-size rules apply.

Cleanup completion does not roll back input or imply complete effect accounting;
uncertain effects can remain after successful native cleanup. Individual
InputResult receipts must remain available and cannot be replaced by these
counts. A closed socket, absent report or transport ACK never supplies missing
counts, confirms physical key release, or completes another viewer's shared
worker. Unconfirmed/incomplete reports retain that distinction at the client.

Independent fixture: binding 7, session 13, host stopping, cleanup unconfirmed,
effects unknown (not captured from the encoder):

```text
46 52 44 30 00 00 00 1e 00 00 00 00 00 00 00 1c
00 00 00 07 00 00 00 00
00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 0d
00 02 01 00 00 00 00 00 00 00 00 00
```

The wire tests include this fixture and an independently specified known-effect
fixture, every truncation/stream split, wrong roles/bindings, pre-admission
refusal before allocation, extensions, canonical summaries and short output.
This wire slice does not by itself send a report or prove native cleanup.
