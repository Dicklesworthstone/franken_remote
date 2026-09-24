# First native desktop: independent source handoff

The first native connection must stay alive after preparing `NativeDesktop`, but
must not become the lifetime owner of the shared capture source. Returning a
desktop or hub ticket immediately from `Server::run_on_protected_listener` or
`LinuxServer::run` ends the original connection scope. Running the whole desktop
inside that callback instead couples every sibling viewer to the first peer.

`NativeDesktop::handoff` closes that ownership gap. It transfers the original
`SessionAgent` and desktop through one bounded, nonblocking, local callback and
returns a `HostService` for only the first viewer. Await that service inside the
native connection callback. The receiving OS-share owner independently drives
`NativeDesktop::serve`, feeds actual platform permission/lifecycle events, and
retains the desktop for `NativeDesktop::reap` after service ends.

The handoff rechecks the original source and local-agent registration. It does
not clone the capture process, readmit a viewer, create a task or transport,
refresh a deadline, infer local permission, or grant input control. The existing
native transport retains its credential/ingress checks. `Ticket::into_service`
follows an already-admitted ticket in its original clock domain; late incoming
hosts still use `Admission::serve_host` and the same service implementation.

A successful publication transfers source lifetime responsibility to the local
receiver. Dropping the first viewer's `HostService`, including before polling it,
fences only that viewer; the source and independently admitted siblings remain.
A refused or panicking publication fences the source and its entire cohort even
when the callback already stored them or admitted another pending peer. A
reentrant source revocation cannot be converted into successful publication.
Completed peer receipts remain terminal and cannot be reset by wrapping the
same ticket in another service.

Shutdown remains two steps: fence observation/capture, then observe the original
child's exit with `reap` using an independent cleanup context and absolute
`Deadline`. A timed-out or abandoned reap retains the original owner; it does
not manufacture successful cleanup or start a replacement child. Retain the
launch's existing `Retirement` handle through first-source preparation failures.

## Evidence and limits

Four new regressions cover handoff and sibling survival past the first peer's
departure, callback refusal/panic/reentrant revocation after ownership escapes,
unpolled connection-service abandonment, and original pending-peer expiry with
stable terminal receipts. They exercise real TLS/UDP and supervised process IPC;
OS permission, capture, HEVC payload and decoder acknowledgements are fixtures.
This is not installed-Tailscale, kernel ingress, native codec or hardware proof.
The focused run passed all four new tests and 19 existing related tests using
rebuilt first-party source, the pinned compiler and compiler-matched retained
third-party libraries. Full current-main workspace qualification is separate.

This handoff builds on the first-source implementation at `bf9151d7`; it does not
replace that implementation, implement multi-client UDP demultiplexing, or
change the `frd run` composition in `crates/frd/src/host_run.rs`.
