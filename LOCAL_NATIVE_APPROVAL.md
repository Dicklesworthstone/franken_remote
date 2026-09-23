# Local approval for the original native session

The opt-in `fr-native` feature `linux-local-approval` supplies a native X11
approval window for the one-use `frd::session_startup::Approval` delivered by
`Host::open` and the existing desktop incoming/startup callbacks. It reuses the
sharing indicator's XCB boundary, without putting GUI dependencies in `frd` or
adding a network approval API, input hook, runtime, or capture/input permission.

Start `logind::approval::Prompt` with the original approval and the `Control` of
one explicitly selected local logind session. The selected UID must match the
process effective UID; the selected X11 display, not ambient `DISPLAY`, chooses
the window's destination. Positive session evidence is checked before opening
and immediately before the final decision. Role labels come from the immutable
negotiated role retained by `Approval`, never from a remote display string or a
caller-provided notification label. Approval still consumes its original startup
deadline and atomic one-use decision; mapping does not grant consent.

The selected local user must complete a primary-button press/release inside the
Allow button after the window is mapped/drawn. Deny, Escape, Enter, Space, window
close/hide, loss of fresh session evidence, expiry, cancellation and abandonment
refuse the original request. XSendEvent cannot produce a positive decision.
XTest and other same-user X11 clients remain inside the existing trusted local
X-server/user boundary; this is not a secure-attention or hostile-desktop defense.
The window uses fixed viewing/control labels and includes no untrusted peer text.
Mapped/drawn status is not proof of physical visibility or that a person saw it.

`PromptControl` exposes status and cancellation, deliberately no allow method.
Keep `Prompt` until `try_finish` confirms its native thread/window have ended.
A decision result is distinct from cleanup completion. Drop denies pending
consent without blocking on XCB; a stuck retiring native thread retains the
process-wide single-window permit instead of allowing unbounded replacements.
After a successful one-use decision, closing its UI cannot undo that decision:
the original session's ongoing policy, logind, input and source guards remain
responsible for active-session revocation.

## Owning the existing notification callback

`logind::approval::ApprovalUi::new(control, &agent)` binds one reusable notification
slot to the exact original `SessionAgent`. Construct it before moving that agent
into its independent native desktop `Driver`; pass `ui.callback()` to `Host::open`,
`Incoming::serve_host`, or `Driver::serve_on_linux`. The closure satisfies the
existing `Send + 'static` notification contract, but retains only a weak UI handle.
Keep the UI owner, original logind Watch/Events, and sharing indicator separately;
notification success still means only that a prompt was started.

The original approval now exposes its immutable local `ControlBinding` as well
as its negotiated role. The adapter rejects a mismatched notification role or
OS-session binding before opening a window. Its opaque weak agent identity and
original-agent revoke registration prevent an equal-numbered replacement agent
from inheriting pending consent. Original-agent loss or revocation denies pending
and future requests without revoking a different replacement agent. Dropping the
UI makes its escaped callbacks refuse, rather than keeping a native owner alive.

There is one slot and no queue. While a prompt is pending **or its native cleanup
receipt remains uncollected**, another request is denied instead of reusing the
old decision. Call `ui.collect()` on the local event/maintenance path; it never
waits for a running foreign call and returns the original retirement outcome once.
Keep that outcome in the caller's bounded status/receipt owner. A successful
collection frees the slot for a fresh request, not a resumed old request.
`ui.stop()` permanently denies pending/future requests while leaving native
cleanup collectable. It does not retroactively undo already committed consent;
active-session policy, source/input authority and logind event handling remain
mandatory independent services.

The focused tests use real UDP/TLS, Host/Viewer negotiation, credential-checked
Unix HTTP, a private real D-Bus daemon, Xvfb, XCB and independent XTest/Xlib events.
Tailnet metadata, login1 service data and user actions are explicit fixtures.
They do not establish installed-Tailscale, real desktop-locker, hardware or
physical-display qualification. This adapter does not by itself wire `frd run`,
provide simultaneous QUIC connections or grant unattended hosting.
