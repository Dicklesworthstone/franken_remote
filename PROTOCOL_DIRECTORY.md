# Terminal tailnet directory (v1)

A host started with `frd run --directory` answers an admitted viewer's
request for the machines it may want to reach next: the host itself and the
viewer's own machines, as the host's installed Tailscale daemon sees them.
The iOS app uses it because Tailscale gives other iOS apps no way to list a
tailnet (docs/ios-client.md). The required capability is
`native-tailnet-directory`, version 1, within native profile 1/version 0 and
application version 0. Code: `fr-wire/src/directory.rs` (wire),
`fr-tailnet/src/directory.rs` (selection), `frd/src/session_startup/opening/directory.rs`
(host), `fr-client/src/startup.rs` and `frd::session_startup::Viewer::fetch_directory`
(viewer).

## Authority

The directory is off unless the host's operator passes `--directory` (also
accepted by `frd install ... -- --directory`). It is served only on a
connection that already passed the host's ordinary admission: native TLS with
the host's certificate, Tailscale `WhoIs` of the peer, and the host's sharing
policy. It never starts capture, approval, a worker or a session, and a
listing grants nothing: every listed machine is reached by its own name, under
its own certificate and its own host's admission.

What is listed, from one `LocalAPI` status read after a fresh admission
refresh:

- the serving host first (Tailscale `Self`, which must be the admitted host
  identity and running);
- then each peer of the **viewer's own Tailscale user**: untagged, not shared
  in or out, not expired, present in the network map, with a `MagicDNS` name of
  exactly `<label>.<tailnet suffix>` and at least one tailnet address;
- never the viewer's own node, and nothing for a tagged viewer (user 0) beyond
  the host itself;
- sorted by name, duplicates (name or node ID) dropped, at most 256 entries.

Each entry carries the `MagicDNS` name (lowercase, no trailing dot), the stable
node ID, Tailscale's OS label (empty when absent or unusual), the online flag,
the serving flag, and at most one IPv4 (`100.64.0.0/10`) and one IPv6
(`fd7a:115c:a1e0::/48`) address. A machine the wire cannot carry exactly is
left out and the listing is marked truncated; a host that cannot list itself
refuses. Logs and Debug output never show names, node IDs or addresses.

## Exchange

The viewer prepares an Observe offer with the marker required (control intent
is refused, never downgraded, and the marker never combines with capability
inspection). After ClientHello:

1. A host without `--directory` refuses with `DirectoryDisabled` (16). A host
   that predates the directory refuses the unknown required capability
   (`RequiredCapability`, 4). Neither reads anything.
2. Otherwise the host answers HostCapabilities: the ordinary intersection plus
   the required marker.
3. The viewer sends exactly one `DirectoryRequest` (0x0086), the only record
   it may send next. SelectedConfiguration, approval and every session record
   are refused in this mode.
4. The host fences the startup (no session can follow), refreshes the
   admission, reads its daemon once and sends the `DirectoryListing` records
   (0x0087), or `DirectoryUnavailable` (17) when the read or the listing
   fails. It waits up to one second for the viewer to close first, then closes.

The whole exchange is bounded by five seconds from each startup owner's
creation, parked time included. Both records travel only on binding 0 of the
bootstrap control pair, with no extension area.

### DirectoryRequest (0x0086), viewer to host, 28 bytes

The ordinary 24-byte FRD0 header (application version 0, flags zero, binding
0, payload length 4, extension length 0), then `version:u16 = 1` and
`reserved:u16 = 0`. Exactly one form is valid.

### DirectoryListing (0x0087), host to viewer

One or more consecutive records, each at most the negotiated control record
size, together at most 128 KiB and 64 records. Payload:

| Field | Encoding |
|---|---|
| sequence | `u8`, 0, 1, 2... with no gap or repeat |
| flags | `u8`: 1 final, 2 truncated (only with final) |
| count | `u8`, 1..255 entries in this record |
| reserved | `u8 = 0` |
| entries | `count` entries |

Entry:

| Field | Encoding |
|---|---|
| flags | `u8`: 1 online, 2 serving, 4 has IPv4, 8 has IPv6 |
| name | `u8` length + lowercase LDH labels, at least two, at most 253 bytes |
| node ID | `u8` length + 1..64 of `[A-Za-z0-9_-]` |
| OS | `u8` length + 0..32 of `[A-Za-z0-9 ._-]` |
| IPv4 | 4 bytes, only with flag 4; tailnet range |
| IPv6 | 16 bytes, only with flag 8; tailnet range |

At least one address; serving implies online. Only the first entry of the
listing is serving, and names and node IDs are unique across all records.
Unknown flags, a broken bound, a record after the final one, or any record
other than the next listing or a typed refusal ends the startup.

## Viewer use

`fr_directory_fetch` (fr-mobile-ffi) fetches over the same named,
certificate-checked and pinned connection a session uses. Each listed
machine with a dialable `MagicDNS` name gets a pin of exactly the listed
addresses; a session to it (`FrConfig.direct`) dials those addresses without
the resolver, and TLS still verifies the certificate for exactly that name.
The app shows only what the host sent and shows a refusal as one.
