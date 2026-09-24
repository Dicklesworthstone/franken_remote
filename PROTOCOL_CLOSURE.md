# Session closure records (v0)

Implementation contract under [PROTOCOL.md](PROTOCOL.md) sections 4 and 6,
bead `fr-rc-protocol-refusal-closure-5dx`. This slice implements the
session-close scope of CloseRequest (0x001d), not control-release-only or
Closed/LeaseRevoked reporting. It is not a platform qualification report.

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
