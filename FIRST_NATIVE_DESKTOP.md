# First observer to a native shared desktop

`SessionAgent::open_native_shared_desktop` joins initial application negotiation,
local observation approval, native source preparation and first-viewer display/
media attachment. It consumes an already protected TLS/tailnet-admitted `Host`.
The local source factory is invoked once, only after the original host's actual
approval and binding acknowledgement. No capture process is kept alive while
approval waits. Control requests are refused before the approval notification;
this path is observation-only, not a silent control downgrade.

The original Host deadline covers every phase, including factory time. The native
preparation and unused-publication bounds remain in force. During preparation the
same HostSession drives network/admission/observation renewal. On native success,
the pending network turn is drained, not dropped; local permission/event handling
continues while it drains. The exact selected child, first IDR, physical pool,
local registration and peer move into the returned `NativeDesktop`.

Drive the desktop on an independent OS-source task. Use its `parts()` with the
existing `SessionAgent::serve_shared_desktop`, its `admissions()` for later peers,
and `first()` for the original viewer's cancellation/status ticket. Do not return
this desktop from a native connection-scoped callback whose completion cancels
the first peer. The caller still supplies actual local OS events/permission probes,
a locally authorized source factory, a protected package and a local selection.
Keep `Launch::retain_cleanup`'s Retirement outside the operation for interrupted
preparation; dropping authority does not prove the native child has exited.

Preflight refusal, cancellation, deadline expiry, local Stop and caught callback
panics fence owned authority before dropping dependent work. A source already
registered to another local agent is refused without revoking that owner's source.
No additional transport, runtime, input grant, retry or media queue is created.

Ten new regressions and 38 existing hub tests pass using actual TLS/UDP and
supervised child-process IPC. They cover approval waits longer than the unused
source budget without starting capture, subsequent streaming on the same child,
role/permission/session refusal, unpolled abandonment, original-deadline expiry,
foreign-source isolation, local revoke during stalled capture and retained futures
after a selector panic. Ten existing namespace-only tests are skipped in that run,
not counted as passes. Production frd passes strict pedantic Clippy. Builds use
the pinned compiler, rebuilt first-party source and matching retained CI dependency
libraries. OS identity/permission and codec/decoder responses remain explicit
fixtures: this is not hardware, installed-tailnet or full-workspace qualification.

The actual `frd run` dispatcher and native platform permission/UI adapters still
need integration. This API does not bind a listener, assert kernel ingress, or
make the unfinished command-line daemon a complete workstation. The source-size
gate is unchanged and remains failing; no release or broad bead closure is claimed.

## Continuous native service

`SessionAgent::run_native_shared_desktop` performs that opening and immediately
runs the original source/hub through `serve_shared_desktop`. One local-event
callback and entropy supplier span both phases, so callers need not move owners
between loops or restart native work. Run it on the independent OS-source task:
after startup, ending the first peer does not cancel a surviving admitted peer.

The one-shot local `announce(Admission, Ticket)` callback exposes the original hub
only after first media attachment, before decoder completion. It is not a usable-
view notification. The shared service's existing guard is installed beforehand;
callback error, panic, revocation, unpolled abandonment and terminal results fence
original authority before dependent futures are dropped. Even a reentrant join
created by announce is fenced. No policy lock is held while invoking callbacks.
The source Retirement remains the explicit cleanup receipt for observing child
exit; callers must retain it, including when opening fails.

Five additional continuous-service regressions pass, including changing-frame
TLS/UDP delivery to a late viewer after the first leaves, source/peer renewal,
callback-state continuity, preflight cadence refusal, parked expiry, reentrant
announcement faults and permission loss during streaming. The static-screen IPC
fixture keeps its original behavior; an explicit test-only changing-screen mode
lets the new regression require multiple delivered frames rather than merely a
live connection. All codec and OS facts remain synthetic fixtures.
