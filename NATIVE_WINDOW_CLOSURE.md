# Native observation-window closure

The ordinary `desktop::reconnect::Session` observation mode now uses
`Desktop::serve_until`. After the original observer is fully configured, and only
when its actually negotiated role is observation-only, the existing native
window is armed for bounded orderly WM close. A `WM_DELETE_WINDOW` event marks
`Status::CloseRequested` and immediately refuses new target/input-window borrows.
The same drawable remains owned while the original observer stops decode and
exchanges its typed close request on its existing connection.

Before another UI callback, the normal observation loop notices that exact local
request and enters the existing `NativeObserver::serve_until` closure path. It
retires the receiver, decoder and local audio before terminal network work. It
creates no new socket, parser, decoder, runtime or permission. The original
250-millisecond exchange and earlier session deadlines still apply. Request ACK,
optional exact host report, transport outcome and local cleanup are distinct.

## Emergency stop stays immediate

The raw window's default behavior is unchanged. Bootstrap and control-capable
sessions never arm this policy. Direct `WindowControl::stop`, emergency input,
local cancellation, signals, resize, unmap, destroy, event flooding and native
failure still stop the original session immediately. They cannot become an
orderly user request or gain a post-cancellation permission.

The first observed WM request also starts an independent, fixed 500-millisecond
native fallback. A stalled handler cannot leave the session/window running;
expiration fences it as `CloseExpired`. Repeated WM requests do not renew that
deadline. Native event handling remains count-bounded. This fallback is not a
lease extension or an additional transport retry. Its timer runs on the existing
window thread; no additional thread or signal handler is introduced.

A request is noticed at the next completed observation-network turn, not at an
invented instant of physical user intent. A native negative event arriving first
still wins. Existing outstanding target references remain subject to the original
session/receiver checks; `CloseRequested` is not a claim to reclaim borrowed native
resources. Unsupported or already-retired recovery scopes refuse rather than
reviving a socket.

## Retained result and original cleanup

`Desktop::disconnect_outcome()` and `desktop::reconnect::Session::disconnect_outcome()`
retain the exact completed exchange. Native cleanup still runs on the original
supervisor's independent bounded cleanup context. Its decoder and window owners
must actually finish before the desktop is removed or another attempt can start.
Cleanup preserves the report; only a new attempt clears the previous attempt's
result. A normal close does not trigger an automatic reconnect, including when
the host report is absent. Missing reports remain unknown, never zero pending
effects or confirmed remote cleanup.

## Executed verification

Seven new tests plus all existing cases in the selected complete targets passed:
33 native library tests with cli/desktop/viewer-input/window features and 39
viewer-window integration tests, 72 unique tests with none ignored. New tests
exercise real XCB windows and independent WM messages, exact TLS/UDP request and
report transport, continued original ownership through native reap, known/unknown
remote effects, a withheld report, fallback expiry and repeated requests, direct
stop, negative native lifecycle and owner drop. Existing input capture tests
exercise emergency/held-state behavior under Xvfb. Source/decoder output uses the
existing explicit supervised IPC fixture: not real HEVC or physical visibility.

The complete affected frd/native production libraries, native library test target
and full viewer-window target passed strict pedantic Clippy. Formatting and patch
whitespace checks passed. Nine first-party libraries were rebuilt from the verified
63caea4 source archive using pinned nightly-2026-08-31 and matching unchanged
external CI libraries from run 36291188001. This is not a cold dependency build,
complete all-feature/workspace run, installed-Tailscale or hardware qualification.
The window C boundary was rebuilt with -Wall -Wextra -Werror; the unchanged native
input/cursor object archive came from that same verified CI source because the
local xcb/render.h development header is absent.

Initial native input test failures came from an invalid ambient DISPLAY. Running
the unchanged full target on its own Xvfb display passed all 33 cases. Initial
new transport tests accidentally used the pre-admission bootstrap routes; the
fixture now transfers its actually installed routes rather than fabricating
bindings. Callback assertions begin at observed native intent, not before an
asynchronous X event is handled. No production deadline or old assertion changed.

This connects normal native-window observation close, not controller shutdown,
confirmed host cleanup or authoritative external-effect accounting.

## CLI outcome retention

The CLI now projects the original Session's retained outcome into `close_exchange`
on observation completion. No exchange and no host report are separate nullable
states. The report retains every typed reason, cleanup stage, and known/unknown
outstanding-effect count; neither request ACK nor local cleanup manufactures
remote cleanup. Failed transport after receipt does not discard the report. A
local cleanup error or signal also preserves a collected outcome on the failure
record without changing its error or exit status. Existing controller reporting
and generic failures without closing evidence remain unchanged.

Six added CLI tests cover all reason/stage/count combinations, report absence,
failed ACK flush, local failure preservation and actual completion rendering.
The complete CLI unit binary passed 58 tests. The existing 14 CLI process tests
also passed against the newly linked executable: actual stdout/exit, signal,
root-credential Unix LocalAPI, trust-store and typed refusal paths. With the 72
native tests above, this session has 144 distinct passing cases, including 13
new tests. Repeated runs are not counted twice. CLI production and complete unit
target strict pedantic Clippy passed; formatting and whitespace checks passed.

The first process run could not launch because the local metadata-only lint
command had reused the executable output path. Separate metadata/binary outputs
and an unchanged source rebuild made all 14 cases pass. The initial CLI lint
finding was corrected by renaming a private field, not suppressing the lint.
These process checks do not claim a newly executed full CLI-to-live-host window
session or audio/clipboard feature matrix. The native closure integration tests
above exercise the actual application/window/session path with synthetic codec
and peer-accounting fixtures. Remaining full-workspace/native-host cleanup and
live-tailnet qualifications are unchanged.

Refs: plan 7.3/19; PROTOCOL.md 4/6; fr-rc-protocol-refusal-closure-5dx remains open.
