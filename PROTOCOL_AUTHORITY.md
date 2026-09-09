# Authority challenges and observation renewal

The existing FRD0 `Challenge` (0x0015) and `ChallengeResponse` (0x0016) kinds
now have bounded binary codecs. `fr-client::authority::ObservationResponder`
answers observation challenges, and `frd::media::renewal::ObservationRenewal`
connects them to the canonical approved `ObservationControl` over the existing
Asupersync QUIC control pair. No alternate runtime, identity source, transport,
listener, or dependency is introduced. The Asupersync release pin is unchanged.

## Exact v0 record layout

The standard 24-byte FRD0 header uses the nonzero, locally installed session
control binding. These are never zero-bound bootstrap messages. All integers
are big-endian. The payload, in order, is:

| Field | Bytes | Meaning |
|---|---:|---|
| Session | 16 | Exact remote session identity, nonzero |
| Scope | 1 | 0 observation; 1 control |
| Lease present | 1 | Must equal scope, not an independent permission flag |
| Lease | 0 or 16 | Present only for control; exact nonzero input lease |
| Nonce | 16 | Nonzero, unpredictable, non-reusing host challenge identity |
| Host deadline | 0 or 8 | Challenge only; fixed exclusive host deadline, nonzero |

An observation challenge is 66 bytes including the header; its response is 58.
Control messages are 82 and 74 bytes respectively. Responses echo the identity,
not a replacement timestamp. Challenges are host-to-viewer; responses are
viewer-to-host. Both require reliable control delivery. Framing, optional
extensions and negotiated control-message limits retain their existing checks.
Contradictory scope/lease fields, zero identities, wrong directions/bindings,
truncation and trailing data refuse. Debug output excludes session, lease and
nonce values. Independent golden fixtures define all four forms.

## The implemented observation path

After admitted startup and local approval, the host attaches one renewal owner
to the exact QUIC connection and installed `Messages::SessionControl` routes.
The application binding comes from that route pair; the session comes from the
approved authority, not an incoming message. A shared one-time claim prevents
another clone of the same `ObservationControl` from installing a second owner.
For network use the control must have been constructed with the installed
Tailscale admission lease; the lower-level constructor remains for separately
qualified compositions and explicit test fixtures.

`service` issues a challenge when the one-second cadence is due and no previous
challenge remains outstanding. The caller supplies a qualified host entropy
source; it must not use a viewer-provided value, predictable counter or timestamp.
The adapter rejects zero and immediate reuse, but does not claim to qualify an
arbitrary callback's unpredictability. No authority lock is held during that
callback or network I/O.

The existing core fixes the challenge deadline at issuance. The adapter retains
one exact challenge through send backpressure, with a separate fixed send bound
of at most one second, capped by current application/tailnet authority. Repeated
service cannot replace its nonce or slide either deadline. Transport admission
and native ACKs do not renew application authority. On a matching observation
response, the canonical core checks that both the old authorization and the
challenge remain live, consumes the challenge once, and installs its ORIGINAL
issue-time deadline. It does not start a fresh lifetime at response arrival.

The viewer retains one fixed-size response and a separate one-second local send
bound. It treats the host deadline as opaque: host and client clock origins need
not agree. Host-deadline ordering is used only to reject reordered challenges,
not to compute local authority. `sent` is called only after those exact bytes
enter transport ownership. Failed sends retain their original deadline; a
terminal send/lifecycle failure must stop the session rather than retry with a
new challenge identity. A pending response backpressures its consumer.

## Integration and terminal boundaries

Service the host owner and the viewer responder during idle as well as before
traffic. The host's `receive` handles observation responses on its exact control
route. Valid control-scope responses and unrelated records remain the enclosing
session's responsibility; they cannot renew observation. Other handlers must be
bounded and nonblocking. Keep the existing media/input owners and independent
native watchdog, rather than calling codecs or native effects inside a network
callback. Do not run a second manual challenge issuer alongside this owner.

Host `drive` uses the retained authority context and constructs its failure guard
before returning a future. Dropping even an unpolled drive, transport failure,
nonce failure/panic and FIN/RESET revoke observation before closing attached
I/O. Dropping the renewal owner also revokes observation; it does not claim to
close an externally owned connection. A stale attachment cannot close a
replacement connection or move its old challenge to it. Old authority or unsent-challenge expiry is terminal, even
without another packet. Existing final media checks and worker deadlines still
protect effects when the network loop is not being polled.

Observation renewal is NOT source observation, pixel freshness, visibility,
local consent, a control grant, a fresh input ticket, or Tailscale revalidation.
Those checks remain independently required. The wire codec supports control
scope, but this responder/host attachment implements observation only. A full
control-lease renewal coordinator and application lifecycle/event loop remain
separate integration work; merely compiling these modules does not launch them.

## Executed verification

Source `d6b01fc` adds five codec tests and five viewer-responder tests. Its complete
committed-source native workspace verification passed in GitHub run 34373509056.
The host integration source is `aca5fa2`; its three published blobs match the
locally compiled and tested files. Later combined-revision CI is a separate gate.

Ten new `fr-native/tests/observation_quic.rs` tests use actual localhost UDP/TLS,
the existing native Asupersync connection, actual FRD0 records and the canonical
application authority. One runs beyond the original three-second grant through
multiple viewer responses. Others verify exact delayed-response deadlines,
consumed-response replay, unanswered expiry, real critical-send backpressure,
fixed unsent expiry, control-scope isolation, invalid routes, duplicate-owner
refusal, connection replacement, failed entropy, unpolled cancellation and peer
FIN/RESET. They passed repeated four-thread runs. Authorities, nonces and control
route installation are explicit test fixtures, not production entropy, live
Tailscale admission, graphical approval, native capture or physical-display proof.

A local Cargo selection passed 254 core/wire/media/client tests including
doctests. Six existing daemon regression suites passed another 31 tests. Together
with the ten new QUIC tests this is 295 tests, zero failed or ignored. The runtime
and QUIC checks rebuilt first-party sources with the pinned compiler and matching
retained Asupersync TLS build inputs; they are not a fresh full Cargo dependency
rebuild. Strict host/test Clippy, selected Cargo Clippy, formatting and document
link checks passed. Full CI results must identify their exact committed revision.

```sh
cargo test -p fr-wire -p fr-client --test authority --locked
cargo test -p fr-native --test observation_quic --locked -- --test-threads=4
cargo clippy -p frd -p fr-native --all-targets --all-features --locked -- -D warnings
./scripts/verify.sh fast
./scripts/verify.sh docs
```

This advances the session-agent, client and wire-framing work. It does not close
the broader live-tailnet, complete desktop lifecycle, control or platform gates.
