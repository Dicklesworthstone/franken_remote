# Session closure after ordinary I/O ends

`ClosedReport` and `ClosedRegistration` retain one original terminal-report
consumer before observation service starts. `QuicRecords::arm_closed_report`
validates the original connection and reliable host control route, then installs
a bounded synchronous observation fence. On ordinary connection closure, that
fence runs before socket custody or retained application state is released.

This reuses the existing lease-revocation registration and terminal drain. A
connection can arm only one consumer of either kind. It has no second queue or
post-fence application write permission. Unstaged records are discarded; native
payload/retransmission backlog, partial records, and queued datagrams still
refuse reporting. Returned cooperative I/O cancellation can transfer custody;
abandoning an in-flight I/O future cannot recover its socket.

The original owner calls `ClosedReport::finish(report)` after its teardown to
supply its actual typed reason, cleanup stage, and effect accounting. The private
preparation placeholder cannot be transmitted: only this unique consumer can
make the drain runnable, and it first replaces that placeholder. Missing or
uncertain native receipts must remain Unknown/Unconfirmed. Encoding uses the
captured binding, never a replacement session. No report is fabricated if the
consumer or socket is abandoned or if no registration occurred.

The 250-ms deadline starts at connection closure, not at finish or its first
poll. Time spent cleaning up consumes that budget. The original terminal security
gate and independently provisioned cleanup context remain authoritative; the
session is never un-cancelled. Transport acknowledgement is not peer processing,
confirmed native cleanup, or rollback of committed external effects.

## Executed verification

All 32 terminal tests pass: eight new deferred-Closed cases plus all 24 unchanged
immediate/deferred LeaseRevoked and immediate Closed regressions. They run real
TLS/UDP, native socket custody, framing, and deadline behavior. Fences and final
cleanup/effect summaries are explicit local fixtures, not native-input or hardware
evidence. Strict pedantic Clippy passes for the full production transport library
and terminal test target; changed-file formatting and whitespace checks pass.

The check uses source-rebuilt first-party libraries on checksum-verified b623c55
source with the exact main terminal drain from 83f8206, pinned nightly-2026-08-31,
and unchanged compiler/lock-matched external libraries retained from the b623c55
CI artifact. No cold dependency build, full current workspace, hardware, or live
Tailscale qualification is claimed. This first slice supplies transport custody;
automatic observation-session orchestration and native-owner cleanup accounting
remain separate integration. Ref: fr-rc-protocol-refusal-closure-5dx.

## Automatic protected observation sessions

The original protected-listener owner now provisions a Closed consumer before
application startup, using the independent broker context already retained by
`bind_linux`. `Host` arms it only after real binding admission and an observation
role selection. Control-intent sessions leave that slot unarmed for their actual
lease reporter. No report is fabricated before an observation grant. The raw
`Server::new` listener path without that independently provisioned broker clock
keeps its old immediate-close behavior rather than resetting a cancelled context.

The existing observation-renewal parser records a client-requested cause only
when the exact original CloseRequest successfully validates. A malformed close
records ProtocolError; FIN and cancellation cannot masquerade as a request.
Ordinary authority fencing and receive-batch termination happen immediately.
After the original scoped application ends, the listener finalizes its captured
socket outside that application's cancelled context. Existing immutable terminal
credential/endpoint/ingress/policy checks still apply. The callback's original
result is preserved; `Server::observation_closure` and the forwarding LinuxServer
query retain report delivery separately. No user callback or ordinary media I/O
is run during this bounded terminal attempt.

The automatic session owner always emits Unconfirmed cleanup and Unknown effects.
It cannot infer another task's native-release result or action receipt counts.
Absent an exact close-request cause, the listener reports its own stop, permission
failure, or host failure; it does not guess a lease-expiry cause. A competing
explicit terminal report cannot replace the first registered consumer. Reporting
is best effort: native backlog, lost credentials, abandoned I/O, dropped owners,
and expired cleanup budgets still prevent delivery. A parent which discards an
independently hosted application before it returns may have no captured socket;
this is not a promise that every desktop shutdown produces a Closed record.

Five additional tests exercise actual protected-listener ownership, TLS/UDP,
LocalAPI credential checks and the ordinary Host/Viewer startup and renewal paths
inside a disposable namespace. They cover automatic CloseRequest replies, normal
observation shutdown, malformed requests, credential loss after capture, and
control-intent exclusion. All five pass. The interface/firewall metadata, approval
and identity are explicit fixtures, not real ingress or installed-Tailscale proof.
Report collection keeps the original viewer transport driven to ACK the record;
this does not claim a new shipped CLI close-handshake or native cleanup test.

The final selected scope passes 62 unique runtime tests: 32 terminal transport,
5 automatic-closure, 10 unchanged native-acceptance and 15 unchanged serial-host
regressions. Strict production frd/transport Clippy and both new test targets pass.
The complete daemon test-source metadata/Clippy attempt timed out without a result
(first at 45 s, then at 120 s); no complete daemon or workspace test pass is claimed.
Existing serial tests were rerun to completion after an aggregate command timeout;
no assertion or test budget was weakened. All first-party libraries in the native
scope were rebuilt with the pinned compiler and unchanged matched CI dependencies.
The executed source is b623c55 plus these slices and exact current paired listener
guards. Publication payloads preserve the newer main exports, closure variant and
managed-revocation fields by preimage hashes, not a full-current-main rebuild.

Confirmed native cleanup, cleanup-owner-derived effect counts, a complete active
client CloseRequest/Closed handshake, and guaranteed reporting across every shared
hub/global cancellation path remain open. No release gate or bead is closed here.
