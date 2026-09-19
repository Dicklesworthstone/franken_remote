# Recovery reporting on the running QUIC session

The `reference-recovery` version 1 extension now has a connection-bound receiver
reporter and an automatic observation-loop integration. These changes do not
advertise the extension globally or claim complete desktop reconnection.

## Original connection, bounded record

`NegotiatedMedia::recovery_receiver` requires the actual completed configuration,
recovery and video attachments, positive capability selection, matching receiver
configuration, and the original reliable control pair. The reporter retains one
fixed-size record and its original receiver/generation proof. Call `service`
even in silence; feed actual successful decode receipts to `observe_decoded`.

A failed reference chain is fenced before its report enters transport. The exact
request bytes and failure deadline survive QUIC backpressure. A new poll cannot
renew the deadline or emit another request after admission. The final transport
callback checks authority and the original receiver again. `Requested` means
transport admission, not host receipt, successful recovery or input permission.

Wrong roles, absent negotiation, foreign connections, receiver replacement,
malformed media and authority loss refuse. A foreign connection is checked before
its queues or receiver can be touched. The parent session retains responsibility
for terminal closure and native cleanup.

## Automatic observation loop

`StreamingViewer` installs the reporter from the actual negotiated selection.
The canonical loop services it before selecting native decode work and around
network turns. Completed decode receipts, including a retained authentic initial
receipt, are recorded before presentation consumes them. Missing history remains
unknown; frame zero is not fabricated.

After recoverable reference loss, an observation-only peer sends its request on
the existing control lane. Its observation renewer and clock owner keep running.
Unsent obsolete repair bytes are cleared, and late old-generation media cannot
re-enter decoding. The loop reports no fresh view from failed pixels. Solicited
load feedback continues with actual queue measurements so a pending advisory
exchange cannot starve or expire during recovery. No decoder or transport queue
is added, and the entire wait retains the original absolute recovery deadline.

Input-owning, viewing-for-control and control-requesting peers retain their
existing terminal failure path. Native decoder failure/cancellation also retains
its existing worker containment. Neither network failure nor a recovery request
silently reacquires input, replays an action, or reuses a failed native worker.

## Remaining work

The host's automatic recovery-request dispatch, replacement-channel handshake,
receiver/presenter handoff onto newly admitted bindings, and safe control
reacquisition remain separate integration work under `fr-p1-loss-recovery-20s`.
The existing host-side recovery/input-fencing work is not changed by these
patches. A host application selecting this extension must still handle its
requests; these changes do not make an unsupported host advertise it.

## Verification scope

Nine new tests exercise six real TLS/UDP transport cases and three canonical
session-loop cases. The session test deliberately loses a reference, observes
exactly one recovery request, sends obsolete-generation traffic, and keeps the
same observation renewal and solicited feedback alive beyond the initial
three-second observation lifetime. Invalid media and a missing reporter retain
typed terminal refusal. Decoder completion is explicitly simulated; this is not
HEVC, hardware, public interoperability, or live-tailnet qualification.

The restored source was rebuilt on September 19 with nightly-2026-08-31 and
unchanged, compiler/lock-matched upstream libraries from CI run 35421807938.
All eight relevant first-party libraries were compiled from source. Six standalone
transport tests and all 283 runnable daemon library tests pass, with 16 tests
explicitly ignored. The complete daemon test binary was compiled without pruning
source or tests. Strict Clippy passes for the daemon library, its complete local
in-crate test source, and the standalone transport tests.

The source baseline for these local checks is 38f1b0c plus these saved patches;
existing modified-file base blobs were reconciled with newer main. Concurrent
native recovery policy, transport retirement and controller/file work is preserved,
but these results are not a complete combined-main or cold Cargo workspace pass.
The earlier execution-limited partial test run remains in the saved patch evidence;
the new full local test run supersedes that limitation for this restored source.

Cargo-generated lock metadata also includes the already-declared fr-lab media and
wire development dependencies. Those two missing lines blocked current CI before
compilation under `--locked`. No dependency version, checksum, feature, compiler
or runtime pin was changed to repair that metadata.
