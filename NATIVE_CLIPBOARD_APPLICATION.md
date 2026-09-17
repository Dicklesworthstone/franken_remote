# Native application clipboard lifecycle

The public Linux `NativePublisher` and `NativeObserver` owners now accept
`configure_clipboard(Configuration)` before their control-capable service starts.
This connects [automatic running-session negotiation](NATIVE_CLIPBOARD_TRANSPORT.md)
to the native application, not just to manually driven `ControlledHost` and
`ControlledViewer` values. Existing observation-only APIs remain observation-only.

`frd::native_clipboard::Configuration::new(consent, timeout, factory, new_id)`
retains independent local clipboard permission and one native factory. The factory
must target the approved local interactive OS session and must not read text while
opening. The item-ID callback must supply qualified local randomness. Neither
callback runs on the UI/network thread or before an original input grant and
bilateral clipboard readiness. Configuration does not select protocol features,
provide OS permission, imply visibility, or request control. A missing clipboard
startup profile refuses without disturbing viewing. Each owner accepts one setup.

The host derives its clipboard attachment from the original selected display and
allocates the next public binding from the original connection's consumed ID
namespace, including retired IDs. Only the independent one-use ticket comes from
its existing qualified nonce source; unordered entropy cannot strand startup. The
viewer uses its original decoder-backed input owner and host-clock projection.
Normal native service turns begin negotiation and consume/spawn the one-use seed;
no application callback must manually call `offer_clipboard`, `expect_clipboard`,
or `take_clipboard_worker`. A factory failure does not cause an automatic retry.

The returned cloneable `Control` is content-free. It exposes `status`,
`set_enabled`, `stop`, and a bounded `take_received` result. Enabled is initially
true but cannot override missing consent, dead authority, or a terminal stop.
A stop or disabled switch before control is acquired exchanges declined readiness
metadata when control becomes available; it never opens a clipboard or strands
the peer's handshake. That one-use setup stays declined even after re-enabling.
The switch fences final OS publication immediately. Network retirement occurs in
the original service turn. A stopped/closed session cannot be re-enabled through
an old handle. Clipboard lane retirement between I/O turns preserves input and
viewing; authority loss or worker failure during active native QUIC I/O retains
its existing conservative connection-wide fence. Incomplete attachment cancellation
is also connection-wide. The API does not promise isolation for these cases.

`Cleanup::Pending` is distinct from stopped admission. Closing a service fences
its native worker but does not block on a foreign call or claim the thread ended.
`reap_clipboard(cleanup_context, original_deadline)` joins only an already finished
thread. A timeout retains the original task for another cleanup call; it cannot
kill a hung native thread. A native factory returning after cancellation is
closed before it can read/publish text. Media and input cleanup remain separate.
`collect_clipboard` transfers at most one retained terminal result per call.
Normal service retains one UI result and backpressures the next uncollected
native result. After stop it also retains that final in-flight result in a second
fixed UI slot. Reap collects it before the native owner can be discarded; both
results remain available through the old content-free handle after reconnect.
An uncollected result is never overwritten. No clipboard contents are retained.

Eleven public-bootstrap regressions exercise actual UDP/TLS connections, supervised
media child fixtures, original input owners and native clipboard threads. They
cover both transfer directions (empty/Unicode), no echoes, independent refusal,
continued input after between-turn optional shutdown and pre-control disable/stop,
missing-profile refusal,
unpolled-service cancellation, failed/panicked factories and bounded cleanup of
a blocked factory. Clipboard contents, recorded HEVC, native injection, permission,
and compositor visibility in those tests are explicitly fixtures. This is native
shell API integration, not a rendered GUI, hardware proof or live-tailnet claim.

## An unconfigured clipboard is a refusal, not a disconnect

Selecting the clipboard startup profile advertises protocol support, not local
permission or the existence of a native factory. At the start of a new native
control-acquisition service, an endpoint with no clipboard configuration installs
a metadata-only declining participant. It exchanges readiness only after the
original control grant exists. There is no native factory or item-ID callback
in this participant, so neither side can read or publish clipboard contents.
Either or both applications may omit configuration and still use keyboard, mouse
and viewing. The original setup timeout and one-use attachment rules still apply;
this is not an implicit retry, permission default, or replacement channel.

Explicit configurations are preserved. Pure observation services and already
controlled sessions with externally attached channels are not retroactively
configured or renegotiated. Three new public-bootstrap UDP/TLS regressions cover
missing host, missing viewer and both missing configurations, including continued
keyboard receipts, no native initialization and cleanup of the original workers.

## Reconnect adapter

`native_connection::reconnect::native_control_view_with_setup` extends the
source-compatible `native_control_view` adapter with one callback receiving the
attempt number and the original approved `NativeObserver`. Configure clipboard
and retain its UI handle there; the hook runs before interactive control serving,
not before authenticated viewing or as a control request. Every fresh attempt
re-enters local setup. There is no implicit reuse of consent, a native factory,
clipboard contents, sequence state or an old handle's enabled state.

The adapter closes input and native publication first, then attempts media and
clipboard cleanup under the same original cleanup deadline. It retains the old
observer when either cleanup fails, and cannot start another attempt until both
are complete. A failed setup callback also retains that owner for cleanup.
Cleanup completion can carry a native worker error: an observed failed thread is
finished, whereas a timed-out foreign call is not. New tests use two fresh actual
UDP/TLS connections with equal fixture IDs, proving both setup callbacks execute
and the earlier UI handle stays closed rather than governing the replacement.
