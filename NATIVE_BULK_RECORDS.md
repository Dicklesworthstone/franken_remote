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
This bulk-record capability alone does not negotiate otherwise unadvertised pool
sizes. When both peers also select [`native-attachment-credit`
v1](NATIVE_ATTACHMENT_CREDIT.md), the same exchange narrows the host's grant to
the viewer's local role allowance before issuing its ticket, allowing asymmetric
endpoint budgets to interoperate within the smaller bound.

Configuration, input, recovery, video and audio limits do not grow. Recovery
still fits both its bulk forward and critical reverse pools. The bulk-record
capability does not increase aggregate queue budgets, native stream credit,
retransmission accounting, priority, item lifetimes or first-byte deadlines.
This is not a new retry or reconnect path.

The text clipboard still permits at most 16,384 payload bytes per chunk and
1,048,576 bytes per item. With default policies, a one-MiB item takes 64 chunks
plus Begin/Commit (66 records), instead of 130 smaller chunks plus Begin/Commit
(132 records). This is an executed record-count comparison, not a bandwidth or
WAN latency measurement. Publication remains commit-only; an original-owner
revocation before Commit prevents publication and releases retained item data.

## Clipboard receive window

The clipboard's native receive window uses each endpoint's existing
`Policy::stream_window` (at most 65,536 bytes), independently of its negotiated
per-record maximum. This applies to legacy and bulk-record selections.
Previously, both viewer reservation and host ticket arming configured that
window from the record grant. A legacy 8,192-byte grant therefore permitted
roughly one chunk per receive-credit round trip even when the clipboard bridge
had admitted eight bounded records.

This is a local flow-control choice, not another wire capability.
[`native-attachment-credit`](NATIVE_ATTACHMENT_CREDIT.md) defines
`byte_allowance` as a maximum **record**, including when asymmetric policies
narrow the final grant. The route and framing still enforce that exact grant.
A smaller endpoint retains its own smaller native window, and every other role
retains its previous window calculation. The connection receive cap remains at
most 524,288 bytes; per-record allocation, sender byte/record budgets, priority,
first-byte expiry and original item/authority deadlines are unchanged. An idle
clipboard consumer does not read merely to return credit.

The real UDP/TLS regressions in
`crates/fr-transport/tests/media_attachment/clipboard/bulk/window.rs` consume
the fixture's initial credit before parking the application reader. They cover
eight legacy records in one bounded flight, modern bulk records, both directions,
narrowed grants and asymmetric local windows. A further record remains
incomplete until reads return credit; oversized records still refuse before
changing usage. These are transport fixtures, not OS clipboard or WAN
qualification.

**2026-11-11 scoped validation:** the exact `record_limit.rs` production module,
its unchanged child tests and actual `fr-core`/`fr-wire` dependencies passed
**12 tests**, including the new local-window assertion and existing one-MiB
Unicode owner/framing cases in both directions. Strict Clippy and pinned
rustfmt passed. Execution of the new native UDP/TLS regressions and full
`frd`/OS/WAN qualification remains pending; these component results do not
establish one-MiB transfer completion on a live connection.

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
