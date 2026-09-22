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
host profile. This feature does not enable `frd run` or fabricate the separate
capture/input permissions that startup requires.

Primary API references: systemd's `org.freedesktop.login1`, `sd_bus_get_name_creds`,
`sd_bus_get_owner_creds`, and `sd_bus_add_match` documentation, and the v257
`src/systemd/sd-bus.h` declarations.
