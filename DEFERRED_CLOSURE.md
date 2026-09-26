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
