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

This is the cold/warm authenticated-Host dispatcher, not multi-client UDP
multiplexing (`frd run` composes it in `crates/frd/src/host_run.rs`). It does not qualify an installed
Tailscale, logind/locker, kernel-ingress or hardware-media path. The focused
regressions use real TLS/UDP and supervised child IPC with explicit permission,
capture/HEVC and decoder-acknowledgement fixtures.
