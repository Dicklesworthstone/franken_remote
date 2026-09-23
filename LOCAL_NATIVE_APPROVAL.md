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

The focused tests use real UDP/TLS, Host/Viewer negotiation, credential-checked
Unix HTTP, a private real D-Bus daemon, Xvfb, XCB and independent XTest/Xlib events.
Tailnet metadata, login1 service data and user actions are explicit fixtures.
They do not establish installed-Tailscale, real desktop-locker, hardware or
physical-display qualification. This adapter does not by itself wire `frd run`,
provide simultaneous QUIC connections or grant unattended hosting.
