# Approved display inventory

`Viewer::inspect_displays` completes the same native session negotiation and
optional host-local approval as normal viewing, receives one bounded validated
`DisplayCatalog`, and closes the original session before returning the snapshot.
`ViewerSession::inspect_displays` provides the same operation for an already
approved session. Both consume their viewer; neither selects a display, attaches
media, launches a decoder, or requests an input lease.

The methods use `ObserverPolicy`'s existing total call-time budget and network
turn limit. Unpolled time and approval are included. Observation renewal continues
while the catalog is pending. Invalid configuration, cancellation, expiry,
malformed/foreign records, callback failure and callback unwind fence the original
session. Approval callbacks are notifications; they cannot authorize disclosure.
An empty catalog is valid metadata, not proof that a usable desktop exists.

The fixed-size catalog preserves the existing eight-display bound, signed origin,
pixel/logical dimensions, rational scale, post-rotation geometry, opaque handle
and geometry generation. The result is information, never authority: aliases and
catalog revisions are session-local. A later connection must obtain and validate
its own current catalog before selecting any display. Do not cache an inspection
as a grant, permit a read-only client to move the host pointer, or infer a primary
display from array order.

Verification uses the production startup, observation-renewal and selection
owners over actual localhost UDP/TLS. Host identity and OS display metadata are
explicit fixtures. Success, empty/maximum catalogs, delayed publication with
renewal, host-local approval, original deadlines, abandonment, callback failures
and foreign-session rejection are tested. No installed-Tailscale, native-media,
physical-display or transport-interoperability qualification follows from these
checks.

```sh
cargo test -p frd inventory --locked
cargo test -p frd --lib --locked -- --test-threads=1
```
