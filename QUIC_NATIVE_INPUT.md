# QUIC records through native input and returned receipts

`frd::input_quic::QuicInput` connects an existing admitted QUIC connection to the
canonical native `Agent`, while its `Driver` continues polling independently in
the Asupersync authority region. This is an implemented transport-to-effect path,
not another input executor, listener, handshake, or permission source.

The initial join is source `4a220c61bf74e4063206766ca86a6f9389eab77b`. Source
`dbd85bfece1c4882d9cd65331441a9c09e5d074d` adds immediate peer FIN/RESET fencing
when native input or receipt delivery is backpressured. Both preserve the
existing native owner, final admission checks, input codecs and receipt stages.

## Ownership and admission

The session supplies the already admitted `QuicRecords`, native `Agent`, shared
clock/authority context and locally installed routes. All reliable input kinds
use one client-initiated ordered stream; the reverse result stream and optional
pointer datagrams have the same application binding. Exact stream, direction,
message family, maximum size and critical priority are checked against the
connection's actual installed routes. An opaque connection-owner identity keeps
an old attachment from binding to a replacement with recycled numeric IDs.
A stale attachment revokes its own agent but cannot close the replacement or
send the old agent's receipts over it.

For network use, create the canonical agent through `Seat::start_admitted` with
the installed-Tailscale admission lease, the independently established local
consent/control grant, and the qualified native factory. The QUIC authorization
callback checks connection/session admission, not just a record's claimed lease.
The agent's shared admission and session authority are checked again immediately
before native effects. This layer never creates identity, consent, control,
readiness, or a fresh ticket. The shared admission implementation lives in
[fr-tailnet](crates/fr-tailnet/src/lib.rs); live ingress qualification remains
separate.

Keep polling the independently owned native `Driver`. On each bounded session
turn, service completed native results, receive available records, and drive the
actual connection; continue idle service when no new action arrives. Other media
or control routes remain with the containing session's handlers. Those handlers
must be bounded and nonblocking, not call a codec or native input synchronously.

## Bounded actions, backpressure and results

The bridge retains at most one native command or one fixed-size unsent receipt.
It does not retain another copy of the input payload. While either is pending,
the input lane's readiness is false, preserving the transport's bounded native
flow credit instead of growing another mailbox. An ordered action receive turn
precedes the pointer turn, so a buffered button/key release wins the one native
slot ahead of pointer traffic. Existing pointer barriers reject obsolete motion
without rewinding a newer click or release position.

The canonical agent's actual result is encoded once with its originating
session, lease, channel and action/pointer sequence space. It keeps a fixed
one-second send deadline measured when that native reply is collected; repeated
polls and transport backpressure never extend it. Once admitted by QUIC, the
transport's existing retained-buffer accounting owns the bytes. `ReceiptQueued`
means admission to that transport, not peer delivery or observed application
execution. No input is replayed with a renewed ticket to recover a missing result.

A revoked input lease does not erase a committed effect. During an authorized
closing drain, its real receipt can still be sent over the same connection.
After connection failure, service can still collect the late native result into
`last_reply`/`pending_receipt`, without forwarding it to another connection.
Evicted, unavailable, refused-before-ledger or panic-without-receipt outcomes
remain explicit local failures; they are never fabricated as zero-effect success.
Local authority challenges and responses use the existing canonical owner but
are not accepted as arbitrary peer-originated native-input commands.

## Closure is not native rollback

Every failed or abandoned bridge I/O turn revokes native authority before closing
its connection. The drive guard is constructed synchronously: even dropping an
unpolled drive future fences input. A synchronous handler panic also unwinds
through that guard. The native owner still reports and releases its actual held
state; a transport failure does not itself certify cleanup.

Authenticated peer FIN/RESET is checked directly from native stream metadata,
without waiting for the input consumer to read another byte. This matters when
platform preparation, irreversible submission, or receipt delivery is blocked.
`receive_ended` is the immediate lifecycle boundary; `receive_finished` remains
the separate claim that framing consumed the stream through a clean FIN.
A FIN does not authorize execution of a buffered suffix before closing input.

A preparation that returns after the fence cannot submit its effect. An operation
already entered into a native API may still finish; its truthful receipt remains
submitted/partial/unknown as appropriate, and release-only cleanup follows. The
seat stays occupied until both core and native cleanup plus native destruction
finish. No thread cancellation or process-crash release guarantee is invented.

## Verification

Nine new tests in [input_quic.rs](crates/fr-native/tests/input_quic.rs) run actual
localhost UDP/TLS, the existing client codecs, the canonical native owner and
X11/XKB/XTest against private Xvfb servers. They cover modifier/drag/release and
returned receipts, buffered-pointer ordering, critical-send backpressure, late
receipts after cleanup, dropped unpolled I/O, connection replacement, revoked
connection admission, local challenges, foreign leases and malformed payloads.
The input grants and presentation evidence are explicit local fixtures, not live
Tailscale admission or optical/physical-display qualification.

Four tests in [input_quic_lifecycle.rs](crates/frd/tests/input_quic_lifecycle.rs)
use actual UDP/TLS and explicitly blocked test-only native calls. FIN and RESET
each run during preparation and irreversible submission. All four failed against
unchanged `4a220c6` at the pre-completion revocation assertion and passed with the
fix; no timeout or native-effect assertion was relaxed. All thirteen new tests
also passed three repeated runs with four test threads.

The wider direct pinned-compiler run passed 333 tests, zero failed/ignored:
57 core, 54 wire, 100 media contract/policy, 25 client, 13 transport, 49 daemon,
and 35 native input tests. Each selected source/test compiled under strict Clippy.
These checks rebuilt first-party code against retained matching Asupersync TLS
libraries, not a fresh full Cargo-workspace build. They cover the published bridge
and FIN/RESET changes on the earlier source baseline; the concurrently landed
negotiation codec is verified separately. Native media/FFmpeg suites, fr-lab and
root-owned tailnet fixtures are not part of that 333-test selection.

The initial bridge commit separately passed the complete committed-source
`./scripts/verify.sh fast` and docs lane in GitHub
[run 34348326338](https://github.com/Dicklesworthstone/franken_remote/actions/runs/34348326338).
That result is scoped to `4a220c6`; later combined-revision CI is a separate gate.

Reproduction on a provisioned pinned-toolchain checkout:

```sh
cargo test -p frd --test input_quic_lifecycle --locked -- --test-threads=4
cargo test -p fr-native --all-features --test input_quic --locked -- --test-threads=4
cargo clippy -p frd -p fr-transport -p fr-native --all-targets --all-features --locked -- -D warnings
./scripts/verify.sh fast
./scripts/verify.sh docs
```

The known pinned Asupersync receive-window enforcement defect documented in
[QUIC_INPUT.md](QUIC_INPUT.md) is not fixed by this bridge. Sustained transfers
past the initial native stream window still need that upstream correction and
cross-window qualification. Complete authenticated startup, live tailnet ingress,
OS lifecycle integration and process-death release remain separate open gates.
The related session-agent, input-pipeline and transport beads remain partially
implemented; their broader acceptance criteria are not closed by these tests.
