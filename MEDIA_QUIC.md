# Native media over Asupersync QUIC

The host's `frd::media_quic::QuicEgress` now connects the canonical observation-bound media sender to `fr-transport::quic::QuicRecords`. It adds no runtime, codec, wire kind, public listener, or competing transport. Asupersync QUIC remains primary. The session still owns the connection and may dispatch other streams on it; this media owner neither consumes nor grants input authority.

## Ownership and failure behavior

`media_egress::Egress` retains one prepared record until transport admission succeeds. Repeated backpressure preserves the exact bytes, owner-bound `PacketOffer`, original frame/channel and absolute deadline. Switching from original delivery to repair cannot bypass that pending record. Idle `tick` checks cache and pending-offer expiry and frees viewer-owned storage on terminal failure. Closing a subscription does not revoke another viewer's observation or shared capture worker.

`QuicEgress::transmit` maps the actual channel to its installed QUIC route and gives QUIC the original send deadline and final authorization callback. Backpressure retains the prepared record; uncertain transport failures close media and the connection rather than replaying a partially admitted reliable stream. QUIC remains responsible for retransmission, congestion control and bounded native stream buffering.

`QuicEgress::drive` checks current observation/cache validity around actual native I/O. Dropping a suspended drive closes both media storage and the native connection immediately. The independent session/input watchdog must remain scheduled during waits and propagate connection closure to input revocation; closing media alone is not evidence of OS key release. The containing session must also arrange graceful worker shutdown or supervised kill/reap.

Repair dispatch validates the exact installed inbound route before using the existing bounded repair cache. Stale, overlapping or rate-limited repair requests are explicitly refused and consumed without extending retention. Malformed requests are errors for the session owner. Other control/input messages remain the responsibility of the session router, not this module.

## Executed native integration

`crates/fr-native/tests/media_quic.rs` uses two private Xvfb servers, the actual capture and presentation child processes, direct FFmpeg software HEVC, the real record codecs, and actual authenticated Asupersync QUIC over localhost UDP with ephemeral CA/hostname verification. No fake codec, manually advanced TLS state or in-process byte transport substitutes for those components.

Three scenarios each capture and present six changing 320x240 tile images. The last scenario stops producing new captures after the sixth frame, deliberately loses all its original video fragments at the application receive boundary, and requires reliable progress plus selective repair to make that final picture decodable and visible. A separate scenario loses one reference fragment. Controlled X11 readback checks an expected tile after every native completion. The sender is restricted to one retained reliable record to force real transport backpressure.

A representative local run with FFmpeg 7.1.5 produced:

| Scenario | Presented frames | Admitted records | Deliberately dropped fragments | Repair requests accepted |
|---|---:|---:|---:|---:|
| No injected loss | 6 | 28 | 0 | 0 |
| One lost reference fragment | 6 | 29 | 1 | 1 |
| Entire final picture lost | 6 | 32 | 4 | 1 |

All scenarios observed actual backpressure. Exact retry counts vary with runtime scheduling. The tests assert the selected transport record/byte ceilings and that terminal close releases sender-owned cache and prepared-record storage. Three additional tests verify revoke between native admissions, cancellation by dropping a live drive, and route/binding/direction refusal.

The six integration tests and seven egress tests passed with the repository-pinned nightly; strict first-party/source/test Clippy and workspace formatting also passed locally. Native verification rebuilt first-party libraries and the actual C bridge/worker against matching previously retained Asupersync 0.4.10 libraries. It is not a fresh full Cargo-workspace rebuild. Existing C bridge indentation warnings and Xvfb startup diagnostics remain visible. Combined committed-source CI must be cited separately by revision.

Reproduce on a provisioned Linux checkout:

```sh
cargo test -p frd --test media_egress --locked
cargo test -p fr-native --features linux-media --test media_quic --locked -- --nocapture
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
./scripts/verify.sh fast
./scripts/verify.sh docs
```

## Deliberate evidence limits

Loss is injected after genuine QUIC reception, not as physical link loss. The 250ms display-budget test profile is bounded correctness evidence, not a 50ms latency or bandwidth claim. Xvfb readback is not optical scanout. Software encoding is not GPU qualification. The test shares locally established routes, authority grants, and a real encoder-derived decoder configuration during setup; it does not implement or qualify the network's admission/startup negotiation. There is no live Tailscale session, independent QUIC peer, browser/mobile presentation, installer, or fully usable remote desktop claimed by this slice. Tailnet membership, OS lifecycle, fair mixed-stream scheduling and end-to-end authenticated session integration remain open.
