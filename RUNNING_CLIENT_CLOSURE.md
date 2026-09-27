# Orderly observation shutdown through the running viewer

`StreamingViewer::disconnect(reason)` and `NativeObserver::disconnect(reason)`
retire the original receiving and native-decoder scope before preparing the
existing `ViewerSession` close exchange. The decoder is aborted, queued pictures,
repair offers and initial receipts are retired, and local audio output is dropped
without flushing samples. External owners of compressed pictures keep their
memory charges until they release the buffers. The original decoder owner stays
available for `reap_media`; neither abort nor a host report proves child exit.

The connection is retained only by the existing typed closing exchange. Ordinary
I/O, renewal responses and clock service stop at call time. The original 250-ms
exchange budget and earlier silence/response deadlines are unchanged. Original
security and cancellation handles remain active. There is no replacement
connection, receiver, decoder, packet queue, authority renewal or effect replay.
The control-session implementation is shared, not copied into a second protocol.

`serve_until` runs the ordinary observation service with a bounded callback
returning `ControlFlow<fr_wire::closure::Reason>`. Continue leaves service running;
Break requests orderly shutdown at the next existing completed network-turn
boundary. It also works while native decoding is pending: the pending operation
is fenced/aborted before the close exchange, never awaited into a late presentation
receipt. No more UI/application callbacks occur during closing. A callback error,
emergency stop, network failure or abandoned future remains an ordinary terminal
failure, not invented client intent. During recovery an existing scope guard may
already have retired the connection; the exchange refuses rather than reviving it.

A control-capable or control-intent session cannot use this observation-only API.
Its original input fence runs first and its existing cleanup/reporting obligations
remain with the controller owner. These methods do not change the native window's
existing immediate emergency-stop behavior. Native window/CLI opt-in remains a
separate integration; this slice supplies the actual running-loop transition.

The exact completed exchange is retained by `disconnect_outcome()` after local
teardown and native reap. The optional host report also remains on the original
ViewerSession. Request acknowledgement, report contents, transport failure and
local child exit remain separate. A missing or unconfirmed report never becomes
zero outstanding effects or successful native cleanup; a rejected second close
cannot erase a previous result. Unpolled abandonment has no completed outcome.

Local audio retirement now drops its output at session close, rather than keeping
an output alive until the entire streaming object is dropped after native cleanup.
Implementations must fence further output/decode synchronously on Drop without
blocking for native completion. No host AudioStop, playback error, remote cleanup
or audibility claim is fabricated. Separate output/process retirement ownership
remains with the local native implementation.

## Executed verification

Seven new tests and 31 existing regressions passed (38 unique cases): six new
TLS/UDP/supervised-decoder cases, one new recording-output lifetime case, five
unchanged audio-gate tests, and 26 existing viewer repair, recovery, renewal,
backpressure and Closed-report cases. The new cases cover call-time fencing,
external picture ownership, exact report retention through reap and repeated
close, UI-requested shutdown after decode and during a pending decode, unpolled
abandonment, emergency-stop/callback-failure distinction and control-intent refusal.

The tests use production connection/session, packetizer, receiver and supervised
IPC paths with explicit identity, compressed media, decode and host-accounting
fixtures. They do not qualify HEVC/GPU, native output devices, physical visibility,
input release or installed Tailscale. Test bootstrap seeds production receivers
with synthetic packetizer output; terminal requests/reports cross actual TLS/UDP.

All eight first-party libraries rebuilt with pinned nightly-2026-08-31 from the
checksum-verified 9d0e3966 archive and these changes, against unchanged matching
external metadata and object libraries retained by CI run 36270681484. Production
daemon strict pedantic Clippy and changed-file formatting pass. The full daemon
test-source metadata checks exceeded the local execution limit; a broad selected
binary also exceeded the memory limit during parallel code generation. Two smaller
batches with serialized compiler backends passed. Only test registration was
excluded in a separate source copy; production and selected assertions/deadlines
were unchanged. No cold dependency or complete latest-workspace pass is claimed.
Concurrent native Opus and clipboard attachment changes are preserved, not counted
as newly executed coverage. The broader closure qualification bead remains open.

Refs: plan 7.3/19; PROTOCOL.md 4/6; fr-rc-protocol-refusal-closure-5dx.
