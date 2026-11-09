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

The native macOS client uses the installed
`/Applications/Tailscale.app/Contents/MacOS/Tailscale` executable. The narrow
`fr-tailnet-platform` crate owns the existing C/process boundary; the safe
`fr-tailnet` crate owns parsing, destination identity and renewal. The original
native source paths remain in `fr-native/src/tailnet_macos*`, compiled only by
the platform crate. This avoids a dependency from identity policy into the
desktop/media/host crate family and retains one bridge implementation.

`LocalApi::installed().discover` runs the fixed `status --json` command.
`LocalApi::peer_target` runs `status --json`, `whois --json NUMERIC_IP`, then
`status --json` under one original one-second deadline, including task queueing,
signature checks, reads and parsing. The address comes from the selected node's
checked address set; no caller-supplied command text, executable, environment,
proxy, loopback secret or DNS lookup is accepted. Status is bounded to 1 MiB;
WhoIs is bounded to 64 KiB. The [Tailscale CLI contract](https://tailscale.com/docs/reference/tailscale-cli)
requires `--json` before the numeric WhoIs argument.

Every exact child starts suspended. Apple's dynamic code validation must accept
Tailscale's published team and one of its official application identifiers before
the original deadline permits resume. The complete bounded code-directory
identifier is retained and compared across all three reads and later renewals.
Each CLI query has a different PID; those transient PIDs are not presented as
daemon continuity. An app replacement/update requires a fresh connection.

Both status snapshots must resolve to the same exact local and remote identity.
WhoIs must match the selected stable ID, node ID, key, user, tags and complete
node-owned address set, with exact host prefixes. Shared, jailed, explicitly
unauthorized, expired or conflicting evidence refuses. A routed address cannot
stand in for a node's own address. Unrelated peer churn and path observations do
not change destination identity. This establishes an outbound destination;
the remote host independently decides membership, sharing scope, local approval,
observation and control.

The whole lookup and its global admission slot belong to one drained blocking
task. Cancellation or a dropped caller cannot reopen that slot before the old
child has been killed and reaped. Lookup and target lifetimes retain their
original deadlines; wall elapsed time also caps them so a paused monotonic clock
cannot preserve a pre-sleep target. Both status and WhoIs key expiry cap the
three-second destination lifetime. Expired identities cannot be renewed.

The ordinary `NativeClient`, `PeerTarget`, `TargetOwner` and `TargetLease` APIs are
available on macOS. They preserve exact node-address binding, provisioned TLS
roots, hostname verification, required native ALPN, disabled early data and
same-identity renewal. No public JSON constructor can manufacture these tokens.
Custom Unix sockets, host admission, certificate provisioning and host ingress
remain unavailable on macOS. Host-shaped types are retained only for shared
client signatures; their macOS transport refuses without fabricating evidence. A signed CLI is trusted installed software, not an OS sandbox
or proof that the selected host runs a usable desktop.

Tracking: `fr-1yxy` supplies the trusted identity dependency for `fr-5a4w`.
Portable tests cover numeric command bounds, identity mismatches, routed/shared
responses, expiry and clock rollback. Native macOS negative tests reject and
reap an actual Apple-signed image without Tailscale's identity for status and both
address families. Those native tests require macOS; Linux execution or Rust-only
cross-target checking is not evidence of Apple Security validation, live
Tailscale access, Metal presentation or a complete macOS desktop connection.
