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

## macOS installed-app identity and outbound connections

The native macOS client reads the standalone Tailscale app's `LocalAPI`, the
same server Tailscale's own CLI uses. The app's network extension serves it as
HTTP on a loopback port. On every start root publishes that port as the
symlink `/Library/Tailscale/ipnport`, and a per-run token in
`sameuserproof-PORT` (root:admin, 0640). `fr-tailnet` refuses the endpoint
unless the directory and both files are root-owned and not group- or
world-writable. It sends the token as Basic credentials and accepts only a
complete, uncompressed `200 application/json` response. Reading the token
requires an administrator account. The App Store variant publishes no such
endpoint and is not supported.

`LocalApi::installed().discover` makes one status read.
`LocalApi::peer_target` reads status, WhoIs for one numeric address, then
status, under one original one-second deadline that includes queueing and
parsing. The address comes from the selected node's checked address set; no
caller-supplied URI, proxy or DNS lookup is accepted. Status is bounded to
1 MiB and WhoIs to 64 KiB. All three reads must come from the same daemon run,
identified by the published port, token and publication time, so a daemon
restart between reads refuses.

What this does not check is which process accepted the connection. A client
cannot learn that, because the kernel's socket list shows an unprivileged caller
only its own sockets, and Tailscale's CLI does not check it either. If Tailscale
is not listening, a local process that binds the port would receive the token
and could answer with false metadata. The signed-CLI transport that preceded
this (`fr-tailnet-platform`) did not close that gap: the CLI itself connects to
this port with this token. It cost 60-80 ms of CPU per read, three reads per
renewal, and the owner chose the direct read on 2026-11-08.

Both status snapshots must resolve to the same exact local and remote identity.
WhoIs must match the selected stable ID, node ID, key, user, tags and complete
node-owned address set, with exact host prefixes. Shared, jailed, explicitly
unauthorized, expired or conflicting evidence refuses. A routed address cannot
stand in for a node's own address. Unrelated peer churn and path observations do
not change destination identity. This establishes an outbound destination;
the remote host independently decides membership, sharing scope, local approval,
observation and control.

The whole lookup holds one global admission slot. Cancellation or a timeout
drops its socket and releases the slot. Lookup and target lifetimes retain their
original deadlines; wall elapsed time also caps them so a paused monotonic clock
cannot preserve a pre-sleep target. Both status and WhoIs key expiry cap the
three-second destination lifetime. Expired identities cannot be renewed.

The ordinary `NativeClient`, `PeerTarget`, `TargetOwner` and `TargetLease` APIs are
available on macOS. They preserve exact node-address binding, provisioned TLS
roots, hostname verification, required native ALPN, disabled early data and
same-identity renewal. No public JSON constructor can manufacture these tokens.
Custom Unix sockets, host admission, certificate provisioning and host ingress
remain unavailable on macOS. Host-shaped types are retained only for shared
client signatures; their macOS transport refuses without fabricating evidence. The
installed app is trusted software; its answer does not prove that the selected
host runs a usable desktop.

Tracking: `fr-1yxy` supplies the trusted identity dependency for `fr-5a4w`.
Portable tests cover identity mismatches, routed/shared responses, expiry and
clock rollback. macOS tests cover the endpoint reader's ownership, port and
token checks. On 2026-11-08 (macOS 27, Tailscale 1.104.1) `fr hosts`,
`fr inspect` and desktop sessions ran against the installed app. Those native
results require macOS; Linux execution or Rust-only
cross-target checking is not evidence of Apple Security validation, live
Tailscale access, Metal presentation or a complete macOS desktop connection.
