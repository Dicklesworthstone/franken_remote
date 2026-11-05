# Native desktop dispatch

`SessionAgent::native_incoming` pairs a cloneable
`source::desktop::dispatch::Incoming` with a single owned `Driver`. The driver
keeps the original local agent and eventual native desktop on the independent
OS-share task. Each authenticated native connection keeps only its own returned
`dispatch::Peer` future in its guarded application callback.

The first caller reserves one cold-start slot synchronously. Other requests
receive `Busy` during startup, not a second source factory or a waiting queue.
The original Host deadline includes the queued time, approval, discovery,
configuration and first media attachment. Observation consent and the positively
negotiated shared-media profile precede the native factory. The implementation
reuses `open_native_shared_desktop`; it does not create permission or input grants.

When startup succeeds, the driver retains the same source and viewer hub. Warm
connections enter that hub through its existing bounded admission path. Each
gets the original connection-scoped `HostService`, never a readmission or renewed
startup budget. The first peer's later departure cannot stop sibling viewers.
The single factory and local-event callback span the original source lifetime;
there is no automatic source replacement or replay after terminal failure.

## Host integration

Create the pair with the locally configured `SessionAgent`, a dedicated source
Cx, the existing viewer policy, a capture interval, and local entropy. Run
`driver.serve(factory, select, local_events)` on the independent OS-share task.
Inside each existing native Server/LinuxServer application callback, use:

```rust,ignore
incoming.serve_host(host, notify_local_approval)?.await
```

Keep that callback alive through the returned peer future's completion. It must
not return immediately after queuing a Host. The driver is not a listener: TLS,
protected TUN ingress, exact-endpoint membership and per-connection credentials
still belong to the existing native host owner. Dispatch accepts only an
already-admitted original Host for the selected OS-session generation.

Local events run before waiting, startup and streaming work, with a bounded
maintenance wake. The independent source Cx is checked throughout, including
while the first peer is waiting for approval or native preparation. A local
Stop, revocation, cancellation, panic or abandoned driver fences the queued
peer or active source and entire cohort before dropping subordinate futures.
Dropping a peer service fences that peer; during cold startup this prevents a
successful source handoff, while after startup siblings remain independent.

Retain the driver after termination and call `reap` with a separate cleanup Cx
and absolute deadline to observe original child exit. `None` means no desktop
was delivered to the driver, not that preparation could not have started a
child. The factory must retain `Launch::retain_cleanup`'s Retirement outside
startup for failed/abandoned preparation, as required by the original opener.
No cleanup call retries external effects or creates another native worker.

## Controlled shutdown and local failures

The controlled share separates the session's terminal result from the original
input owner's cleanup and terminal-report drain (plan §7.3). Its managed service
exposes `ManagedControlService::shutdown_started()`, a monotonic witness set
after the service revokes authority. `terminal_session()` separately retains the
actual session result once it arrives. Native input may end first, leaving
original network I/O pending while cancellation is returned; the shutdown
witness is already set, but the session result remains `None`. Neither witness
is a cleanup receipt. The caller must keep polling that same service until it
returns `ManagedControlReport`; it does not replace the service, renew a deadline,
or reacquire control.

The native driver checks local events before each active-session turn. Once the
managed service has begun shutdown, the driver stops those local checks and
continues the original I/O and cleanup drain. This prevents cancellation caused by
teardown from replacing an earlier transport or session failure with an unnamed
`LocalEvent`. An explicit local stop during an active session still fences input
before the next service poll and preserves the original lease stop reason.

Native local failures carry one fixed, content-free cause:

| Cause | Observed condition |
|---|---|
| `SourceLifetimeCancelled` | The original independent source context failed its cancellation checkpoint. |
| `DispatcherClosed` | The original dispatch route was closed. |
| `DispatcherPoisoned` | The dispatch-state mutex was poisoned. |
| `CallbackFailed` | The embedding's local event or monitor callback returned an error. |

The original source and route are checked before and after a successful local
callback; the first failure prevents later callback execution. If a local
failure occurs while the session is still active, `LocalFailure { cause, session }`
retains both that cause and any subsequent managed-session error. It remains
a host fault without inferring which concurrent failure caused the other.
Uncertain native cleanup still takes precedence as `InputCleanup`.

The added regressions cover cancellation, route closure, mutex poison, failed
callbacks, simultaneous local/session errors, native-stop fencing before pending
I/O returns, ordinary local stops and cleanup precedence. They exercise the
production local gate and controlled loop with a scripted managed service, and
the production managed service with an explicit pending drain fixture. They
require no native SDK or live network. Source-level
verification is separate from `fr-jlgm`'s loaded namespace acceptance and any
live-tailnet qualification: these fixtures cannot establish that the reported
capture-stall and files-plus-clipboard load failures are eliminated.

This is the cold/warm authenticated-Host dispatcher, not multi-client UDP
multiplexing (`frd run` composes it in `crates/frd/src/host_run.rs`). It does not qualify an installed
Tailscale, logind/locker, kernel-ingress or hardware-media path. The focused
regressions use real TLS/UDP and supervised child IPC with explicit permission,
capture/HEVC and decoder-acknowledgement fixtures.
