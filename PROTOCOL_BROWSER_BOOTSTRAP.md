# Browser WSS ClientHello

This specifies the browser startup record implemented by
`fr_wire::negotiation::browser`, under [PROTOCOL.md](PROTOCOL.md).
The daemon listener and Rust/WASM browser adapter that consume this record
remain integration work. Parsing a hello establishes no observation, media
readiness or input authority.

## Profile and envelope

Dedicated-connection WSS uses transport profile **3**, profile version **0**.
HTTP/3 WebTransport remains profile 2 and requires separate qualification.
The application protocol version is independently 0.

The first binary application message contains exactly one ordinary FRD0
`ClientHello` record (kind `0x0001`) with control binding **0**. A raw
`FRBA` prefix, JSON message or session ID is not an alternate hello.

| Field | Encoding |
|---|---|
| Application versions | u32 count and sorted unique u16 values, at most 8 |
| Transport profile | u8, exactly 3 |
| Transport profile version | u16, exactly 0 |
| Requested role | u8, 0 observer or 1 controller request |
| Receiver limits | The same 33-byte limits block as native negotiation |
| Capabilities | The same bounded, sorted native name/version/required list |
| Nonce present | u8, exactly 1 |
| Bootstrap nonce | Exactly 32 opaque bytes |

Integers are big-endian. The complete record is bounded to 4096 bytes and
the receiver's smaller control-message ceiling. The ordinary FRD0 envelope
validates its version, binding, flags, reserved fields and exact payload length.
Missing or truncated nonce, an unknown profile/version, invalid limits,
duplicate or unsorted versions/capabilities, and trailing bytes refuse.
An output buffer that cannot hold the record is rejected before writing.

The native encoder and decoder in
[PROTOCOL_NEGOTIATION.md](PROTOCOL_NEGOTIATION.md) retain their existing
profile-1 bytes and absent nonce. The native decoder refuses this browser
hello; browser framing cannot accidentally enter the native negotiation path.

## Authentication ownership

The caller obtains the unpredictable, short-lived, one-use nonce through the
origin-checked same-origin HTTPS bootstrap described by the comprehensive plan.
It transmits the nonce only in this binary hello, never in a URL, process
argument, diagnostic or log. The Rust hello's debug representation omits it.

The receiver must verify the actual TLS connection, exact Origin and Host/SNI,
installed-Tailscale peer membership and original ingress authority before
accepting application traffic. Only after complete hello and role validation
may it atomically consume the nonce against the original peer, role and expiry.
The codec deliberately has no authority store and cannot perform those checks
on behalf of its connection owner.

Successful transport authentication proceeds to ordinary negotiation and any
required local approval. Desktop admission, decoder readiness, fresh source
evidence and an explicit input grant remain separate steps. Auxiliary channels
require their own role-specific one-use attachment authorization.

## Evidence

The browser codec has a literal golden byte fixture, truncation and malformed
record cases, a small-output-buffer atomicity check and nonce-debug redaction.
The native negotiation fixtures verify compatibility and reject browser
records. These are wire-codec tests; independent browser interoperability and
live-tailnet qualification remain open.
