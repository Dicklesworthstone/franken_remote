# Linux session lifetime evidence

The opt-in `fr-native/linux-logind` feature observes one explicitly selected
local X11 session through the installed system bus and `org.freedesktop.login1`.
`logind::Watch::start` returns a native cleanup owner; its cloneable `Control`
reports `Opening`, fresh `Active` evidence, or a terminal, content-free cause.
It does not grant screen-capture permission, local observation approval or input.

## Identity and event ordering

The production bus address is fixed to `/run/dbus/system_bus_socket`. Environment
bus/session/display variables are not consulted. Both the bus endpoint and the
unique login1 service owner must supply root effective-UID evidence without
race-prone credential augmentation. All queries target that unique owner.

A local selection supplies the exact session ID, UID, seat and display. A positive
snapshot requires every relevant property, an active local `x11`/`user` session,
an unlocked hint and positive `CanLock`. The creation timestamp and leader are
retained with the original identity. Missing properties, changed/reused session
identity, a different service owner and unsupported sessions fail closed.

Signal subscriptions precede the first snapshot. Lock, critical session-property
changes/invalidations, session removal, preparation for suspend/shutdown, or loss
of the service owner latch a terminal result. An unlock or later positive reply
cannot erase a lock that arrived during a snapshot. Unrelated sessions and other
bus clients cannot supply events for this selected unique service owner. A change
to even the same critical value is conservatively terminal; idle-hint updates do
not renew or change application authority.

## Bounds and cleanup

All foreign D-Bus calls execute on one native thread, never on the authority
thread or under an authority mutex. Each method has a 200 ms timeout. Positive
snapshots are refreshed at 100 ms intervals and expire 500 ms from query START,
not reply arrival. The initial acquisition limit is two seconds. Freshness uses
Linux `CLOCK_BOOTTIME`, including suspension, rather than wall time. These are
configured limits, not measured scheduling or screen-lock latency guarantees.

The consumer must check `Control::status` on its bounded local maintenance path
and before renewing authority. `register` supplies a coalescing native-event wake;
it does not replace maintenance while the native thread is stalled. There is one
process-wide native-worker permit, held through actual thread exit even after
Drop, so failed or stuck probes cannot accumulate unlimited replacement threads.

Signal processing is bounded to 32 events per drain and property inspection to
64 entries, with bounded string copies. These bounds cover FrankenRemote-owned
work. The installed root bus/service and libsystemd are trusted native boundaries;
this is not a sandbox or a limit on libsystemd's internal D-Bus message allocation.
No peer-originated protocol/media messages enter this connection.

Stop is terminal and nonblocking. Keep `Watch` until `try_finish` confirms native
thread exit; a stopped status alone is not cleanup completion. No automatic
reconnect, unlock/resume reauthorization, or replacement-session selection occurs.

## Qualification and integration boundary

The ABI declarations are checked against systemd v257's `sd-bus.h`; linking uses
the installed `libsystemd.so.0` SONAME without a download or new Rust dependency.
The real-bus tests use installed libsystemd and a private `dbus-daemon`, with an
explicitly synthetic login1 service. They do not modify the installed system bus
or exercise an actual desktop locker, logind daemon, Tailscale, or codec.

`LockedHint` is maintained by the desktop, not independent physical lock proof.
A desktop whose locker does not faithfully maintain it is NOT qualified by this
adapter. The selected user, desktop and OS remain trust boundaries. Actual
per-desktop lock/logout/suspend qualification is still required before enabling a
host profile. This feature is not used by `frd run` yet and does not fabricate the separate
capture/input permissions that startup requires.

Primary API references: systemd's `org.freedesktop.login1`, `sd_bus_get_name_creds`,
`sd_bus_get_owner_creds`, and `sd_bus_add_match` documentation, and the v257
`src/systemd/sd-bus.h` declarations.

## Original local agent and final input integration

The optional `linux-session-events` feature exposes `logind::agent::Events`.
Construct it with fresh Control evidence, the exact selected `SessionAgent`, and
that source owner's existing cancellation/clock context. Its borrowing `callback`
fits the local-event callback of the existing native desktop open/run/serve loops.
Keep Events outside the running future so `take_cleanup` remains available after
termination; keep Watch independently until actual native-thread exit is observed.

Positive evidence only returns Continue. It never marks capture/input permission
granted, approves a viewer, unlocks a session, grants control or replaces an OS
session. Negative evidence calls the original agent's immediate revocation path,
including registered source/input revokers, and fences only the supplied source
context. An opaque weak `AgentIdentity` prevents retargeting even when numeric
OS-session IDs are reused. Missing evidence is not mislabeled as physical lock.

The adapter retains one release batch and the original pending OS-cleanup receipt;
repeated polls cannot silently discard or replace them. The existing held-state
tracker now reports `PendingCleanup` and retains held obligations when it merely
generates releases. Only confirmed release submissions clear those obligations.
Input driver shutdown, unresolved/unknown releases, and native thread exit remain
separate observations. A revoked flag is never proof of cleanup completion.

With `linux-input-agent` and `linux-logind`, `start_x11_guarded` composes the same
canonical InputSession/Seat/Driver with the selected local-session evidence. It
requires the watch's explicit display and UID to match the input process. The
original native factory still independently checks X11 geometry/capabilities.
The gate checks evidence around preparation and immediately before each native
submission. The existing InputSession still checks all leases, tickets, sequence
and geometry requirements; logind is not a substitute for those checks.

When evidence ends, revoke first. Canonical InputSession cleanup may then submit
only releases for that original owner's held keys/buttons/wheel; normal remote
input, INCLUDING remote releases, still fails its final authority check. A native
call already entered keeps its actual Submitted/Unknown result; no automatic
application retry or retroactive cancellation claim is introduced.

GuardedDriver polls evidence on the original input watchdog's bounded cadence,
with a native-event wakeup, so idle input cannot retain authority until another
packet arrives. No second runtime, watchdog queue or input sink is introduced.
Its Shutdown result comes from the original native cleanup owner. Unpolled Drop
still requests canonical abandonment; it does not assert synchronous native reap.

Additional tests use a private real Xvfb server and an independent Xlib connection
to observe held Shift/button state, not only a submission receipt. Synthetic
logind lock or lost replies cause actual release without another input packet;
unpolled driver abandonment also releases/reaps the same original native owner.
This does not qualify an installed desktop locker or enable the `frd run` CLI.
