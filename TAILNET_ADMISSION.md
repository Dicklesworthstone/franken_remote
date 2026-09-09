# Installed Tailscale admission and effect-bound authorization

`fr-tailnet` implements the Linux LocalAPI identity boundary and one shared,
revocable admission lifetime. `frd` connects that lifetime to its existing media
sender and input owner. Asupersync remains the sole runtime and QUIC the primary
transport; there is no replacement network stack, embedded VPN, account database,
public listener, or change to the HEVC pipeline.

This implemented profile is explicitly **policy-configured app capabilities**.
It does not replace the planned zero-setup profile or claim that same-tailnet
membership across real sharing/install variants has been qualified.

## Explicit policy profile

The administrator grants the per-connection capability
`github.com/Dicklesworthstone/franken_remote/cap/desktop`, with values such as:

```json
[{"version": 1, "observe": true, "control": false}]
```

Control also requires observation. Multiple entries combine by union. Unknown
versions, contradictory control-only entries, duplicate fields and unknown
restrictions refuse rather than dropping an intended restriction. Other
applications' capabilities are skipped, not interpreted as desktop grants.
The capability MUST be scoped in Tailscale policy to intended tailnet members
and explicitly selected tags, not external users or a wildcard granting guests.
This deployment profile trusts that administrator-controlled scope. FrankenRemote
never writes or broadens the policy itself.

`GrantPolicy` additionally enforces local scope. `OwnUser` is the default; a
different user or tagged peer requires explicit `Tailnet` scope. A tagged host
has no meaningful owning-user default and requires that explicit choice. Known
shared-in and external-sharee metadata refuses even when a grant is present.
This is not an ad-hoc local allowlist.

App capabilities come from WhoIs's **per-connection `CapMap`**, never a node's own
capability inventory. A WhoIs result, matching hostname, `InNetworkMap`, matching
email domain, zero-valued sharing fields, or a 100.x source address is not alone
permission to observe or control a desktop.

## Local authority and bounded reads

`LocalApi::installed()` targets `/var/run/tailscale/tailscaled.sock`. A custom
absolute local socket path is supported, but kernel-reported peer UID must still
be root, with a positive PID, before any query is sent. No production non-root
credential override, TCP bridge, proxy header, redirect or accept-any-authority
mode exists. Root and the host OS remain trusted. Other platforms/installations
need separately qualified local transports.

A shared client permits one active lookup. Admission reads status, WhoIs for the
exact transport peer, then status again. It requires the same daemon process and
consistent relevant snapshot, with at most one retry. Limits are 1 MiB for status,
64 KiB for WhoIs, 1,024 peers, eight addresses, 32 tags, 128 capability keys and
eight desktop grants. Duplicates and oversized lists refuse; ordinary unknown
JSON fields are skipped for forward compatibility. Asupersync handles bounded
HTTP chunked/content-length framing. Compression, trailers, truncated bodies and
non-200 responses refuse. Errors and Debug output contain no response contents,
peer names, addresses, keys, paths or credentials.

The local endpoint must be a self-node address. Status and WhoIs must agree on
stable/node/key/user/tag identity, and the source must be an exact node-owned
`/32` or `/128`. `AllowedIPs` and routed subnets cannot supply that evidence.
Expired, unauthorized, jailed and peer-API-only nodes refuse. Tailnet names are
retained for change detection, not used as independent permission evidence.

## Shared lifetime, not renewable copies

`Authorization` has no public JSON constructor and cannot be cloned. Default
validity is one second, beginning BEFORE the first LocalAPI request, configurable
only up to three seconds. A separate lookup timeout is bounded to three seconds.
Delayed responses and snapshot retries never move issuance time. Strict RFC3339
key expiry caps the deadline, fractional expiry rounds down, and wall-clock checks
also refuse crossed expiry or regression. Revalidation requires the old proof to
remain live through completion and preserves endpoint tuple, node/local identity,
policy, daemon PID and LocalAPI instance. An expired proof cannot be renewed.

`Admission` is the single refresh owner; its `Lease` clones share one state rather
than copy authorization. Checks use the retained host context/clock. Dropping the
owner, revocation, cancellation, expiry, permission change or identity change
ends that lifetime. Dropping an already-started refresh also ends it. A successful
late LocalAPI response cannot overwrite a concurrent revoke. A read-only control
refusal does not by itself revoke otherwise valid observation.

The owner creates no background task. Its enclosing structured session schedules
refresh before expiry, polls the existing input driver independently, and ends
the admission on real OS suspend/lifecycle boundaries. No state mutex is held
across LocalAPI I/O. New or changed permissions require a new application grant;
they never silently escalate an existing controller.

## Media and input integration

`ObservationControl::new_admitted` joins the shared lease to an already approved
`SessionAuthority`. Successful identity lookup is NOT local consent, media
readiness or controller ownership. Pending local approval therefore still blocks
observation. Capture deadlines use the earlier of peer-admission and application
observation deadlines. The existing final media-send guard rechecks admission,
including a record already retained across transport backpressure. Revocation
cannot be bypassed by an old heartbeat, cached packet, or fresh repair request.

`Seat::start_admitted` requires control permission before acquiring the input
seat or invoking the native factory. It uses the existing canonical Agent,
Driver, watchdog and InputSession, not another actor. Permission is checked before
copy/enqueue, on idle driver service, before authority mutation and **after every
native preparation, immediately before submission**. Losing admission stops the
existing input control and enters release-only cleanup. The original confirmed
external-effect prefix and result semantics remain intact; nothing is replayed
or represented as rolled back. Local approval, leases, tickets, view generations
and current presentation evidence remain separately necessary.

Network/session composition must use the admitted entry points. The lower-level
constructors remain for already-qualified compositions and existing tests; they
are not an alternate network authentication mode. Endpoint tuples MUST come from
the actual established transport. This module does not prove TUN ingress merely
because a caller supplies matching addresses. A TUN-restricted listener, complete
session negotiation and real lifecycle binding remain outstanding.

## Executed evidence

Initial sources published in `a1e3c1` passed a clean pinned-toolchain build,
15 LocalAPI tests and strict Clippy in
[run 34314791348](https://github.com/Dicklesworthstone/franken_remote/actions/runs/34314791348).
The shared lifetime/media sources published in `204e952` are the exact eight
objects verified on `8fb691b` by
[run 34315921329](https://github.com/Dicklesworthstone/franken_remote/actions/runs/34315921329).
That clean run passed `scripts/verify.sh fast`, documentation checks, 23 LocalAPI
and lifetime tests, and the five root-owned LocalAPI-to-media scenarios.

The input extension has local passing evidence for all eight opt-in scenarios,
strict first-party/test Clippy, and 36 existing input-owner/cancellation/result/
watchdog/media-egress tests. The earlier local core/wire/media/client Cargo
selection passed 237 tests. Runtime-bound local checks rebuild first-party code
against the pinned compiler and exact retained Asupersync libraries; they are not
a fresh full dependency rebuild. The full input-extension verification is tracked
separately by
[run 34318181209](https://github.com/Dicklesworthstone/franken_remote/actions/runs/34318181209),
which records the exact base and expected patched hashes.

The 23 crate tests cover real Unix sockets, HTTP parsing, peer credentials,
own-user/tagged/shared/routed cases, malformed metadata, bounded retries,
deadlines, cancellation, permission changes and late refreshes. Their metadata
is synthetic. Private unit-test setup can select the fixture server UID on
non-root CI; public constructors always require root.

The eight opt-in scenarios run a real root-owned Unix LocalAPI fixture through
the actual admission and media/input owners: pending-packet revoke, capability
removal, expiry, suspend, final-send revoke, read-only input refusal, idle input
cleanup and revocation during preparation. Media bytes are deliberately opaque
packetizer inputs, not a codec test. The input sink records effects; it is not
X11 or evidence of real OS injection in this slice.

A local negative control removed ONLY the post-preparation admission check from
a separate source copy. The unchanged eight-scenario test then failed because
the revoked key press reached the recording sink. Restoring the check passed.
No assertion, authority deadline or production permission check was relaxed.

Reproduction with the repository-pinned compiler:

```sh
cargo test -p fr-tailnet --locked
cargo clippy -p fr-tailnet --all-targets --locked -- -D warnings
./scripts/verify.sh fast
./scripts/verify.sh docs
cargo build -p frd --example tailnet_media_check --locked
sudo -- ./target/debug/examples/tailnet_media_check
```

With a custom Cargo target layout, use the executable path in Cargo's JSON
artifact output. Build unprivileged; only this explicitly synthetic fixture
binary needs root to exercise the production peer-credential check. It does not
connect to the installed daemon, mutate Tailscale policy or inject OS input.

## Remaining qualification

No live Tailscale credentials or administrative policy were available. Captured
sharing/version fixtures, zero-setup membership semantics, protected-interface
listener binding, browser origin bootstrap, complete network/session startup,
macOS/Windows LocalAPI variants, and OS lifecycle wiring remain open. None of the
above declares an installable unattended desktop or closes those phase gates.
This advances `fr-p1-fr-tailnet-602`, media/session integration and final input
admission while preserving their broader acceptance criteria.

Reference contracts: plan sections 6.1-6.3 and 19.2; Tailscale's
[application capabilities](https://tailscale.com/docs/features/access-control/grants/grants-app-capabilities),
[identity model](https://tailscale.com/docs/concepts/tailscale-identity), and
[LocalAPI types at 3945b82](https://github.com/tailscale/tailscale/blob/3945b82f8a9550b54c33e61d4ed2227862d53e8a/client/tailscale/apitype/apitype.go).
