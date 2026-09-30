# Changelog

## 2026-09-30 — slow paths keep their connection

- An admitted reliable QUIC record had to be acknowledged by its send-by, and
  missing that closed the connection. At 120 ms RTT a 250 ms record could miss
  it on a healthy path, because records on one stream wait for the previous
  epoch. Such sessions ended with `transport_deadline_expired`. A record now
  has max(send-by, admission + a path allowance). The allowance is three RFC 9002
  probe timeouts from the measured RTT, clamped to 250 ms-2 s (QUIC_RECORDS.md).
  Admission still refuses records past their send-by, and a peer that stops
  acknowledging still closes the connection.

## 2026-09-29 — a repaired picture on a static screen is shown; control without a wheel

- A picture whose repair completed after its 50 ms display budget was decoded
  without being presented even when it was the newest picture. On a static
  screen nothing replaced it, so a view-only session showed the previous
  picture indefinitely without a named end (12 of 61 diagnostic repetitions
  at 40 ms RTT with 5% loss). The newest late picture is now presented; a
  superseded one is still decode-only, and late presentation is never fresh
  evidence (PRESENTATION_FRESHNESS.md). With the fix: 0 of 24.
- Control capabilities are a meet, not a static grant (fr-rc2-control-capability-meet-sf8).
  `frd run --input-agent` probes its executor once at startup (the same display
  open, no indicator, no input). Missing keys, repeat, absolute pointer or
  buttons refuse at startup (`control_capability_missing`); a failed probe is
  `input_agent_unavailable`. Line scrolling is offered as the optional
  `native-input-line-scroll` v1 capability only when the executor has it, and
  `fr` asks for the wheel only when both peers selected it. Previously a host
  with several X screens or unmapped horizontal wheel buttons failed every
  grant. `fr`'s control completion reports `control_capabilities_granted` and
  `wheel_unavailable`. Namespace e2e: a viewer wheel notch reaches the host as a
  button-4 press/release; a host without horizontal wheel buttons keeps control
  without the wheel (planted negative: the static grant fails it).

## 2026-09-28 — impairment limits, named session ends, installer options

Namespace evidence only, as below.

- Real `tc netem` impairment rows for control and view-only sessions (7832436,
  cfb2402). Control holds on a clean link and at 40 ms RTT, and ends at 60 ms RTT
  or more, or with 1% loss. View-only holds with 1% loss and at 100 ms RTT, and
  exits at 200 ms RTT. Causes isolated: near 60 ms RTT the fixed 250 ms
  source-age bound ends control, and beyond it the transport record deadlines
  do (PRESENTATION_FRESHNESS.md). The owner decision on view-lapse semantics
  and an RTT-aware bound is pending.
- `fr` names local session ends: `view_stale`, `transport_deadline_expired`,
  `host_not_heard`. A stale view met by the transport's view gate is no longer
  a bare `Unauthorized`, and any remaining generic end prints its typed chain
  on stderr.
- Transport: critical session records (renewal challenges, control responses,
  presented reports) are no longer held behind queued media datagrams. Under
  loss, that wait expired host renewal challenges and closed view-only sessions.
  View-only now holds at 40 ms RTT with 5% loss in the netem rows.
- `frd install [flags] -- <frd run options>` (systemd), validated at install
  time. User units start with the graphical session, and configuration
  refusals do not restart-loop (514af9b).
- Test oracles instead of races: the CA-bundle link policy, and UDP socket
  release checked by this process's own descriptors (e1f9c54, b2abf66).

## 2026-09-25 to 2026-09-27 — control, clipboard, audio, files, second reality check

All namespace end-to-end evidence (real processes, QUIC, X11 and codecs; fixture
LocalAPI and CA); no client on a second tailnet machine has connected yet.

- Remote control: `frd run --input-agent` + `fr connect --control` (keys, buttons,
  absolute pointer, discrete wheel) through a per-lease `fr-input-agent` XTest
  child with a mandatory indicator; consent surfaces reject controller-injected
  XTest events (a7b29ce).
- Cursor forwarding with one renderer (view-only and control); opt-in text
  clipboard; viewer-to-host file sending (and both together since 41f2201); host
  playback audio for view-only sessions, client Opus decode in the restricted
  `fr-opus-worker`.
- LeaseRevoked, Closed and CloseRequest wire kinds and the bounded close exchange.
- Root `frd ingress-helper` so `frd run` can run unprivileged (6019af0).
- Control-session deaths root-caused to IDR fragments paced four per turn and
  fixed without weakening any deadline (eb8f450, bd177a8).
- `frd run --logind-session`: lock, logout, switch or suspend of the selected
  session ends sharing and the run with a typed cause (5a8e12d).
- Operator/diagnostic strings no longer understate control or advertise the
  unserved HTTPS/h3 endpoints (e55e56e).
- Second reality check (2026-09-27): 19 new beads (fr-rc2-*), three false
  closures reopened, done-at-namespace beads blocked on a live lane (71541f6).
  Narrow CI workflows moved to nightly/dispatch (91d4e3f).
- Still open: a live two-machine run, transport interoperability (Asupersync
  pin), more than one peer at a time, hardware HEVC, Wayland/macOS/Windows/
  browser/mobile, and size (about 312,800 lines against the 240,000 planned
  maximum; the hard stop is 500,000 since 2026-09-24).

## 2026-09-12 to 2026-09-24 — audit, first host composition, withdrawn claims

- A reality check on 2026-09-23 (at e1d9cc2) found that no binary could host. It
  also found that a September 19-22 burst had closed 65 beads, including every
  phase gate, and had added fabricated success surfaces. 46 false closes were
  reopened (6fbd380).
- `frd run --software-explicit` now composes the existing host library into a
  real listener. It has been verified only in the namespace end-to-end suite,
  not on a live tailnet. Fixes along the way:
  - tailnet admission, which refused every real peer;
  - acceptance of the installed daemon's `"Peer": null`;
  - the 3-second session death (QUIC receive batching);
  - headless Xvfb cookie authentication;
  - `fr` trust roots now default to the distribution CA bundle.
- Withdrawn fabrications:
  - the `frd status` fixture output;
  - the `fr doctor` granted/passed rows;
  - the `fr robot`/`fr disconnect` success envelopes;
  - the GNOME/KDE/Hyprland and Windows GPU qualification tables (docs and runtime);
  - the Windows/macOS sandbox claims.
- CI runs every lane independently. Missing native libraries and two flaky tests
  were fixed.
- Later on 2026-09-24: departing viewers are peer outcomes (fr-704 closed);
  `frd run` started and stopped cleanly on a real tailnet host after the nft
  readback fix (real nftables prints `meta iif` by name).
- Still open: remote control (input-agent process), a live two-machine run, and the size gate (272,665 at 0b543db against
  a 250,000 hard stop).

## Native host publication bootstrap

- Join approved observation, native display discovery, explicit selection,
  channel attachment, decoder startup, and continuous capture through
  `HostSession::publish_display`. Preserve the original connection and worker,
  call-time deadlines, renewal during native waits, and fence-before-cleanup.
  [HOST_PUBLICATION.md](HOST_PUBLICATION.md) records the native evidence and limits.

## Continuous decoder-load feedback integration

- Connect the canonical negotiated decoder-metrics protocol to continuous
  host/viewer service and actual capture pacing, including input and receipts
  during supervised decoder waits. Preserve original query/reply deadlines and
  exact connection/view ownership. See [RECEIVER_FEEDBACK.md](RECEIVER_FEEDBACK.md).


## 2026-09-10 — Native control-lease renewal

Join observation delivery and native input to the same approved authority and
renew control outside the native-call mailbox. Preserve original challenge
expiry, independent revoke/cleanup, fixed transport storage, action receipts and
all ticket/view boundaries. A consumed native attachment cannot reset its replay
ledger; old owners remain fenced even across reused numeric lease identities.

The viewer's control responder belongs to its input/view lifetime, not generic
network liveness. Real UDP/TLS and X11 tests keep observation, control and tickets
alive beyond the initial lease, and verify releases, expiry, replay, backpressure
and cancellation while native calls are blocked. See
[CONTROL_LEASE_RENEWAL.md](CONTROL_LEASE_RENEWAL.md) for exact integration and
verification scope. Initial grants and the complete application loop are not
claimed implemented. No Asupersync pin or dependency changes.

## 2026-09-09 — Observation renewal over session control

- `d6b01fc`: implement the bounded Challenge/ChallengeResponse codecs and a one-response viewer owner. Preserve full session/scope bindings, opaque host deadlines, exact bytes across backpressure, replay refusal and redacted diagnostics.
- `aca5fa2`: attach one renewal owner to the existing approved observation and actual QUIC control streams. Timely matching responses install the original issue-time deadline. Queuing, ACKs, media traffic and control-scope responses do not renew observation. Unsent expiry, peer FIN/RESET, failed or abandoned I/O and owner drop end observation; connection replacement cannot redirect old challenges.

Twenty new codec, responder and real localhost UDP/TLS tests pass. The local selection totals 295 tests, zero failed or ignored, with strict selected Clippy and formatting. Runtime-bound checks rebuild first-party sources against retained matching Asupersync TLS libraries, not a fresh dependency build. Source `d6b01fc` also passed complete native-workspace GitHub verification in run 34373509056; the later combined revision is checked separately. No Asupersync release or dependency pin was changed.

Observation renewal is separate from Tailscale revalidation, local consent, source/presentation freshness, input tickets and control-lease renewal. This adds no listener or graphical lifecycle. See [PROTOCOL_AUTHORITY.md](PROTOCOL_AUTHORITY.md) for exact bytes, integration and evidence.

## 2026-09-09 — Installed Tailscale admission through final media/input effects

- `a1e3c1`: implement the Linux installed-LocalAPI boundary with root Unix peer credentials, bounded Asupersync HTTP, consistent status/WhoIs snapshots, exact node addresses, explicit sharing scope and per-connection app-capability grants. No prefix/DNS membership inference, embedded VPN, new runtime, policy mutation or public listener.
- `204e952`: add a single owned admission lifetime with shared non-renewing checks. Expiry, revoke, dropped owner/refresh, identity changes and permission changes are terminal. Bind approved observation, capture deadlines and the existing prepared-media final-send guard to that lifetime.
- Connect the same gate to `Seat::start_admitted`, canonical input enqueue/idle service, and every post-preparation native submission check. Read-only grants never invoke the native input factory; already submitted effects keep their original receipts and release-only cleanup.

The initial admission sources passed clean CI run 34314791348. The exact shared-lifetime/media objects passed full pinned-toolchain workspace verification, docs, 23 LocalAPI/lifetime tests and five root-owned synthetic media scenarios in run 34315921329. Local input-extension checks passed all eight media/input scenarios, strict first-party/test Clippy and 36 existing owner/watchdog/egress regressions. Removing only the final post-preparation gate caused the unchanged input scenario to fail on a revoked key press. The full input-extension run is 34318181209, separately revision-bound.

These fixtures exercise real Unix/HTTP/peer credentials and the production policy/owners with synthetic authority metadata, opaque media packet bytes and a recording input sink. They do not establish live Tailscale sharing semantics, TUN ingress, OS input effects or a complete network session. Primary Asupersync QUIC and concurrent decoder/critical-stream work are preserved. See [TAILNET_ADMISSION.md](TAILNET_ADMISSION.md) for the explicit app-grant profile, integration, evidence and remaining gates.

## Native HEVC over primary Asupersync QUIC

- `52bd52d`: retain one exact prepared media record across transport backpressure, with unchanged owner-bound packet identity and deadline. Service cache and pending-record expiry on idle turns; terminal close releases viewer-owned storage without revoking another viewer's observation.
- `ed14f73`: connect that canonical sender to `fr-transport::quic::QuicRecords`. Actual native capture and presentation workers now exchange HEVC through real UDP/TLS QUIC, including selective repair and cancellation-terminal I/O. The surrounding session retains ownership of unrelated control/input streams.
- `3877a9f`: fix the imported transport test's pending-offer move across retries by borrowing it; preserve the immutable capability and every assertion.

Six new native/QUIC integrations and seven egress tests passed locally. Three scenarios each present six changing images through X11 capture, direct software HEVC, real QUIC, reassembly, supervised decode and X11 readback. They force actual transport backpressure and recover both a missing reference fragment and an entirely lost final picture before idle. Separate tests cover revocation, dropped I/O futures and route substitution. The eight canonical live-QUIC tests also pass after the borrow correction.

The current core/wire/media/client Cargo suites passed 237 tests with zero failures or ignored tests; selected source/test Clippy and formatting passed. Native and runtime-bound local tests rebuild first-party code against the pinned compiler and exact retained Asupersync libraries, not a fresh full Cargo-workspace build. Combined-revision CI is a separate gate. There is no claim of live Tailscale admission, independent-peer interoperability, GPU performance, optical latency or an installable desktop. See [MEDIA_QUIC.md](MEDIA_QUIC.md) for ownership contracts, reproduction commands and measured scope.

## Native input cancellation and result delivery

- Check parent Asupersync cancellation after each native preparation and before
  final input submission; preserve an already submitted text prefix. Two
  deterministic negative controls fail on unchanged `3f4f39b` and pass after
  the fix.
- Return actual native receipts through the existing `InputResult` codec with
  their original request binding, even across cleanup and handoff. Preserve
  explicit evicted/missing/unknown outcomes and cancelled-wait semantics.
- Exercise the canonical native owner and watchdog with actual XKB/XTest,
  private Xvfb servers, idle expiry, blocked preparation, lifecycle stops and
  native repeat restoration. See [INPUT_AGENT_RESULTS.md](INPUT_AGENT_RESULTS.md)
  for locally verified test scope and remaining integration limits.

## 2026-09-08 — Input records through native submission

- `d9f8ea7`: seven bounded, allocation-free input action codecs with complete view/authority bindings, separate pointer/action sequence spaces and independent golden bytes.
- `3b8f7e9`: join wire actions to submission-time authority, replay accounting, pointer barriers, scalar-bounded text, explicit partial/unknown results and release-only held-state cleanup.
- `eca3253`: add the opt-in real X11 pointer/button adapter and native effect tests for dragging, duplicate suppression, stale pointers, expiry cleanup and mid-action revoke.
- `8f501c0`: account for FFmpeg ABI row alignment in decoder allocation admission; preserve exact wire geometry limits and expand cropped-frame regressions.

The input features add no Rust dependency, runtime, alternate transport or network listener. The native adapter does not guess keyboard layouts or claim unsupported text injection. Watchdog, identity/transport and full client/agent integration remain open. See [PROTOCOL_INPUT.md](PROTOCOL_INPUT.md), [NATIVE_INPUT.md](NATIVE_INPUT.md), and [IMPLEMENTATION_STATUS.md](IMPLEMENTATION_STATUS.md) for exact verification and remaining boundaries.

## 2026-09-08 — Encoded-media delivery

Implemented the missing encoded-picture delivery path rather than adding another set of placeholder traits.

- `5d3b23f`: add `fr-wire` with executable FRD0 video fragments, reliable recovery chunks, progress announcements and selective-repair requests; allocation-free borrowed parsing, checked fragment geometry, actual transport-sized records, and independent golden bytes.
- `0210533`: add subscription-scoped reassembly, contiguous IDR recovery, reference-ordered decoder delivery, separate display/reference deadlines, missing-final-picture repair, and memory reservations that remain charged while a decoder owns compressed input.
- `7923591`: add the sender packetizer and bounded reference cache, original-fragment retransmission, repair cadence/byte limits, unsent-reference expiry fencing, and sender/receiver loss-reordering-duplication regressions.
- `b2a2888`: add a reproducible real HEVC corpus lane and independent software decode comparison; include example parser regressions in the normal verification lane. Adopt the reviewed sender formatting without changing behavior.

The workspace remains safe Rust without additional production runtime/codec dependencies. Linux pinned-nightly source checks and offline HEVC preservation tests do not establish live Tailscale/QUIC, capture, hardware acceleration, presentation latency or OS-input support. See [IMPLEMENTATION_STATUS.md](IMPLEMENTATION_STATUS.md) and [MEDIA_DELIVERY.md](MEDIA_DELIVERY.md).

## 2026-09-08 — Authority and input replay

Earlier source work added issue-time authorization renewal, exact-deadline expiry, refusal/closure cleanup, clock and suspend fencing, bounded input receipts with a persistent consumed-sequence floor, and redacted diagnostic formatting. Source `e7d57d5` passed 76 Rust tests before the encoded-media additions. These policy components still require authenticated session and native input integration.
