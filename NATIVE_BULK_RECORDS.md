# Negotiated native bulk records

`native-bulk-records` v1 removes the unrelated critical-control reserve from the
clipboard/file record ceiling. Both peers must select this optional capability.
It is advertised by the shipped native host and client only with explicit
control and clipboard and/or file support. It grants neither a lane nor OS
permission: all existing role, input attachment, consent and authority checks
remain required. An older peer or incompatible optional version retains the
legacy record ceiling without losing the control connection.

With positive selection, the maximum clipboard/file record is the minimum of
selected protocol bytes, local stream window and local retained bulk bytes.
Default policies therefore allow 65,536 bytes rather than 8,192. Reservation,
received grants and final promotion all retain their local/negotiated bounds;
a peer with a smaller local pool refuses an oversized grant before promotion.
The native grant is not a negotiation of otherwise unadvertised pool sizes.

Configuration, input, recovery, video and audio limits do not grow. Recovery
still fits both its bulk forward and critical reverse pools. No aggregate queue
budget, stream credit, retransmission accounting, priority, item lifetime or
first-byte deadline is increased. This is not a new retry or reconnect path.

The text clipboard still permits at most 16,384 payload bytes per chunk and
1,048,576 bytes per item. With default policies, a one-MiB item takes 64 chunks
plus Begin/Commit (66 records), instead of 130 smaller chunks plus Begin/Commit
(132 records). This is an executed record-count comparison, not a bandwidth or
WAN latency measurement. Publication remains commit-only; an original-owner
revocation before Commit prevents publication and releases retained item data.

## Executed qualification

Using the pinned `nightly-2026-08-31` and unchanged lockfile/dependencies:

```sh
cargo test --offline --locked -p fr-core -p fr-wire -p fr-media -p fr-client
cargo clippy --offline --locked -p fr-wire -p fr-client --all-targets -- -D warnings
```

The four-crate run passed **1,200 tests, zero failures or ignored tests**. Its
native-profile target includes four new opt-in/version/legacy regressions.

Two external manifests also compile the exact checked-in production modules
against actual first-party dependencies, without substituting transport or
runtime implementations. The private `quic/attachment/record_limit.rs` module
passes **11 tests**: seven bound/negotiation regressions and four composition
checks using actual framing, clipboard codecs and original input/clipboard
owners. They transfer one-MiB Unicode items in both directions, split headers
and code points, verify commit-only publication and echo suppression, reject
oversized headers before allocation, preserve first-byte expiry and fence a
revoked recipient. Record-admission/publication sinks and clocks are explicit
fixtures, not native QUIC, OS clipboard or application-paste evidence.

The extracted stateless host `native_control/offers.rs` production module passes
**four tests**, including every host option combination and all 64 host/client
auxiliary combinations through actual negotiation encoders/decoders. Older
peers in either direction and incompatible optional bulk versions preserve the
other selected capabilities. Both external targets pass all-target pedantic
Clippy with warnings denied. Three isolated source mutations (restoring critical
coupling, bypassing bilateral selection, ignoring the stream window) each fail
the corresponding regression. Changed Rust formatting and whitespace pass.

## Boundaries and remaining work

The transport build was attempted but the pinned Asupersync compiler process
was killed under this environment's 4 GiB memory limit. The six added real
UDP/TLS bulk-attachment regressions were **not built or executed**. Full frd,
native-device, impairment and WAN qualification remain outstanding. No broad
acceptance bead is closed; larger records alone do not establish that a one-MiB
item meets its unchanged deadline on a high-latency path.

The build uses checksum-retained `c45b547` source/vendor inputs with the already
published audio/transport overlays. Modified production baselines and existing
test files were checked against GitHub `7d62843d31f26902d00768206325a53598dcb785`;
publication preserves that main tree and its intervening changes. Supporting
first-party files are not a complete current-main checkout, so these are scoped
source/component checks, not full current-main workspace certification.
