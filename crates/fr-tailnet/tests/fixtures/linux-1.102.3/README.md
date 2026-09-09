# Installed Linux LocalAPI refusal projection

Captured on 2026-09-09 at 13:23:05 UTC from the installed Linux Unix socket,
Tailscale `1.102.3-t9329c3677-ga522f65e9`. Three bounded HTTP requests
(status, selected peer WhoIs, status) verified root UID and a positive,
unchanged daemon PID before sending each request. The selected host/peer
metadata and current tailnet agreed across both status reads. The host and
selected peer had equal user IDs.

These JSON files are a **sanitized projection**, not the full responses:

- Keep only fields consumed by `fr-tailnet::metadata`; omit unrelated fields,
  user profiles and the other 16 peers.
- Replace stable/numeric node IDs, node public keys, user IDs, tailnet names
  and IPv4/IPv6 addresses consistently. Preserve equality relations and prefix
  lengths. The actual retained row had no tags or nonzero sharing markers;
  that absence does not prove membership.
- Preserve field presence, nulls, booleans, daemon version and key expiry.
  In particular, `WhoIs.Node.MachineAuthorized` was **absent**, and the
  top-level `WhoIs.CapMap` was explicitly **null**. Neither is synthesized.

The corresponding test replays this projection over real Unix/HTTP fixture
sockets in both sharing scopes. It expects `MachineNotAuthorized`, not an
authorization grant. This is schema/refusal regression evidence; it does not
qualify live membership, sharing, ingress, other platforms or a positive app
grant. Synthetic positive-grant mutations are tested separately.

See [the qualification record](../../../../../spikes/tailnet-identity/README.md)
for pinned upstream semantics, the live Rust probe and its retained results.
