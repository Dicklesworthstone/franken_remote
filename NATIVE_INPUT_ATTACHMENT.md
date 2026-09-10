# Negotiated native input channels

`native-input-attachment` version 1 joins the existing one-use channel attachment
exchange to the canonical native input agent. Asupersync QUIC remains the primary
transport. No new runtime, codec, identity mode, listener or dependency is added.

This is an input-channel integration, not a complete remote-desktop application.
The native tests supply the initial admitted control pair, local control grant,
display selection and presented-view evidence explicitly. They do not establish
live Tailscale admission, protected ingress or a finished GUI lifecycle.

## Wire and transport contract

The existing attachment descriptor gains role **Input (4)**. Its primary
application direction is **viewer-to-host (2)**; the three media roles retain
host-to-viewer (1). Descriptor length, field order and the 186-byte grant/attach
records are unchanged. Unknown roles and a role/direction disagreement refuse.
Input additionally requires `native-input-attachment` version 1 and the negotiated
`RequestControl` intent; the existing `native-media-attachment` capability is also
required. Configuration-only and observer selections cannot activate input.

The handshake remains:

```text
host -> StreamBinding on established control
viewer -> BindingAccepted after opening its allocated stream
host -> ChannelTicket after arming receive credit
viewer -> ChannelAttach on the new viewer stream
host -> ChannelAttached on the new host stream
both -> activate exact input routes
```

| Route | Direction | Accepted application records |
|---|---|---|
| Ordered critical stream | Viewer to host | Existing InputActions family, including release-only HeldState |
| Critical feedback stream | Host to viewer | InputResult and InputValidityTicket |
| Bounded datagrams | Viewer to host | Pointer motion only, kind 0x0042 |

All three routes share one nonzero binding. Native stream identity, direction,
message family and delivery class remain independently checked. Pointer traffic
cannot precede attachment acknowledgement. Actions never fall back to datagrams,
and feedback cannot be submitted on the ordered action stream.

Reliable input records use the minimum of the selected control-message limit,
actual stream window, critical-send allowance and input-codec record ceiling.
Pointer datagrams are separately limited by the connection's datagram ceiling
(1150 bytes under the current transport policy), even when the reliable allowance
is larger. A smaller negotiated limit constrains both paths. Rejections do not
consume native send credit or copy a payload into a hidden fallback queue.

Exactly one input attachment may be reserved during a connection's lifetime.
A completed, dropped or retired reservation cannot create a second action-ordering
domain. Existing static InputActions routes also prevent a second attachment.
There remain at most 16 reliable routes and four datagram routes per connection;
video and pointer routes share that allowance. Incomplete attachment storage is
bounded and retains its original exclusive deadline through backpressure.

Replacement after a changed display/view requires a new checked session/grant
path; this increment does not recycle input binding identities on a live
connection. It must not be advertised as seamless input reattachment.

## Native owner integration

`frd::input_quic::NegotiatedInput` consumes the completed, non-cloneable Input
attachment and checks it against the completed Configuration attachment on the
same live connection. It requires identical host boot, OS session, remote session,
display and all geometry, viewport, codec and recovery generations, apart from
the intentionally distinct auxiliary binding IDs. Both retained protocol limits
must equal the supplied selection. A copied numeric route list cannot construct
this owner.

`check_request` verifies the exact parent, displayed configuration binding and
view in an existing ControlRequest. It does not grant control or replace the
broker's checks against locally selected bounds, native capabilities and consent.

`into_host` takes an already granted native Agent. The agent must have the same
protocol limits, remote session and view and must own the **actual shared
observation authority**, not merely matching numeric identifiers. This check
borrows its control lease; the separate one-use control-renewal owner is not
consumed. The join derives canonical QuicInput routes and retains its normal
watchdog, ticket-expiry, input-ordering, final-submission and receipt semantics.
The independent native Driver must continue running in the authority region.

Viewer sends validate the real input codec, original session/view and delivery
class before passing the unchanged bytes and absolute deadline to QUIC. HeldState
uses its existing separate release-only codec and checks session identity; it is
not reinterpreted as a ticketed action. The existing client and native owners
continue checking its lease and reconciliation barriers. Feedback dispatch reads
only the exact negotiated result/ticket stream and leaves other session/media
records to their existing owners.

Attachment is not observation, local approval, control permission, mapping
confirmation, a valid input ticket or evidence of a fresh visible frame. Dropped
or refused joins cannot authorize another native operation. Late real results
remain subject to the existing truthful receipt and cleanup behavior. A foreign
connection is refused before its queues are changed.

## Executed evidence and limits

The changes were tested with pinned nightly-2026-08-31, retaining Asupersync 0.4.10.
Thirteen new tests were added: two independent wire tests, six live transport
tests, and five native input integration tests.

The selected production checks passed **499 tests, zero failures and zero ignored
cases**:

| Selection | Passed |
|---|---:|
| Core, wire, media and client Cargo suites | 364 |
| Native input/QUIC suite, including five negotiated-input tests | 34 |
| Live channel-attachment suite, including six input tests | 20 |
| Seven existing daemon integration suites | 51 |
| Daemon unit tests, including the concurrently published seat-reservation tests | 30 |

The new native path uses actual localhost UDP/TLS, private Xvfb servers, X11
keyboard/pointer state and the real input codecs and agent. Only the initial
session-control pair is preinstalled. Configuration and input attach over the
network before effects are submitted. Cases cover key presses, pointer motion,
drag/release, returned native receipts, ticket rollover, release-only held-state
reconciliation, foreign connections, altered control targets, independent
same-ID authority objects and mismatched view generations. These are synthetic
admission/grant/presentation fixtures, not live-tailnet qualification.

Selected source/test strict Clippy, workspace formatting and documentation-link
checks passed. Runtime-bound checks freshly compiled first-party Rust and native
keyboard C against matching retained Asupersync libraries. Both attempted cold
workspace builds were killed for memory exhaustion while compiling Asupersync;
there is **no fresh full-workspace or GitHub CI pass for this increment**.

A separate source copy removed only the new shared-authority object-identity
check. The unchanged independent-authority regression then failed because the
foreign agent was accepted. The production source and test assertions were not
modified by that experiment. An initial negative-control harness setup failed
before running a test because it matched two similar predicates; the corrected
harness limited the mutation to the new method and produced the expected failure.
That harness setup error is not counted as a production test failure or pass.

The concurrently published atomic seat-reservation change from `4e7dbfe` is
preserved in the tested native agent. Its local tracking import is not part of
this feature's patch series. No unrelated control-grant work is overwritten.
No UBS run or Beads closure is claimed.

Reproduce on a provisioned checkout using the pinned toolchain:

```sh
cargo test -p fr-wire --test attachment --locked
cargo test -p fr-transport --test media_attachment --locked
cargo test -p fr-native --features linux-input-agent --test input_quic --locked -- --nocapture
cargo test -p frd --lib --locked
./scripts/verify.sh fast
./scripts/verify.sh docs
```

Protected tailnet ingress, network-to-broker control-grant orchestration and the
final application loop remain separate unfinished work. The patch contains no
wildcard listener, authorization bypass or transport substitution.

Related: [PROTOCOL_ATTACHMENT.md](PROTOCOL_ATTACHMENT.md),
[NATIVE_MEDIA_ATTACHMENT.md](NATIVE_MEDIA_ATTACHMENT.md),
[PROTOCOL_INPUT.md](PROTOCOL_INPUT.md), [QUIC_NATIVE_INPUT.md](QUIC_NATIVE_INPUT.md),
[CONTROL_LEASE_RENEWAL.md](CONTROL_LEASE_RENEWAL.md),
[SESSION_DRIVERS.md](SESSION_DRIVERS.md).

## Persistent host service

After the native join, `HostSession::into_controlled` consumes the same initialized
input owner into the [persistent controlled host](CONTROLLED_HOST_SESSION.md).
It services input results, tickets and control renewal alongside observation and
admission refresh. Keep polling the original native Driver independently; this
composition creates no new grant, permission, presentation evidence or OS sink.
