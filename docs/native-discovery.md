# Installed-tailnet machine discovery

`LocalApi::discover` supplies the machine list for a native picker/CLI through the
existing credential-checked Unix LocalAPI adapter. It performs one bounded status
read, no network scan or capability probe, and uses the canonical outbound target
validation to reject shared, expired, conflicting or unusable destinations.
The selected host identity must itself validate. Rejected candidates are counted
without including their identity in diagnostic output. Stable-ID ordering is
independent of the daemon's map ordering.

A discovered machine is not an installed/ready FrankenRemote host, and is not
observation or input authority. Its canonical name selects TLS identity only.
Connecting must call the existing fresh `peer_target`/native dial path; discovery
objects cannot be attached to a session or promoted into credentials. Host-side
admission, own-user sharing policy, approval and codec probes remain independent.

Bounds: one second per lookup, a 1 MiB response, at most 1024 peers, 128-byte IDs,
254-byte names, eight addresses per peer, and the existing shared lookup slot.
Cancellation and deadline checks surround parsing as well as socket waits. Errors
and Debug output do not contain host names, addresses, keys or response bodies.
The explicit inventory accessors are intended for local user-requested display.

Verification: `cargo test -p fr-tailnet --locked`. The discovery regressions use
synthetic Tailscale metadata over real Unix/HTTP and Asupersync. They cover chunked
responses, exclusion counts, conflicting addresses, deterministic order, kernel
credentials, cancellation, lookup contention, timeouts and the peer-count bound.
These are adapter results, not installed-Tailscale version qualification or live
tailnet/desktop availability evidence.
