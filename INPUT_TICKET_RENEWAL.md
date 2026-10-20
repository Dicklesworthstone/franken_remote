# Native input ticket renewal

Input-validity tickets now travel from the canonical native input owner over the
existing authenticated QUIC input-feedback stream to the viewer. A ticket renews
only the short-lived credential for future actions. It does not create or renew
a control lease, observation authorization, input mapping, source observation,
or visible-frame evidence. The optional authority-bearing profile also reports
the original owner's already-existing control/observation deadline. The optional
readiness profile additionally acknowledges a presentation report that the host
actually applied, so a recovered client can establish the relevant readiness
transition without waiting for the conservative one-second inference window.

## Preserve in-flight input

`SessionAuthority` retains at most eight live tickets for its current control
lease. Each retains its original exclusive expiry, capped by observation and
control authorization. A new ticket does not invalidate an earlier in-flight
action. Full capacity refuses issuance rather than evicting a live credential;
expired entries can be reused. Zero and duplicate retained ticket IDs refuse.
The caller remains responsible for unpredictable, never-reused credential IDs.

Stale view, suspension, revocation and input-mode changes invalidate **all**
retained tickets, not just the latest ID. An input-mode change requires a newly
issued ticket. No ticket transition resets consumed action or pointer sequences,
restores failed actions, changes native receipt contents, or permits retry of an
unknown external effect.

## Wire representation

The legacy `InputTicket` (`0x0017`) is 128 bytes including the existing 24-byte FRD0 header.
The 104-byte body is, in order: remote session, input lease and ticket IDs
(16 bytes each); geometry, viewport, codec-configuration and recovery generations
(8 bytes each); issuance sequence, host-monotonic issue time, host-monotonic
exclusive expiry (8 bytes each). Integers are big-endian. Zero clock samples,
sequences and initial generations are valid; zero IDs or channel bindings are
not. The transmitted lifetime must be positive and at most 1,500,000 microseconds.
Trailing data, invalid mandatory extensions, wrong direction and datagram
carriage refuse; the negotiated control-message ceiling remains enforced.

### Optional authority-bearing profile

Positive selection of `input-ticket-authority` version 1 uses the same kind,
direction, feedback binding and reliable stream with a 112-byte fixed body
(136 bytes including the header). The first 104 body bytes retain the exact
legacy layout. One big-endian `u64`, `authority_until_us`, follows at record byte
offset 128. The header's payload length is 112; its version, kind, flags and
binding rules are unchanged. Legacy `Ticket`, `encode` and `decode` remain
byte-compatible; the extended profile has separate `AuthorityTicket`,
`encode_authority` and `decode_authority` APIs.

The capability is optional for compatibility. An absent or non-intersecting
optional offer leaves the legacy profile in force. An explicitly selected
unsupported version refuses. `authority_selected` validates the selection and
requires `RequestControl`, a control-message ceiling of at least 136 bytes, and
version-1 selection of `native-input-attachment`, `native-control-grant`,
`clock-correlation` and `presented-state`. The input attachment's existing media
attachment prerequisites remain independently enforced. Neither endpoint infers
the profile from record length, silently tries another decoder, nor treats an
offered capability as selected. Each decoder refuses another profile's fixed
layout. `authority_selected` is also true for the readiness profile below;
`selected_profile` chooses the exact codec.

`authority_until_us` is the actual exclusive host-monotonic cap sampled atomically
with ticket issuance from the original owner: the minimum of its current control
lease and observation authorization deadlines. It is never reception time plus a
new lifetime, an estimate from the input ticket's short lifetime, or a fresh grant.
The wire requires a valid enclosed ticket, `issued_at_us < authority_until_us`,
`expires_at_us <= authority_until_us`, and at most 3,000,000 microseconds from
issuance to authority expiry. Checked subtraction rejects wrapped or regressing
bounds. The enclosed input ticket still has its original maximum 1,500,000-
microsecond lifetime and its actual shorter expiry.

New snapshots convey the exact current authority cap, including a shorter cap;
they do not retain the maximum of old and new authority. The viewer uses its
authenticated clock correlation to conservatively project that existing bound.
Input validity and presentation readiness keep their own gates. An accepted
authority snapshot may keep an otherwise-live optional controller facility
usable within the original control grant, but cannot resume a revoked or stale
view, extend an existing clipboard item's deadline, or retime already queued
egress. Those records retain their original exclusive expiry and bytes.

### Optional readiness acknowledgement

Positive selection of `input-ticket-readiness` version 1 requires selected
`input-ticket-authority` version 1 and all of its existing dependencies. It uses
the same `InputTicket` kind and original reliable feedback route with a 120-byte
fixed body: exactly 144 bytes including the header. The first 112 body bytes are
the authority profile unchanged. One big-endian `u64`, `presented_sequence`,
follows at record byte offset 136. The header's payload length is 120. The new
`ReadinessTicket { authority: AuthorityTicket, presented_sequence }` has separate
`encode_readiness` and `decode_readiness` APIs; the decoder rejects every size
other than 144 bytes, and the selected control-message limit must admit all 144.

The acknowledgement is strictly nonzero. It identifies the latest positive
`PresentedState` report whose verified source evidence was successfully applied
to the original authority, after any required release of held keys and buttons.
It is sampled under the same authority lock as ticket issuance and the existing
minimum of lease/observation deadlines. A report merely received, replay-rejected,
obsolete, unavailable or waiting for native release is not such proof. A host
that selected this profile and has no applied report proof withholds issuance
as `ViewUnready`; it does not encode a zero sentinel, switch layouts or mint a
replacement authority lifetime.

The report's sequence is meaningful only within the ticket's exact view and
recovery generation. On a local freshness lapse, the client first fences input
and requires an explicit negative presentation report. It records that report's
sequence only after admission on its original ordered session-control stream.
A readiness ticket can resume the same-generation suspension only when its
acknowledgement is greater than this loss-report sequence and no greater than
the client's actually admitted report ceiling, and independent current
presentation/mapping evidence is fresh. Ordered delivery then establishes that
the host processed this loss before the positive report it acknowledges.
Preparing a report, receiving a heartbeat or completing a native decode creates
no report admission or readiness proof.

A later fresh-to-stale transition requires a new loss barrier even while the
client is still waiting for an earlier acknowledgement. The original suspension
deadline stays fixed. Pending negative reports retain their original bytes and
expiry under backpressure. Recovery replaces the reporter's sequence scope;
old-generation tickets cannot acknowledge the replacement's reports. Neither
this proof nor the optional authority cap replaces the short input-ticket
expiry, replays an action, renews an old clipboard/file item's deadline, retimes
queued egress, or revives a terminally stopped owner.

`selected_profile` returns `Profile::Legacy`, `Profile::Authority` or
`Profile::Readiness` from the positive selection, and rejects unsupported explicit
versions, incomplete dependencies, wrong roles and inadequate limits.
`readiness_selected` is true only for the third profile. Optional negotiation
with an authority-only peer retains the exact 136-byte layout; a legacy peer
retains the exact 128-byte layout and existing conservative resume inference.
No endpoint guesses a profile from incoming bytes or retries a weaker decoder.

### Feedback route

The route must be explicitly installed as `Messages::InputFeedback` on both
endpoints. It admits only `InputTicket` and `InputResult` (`0x0048`) on one
host-initiated, reliable, critical-priority stream, with the same binding as the
ordered input stream. Legacy exact-result routes remain supported but refuse
renewal. Route installation is an authenticated session decision; this code does
not automatically advertise a capability or infer peer support from traffic.

## Host ownership and transport

`QuicInput::renew_ticket` queues issuance in the existing single-command native
mailbox. The native owner checks current cancellation/admission and uses its
retained runtime clock when it actually issues the ticket, not a peer timestamp
or an earlier enqueue timestamp. The session must call renewal on idle turns,
after servicing ordered input, and drive the existing watchdog independently.
The API starts no thread, timer, task, listener or replacement executor.

There is at most one command or one fixed-size unsent feedback record. An action
receipt has priority over renewal and cannot be overwritten or relabelled as a
ticket. The supplied credential generator is not called while that slot is busy
or before the 250-millisecond issuance cadence. Actual ticket bytes, sequence and
native expiry survive transport backpressure unchanged. A failed or abandoned
send never issues a replacement or re-encodes old input. An expired ticket that
was never admitted can be discarded without resetting its consumed identity or
cadence; transport-retained records keep their original deadline and cannot gain
another transmission window.

Unlike a receipt for an already committed effect, a ticket cannot be sent after
input revocation. Existing connection-identity, stream FIN/RESET and cancellation
guards apply, including when an unpolled drive future is dropped. Native cleanup
still runs independently, and handoff still waits for its confirmed completion.

## Viewer deadlines

`InputClient::accept_ticket` checks exact session, lease, view, monotonic issuance
sequence and host-clock evidence. It accepts an initial matching credential once
so a bootstrap grant can acquire its actual deadline; replay cannot retime it.
`PresentedInput::accept_ticket` also requires the correlation's host boot to match
the media tracker. Clock evidence must come from this authenticated session's
existing clock exchange, not an arbitrary caller-supplied remote timestamp.

`ClockCorrelation::deadline_lower_us` derives a conservative local expiry from
the full measured exchange interval and the qualified relative clock-drift bound.
It also caps validity by the correlation's own deadline. Network delay, queued
send time and delayed callbacks are charged to the credential's lifetime; receipt
time never starts a fresh duration. Expired/regressing/overflowing correlations
are refused, not saturated into a longer authorization.

Ticket expiry alone pauses new actions and pointer updates without consuming
identities or deleting pending receipts. A genuinely newer valid ticket can
resume only an otherwise-live client with fresh presentation/mapping evidence.
Hidden, unfocused, suspended, disconnected or otherwise stopped clients never
reopen. After entering this network-bounded path, the legacy opaque-ID setter
cannot bypass it. Late input receipts retain their original meaning.

## Verification and scope

Seven core regressions cover overlapping in-flight credentials, bounded capacity,
original expiry, authority caps, lifecycle boundaries and complete mode fencing.
The core increment published as `46b7f70` reused the exact source objects from
successful full-workspace `fast` and documentation verification in GitHub run
34424299214, attempt 2. Attempt 1 exited in the pre-existing X11 input suite
without an assertion result; the unchanged rerun passed. No source, expectation
or timeout was relaxed for that rerun.

Three independent wire tests cover byte-exact layout, truncation, limits, roles,
invalid values and diagnostic redaction. Six client tests cover delayed and late
arrival, replay, foreign generations/boot, bootstrap deadline binding, pending
receipt retention and lifecycle refusal. Two clock tests cover different origins,
drift, exclusive expiry and arithmetic boundaries. Seven native integrations use
actual localhost UDP/TLS and X11/XKB/XTest for in-flight input across rollover,
input beyond initial ticket expiry, send backpressure, original expiry, receipt
priority, abandoned I/O and exact route families. Native integration grants,
visibility and shared-runtime clock evidence are explicit test fixtures, not
live tailnet approval or physical-display measurement.

Eight additional authority-profile wire regressions preserve the existing golden
legacy bytes and cover the exact appended field, both profile mismatches, every
truncation, trailing bytes, direction/channel/header/identity checks, control
limits, shorter and overflowing authority caps, optional bilateral selection,
unsupported versions and required dependencies. They establish protocol behavior;
they add no WAN latency, throughput, native clipboard or live-tailnet measurement.

Six readiness-profile wire regressions compare all three layouts against the
original independent golden bytes and the specified appended fields, reject all
six cross-profile decoder combinations, and exercise every truncation, trailing
data, role/channel/header/identity checks, full-record limits, zero proof,
inherited ticket/authority bounds, overflow edges and optional bilateral
selection. Missing dependencies and malformed explicit selections refuse through
both convenience helpers as well as the profile selector. Wire validation alone
does not establish the causal suspension/report relationship or measure native
resume latency; those belong to the original runtime and client owners.

The causal path additionally has eight core tests, ten reporter/admission tests
and fourteen client tests. These exercise applied proof under the original
authority lock, actual held-input release, repeated losses, exact report-owner
identity, transport-admission ceilings, expired and obsolete feedback, recovery
sequence restart and the unchanged suspension deadline. The client simulation
resumes a same-generation lapse in 50,100 microseconds once its admitted loss,
later presentation and host acknowledgement are available. This establishes
removal of the one-second inference wait; it is not measured native or WAN latency.

Native regressions cover the real mailbox, verified-report application, actual
localhost TLS/QUIC admission and the production streaming viewer. The readiness
bootstrap case selects the shipped offers and requires another press/release
after 3.1 seconds of control, with four distinct native receipts in total. These
daemon/native cases require the full runtime gate. They were source-reviewed but
not executed in the local 8 GiB environment, where the unchanged Asupersync
dependency prevents the daemon build from completing. The four pure crates'
executed tests are independent evidence, not a replacement for that gate.

```sh
cargo test -p fr-core --test ticket_renewal --locked
cargo test -p fr-core --test input_readiness_ticket --locked
cargo test -p fr-wire -p fr-client --test input_ticket --locked
cargo test -p fr-wire --test input_ticket_authority --locked
cargo test -p fr-wire --test input_ticket_readiness --locked
cargo test -p fr-client --test input_readiness --locked
cargo test -p fr-media --test presented_admission --locked
cargo test -p fr-media --test clock_deadline --locked
cargo test -p fr-native --all-features --test input_quic --locked -- --test-threads=4
./scripts/verify.sh fast
./scripts/verify.sh docs
```

Local runtime verification rebuilds first-party crates against matching retained
Asupersync TLS dependencies and the pinned compiler, rather than freshly building
every external dependency. The full-workspace candidate verification is recorded
separately in the publication commit. No Asupersync dependency/release change is
needed by this implementation. Controller-specific lease challenge coordination is now connected separately in
[CONTROL_LEASE_RENEWAL.md](CONTROL_LEASE_RENEWAL.md). Initial broker grants,
platform input sampling and the desktop application loop retain their separate
owners and qualification boundaries; these ticket profiles do not close broader
platform or live-tailnet qualification gates.
