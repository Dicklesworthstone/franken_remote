# Changelog

## 2026-11-18 — the Windows viewer runs above normal priority and names each reconnect

- On 11-18 another agent's three-job Rust build ran on the owner's 4-core
  laptop over SSH (it is an `rch` build worker). The UI thread stalled for
  up to 1.7 s, and the viewer reconnected about every 20 s
  (host: `PeerClosed(Client(Failure))`, `RecoveryLimitExceeded`).
  Putting that build at idle priority cut the drops from every ~20 s to
  minutes apart.
- The viewer now sets its process to above-normal priority at start. It
  uses little CPU itself (decode is on the GPU), so builds or a browser can
  no longer starve its input, renewal and clock deadlines.
- Each reconnect line now names how the replaced session ended (end code
  and detail). The completion document names only the window's last session.

## 2026-11-18 — the session log is written while a window is open

- The Windows app wrote each window's status lines to `sessions.log` only
  when the window closed. Now that a window reconnects instead of closing,
  that could be hours later. Lines are now appended as they arrive, each
  stamped with seconds since the window opened. The window's header
  (`=== session started at unix …: args`) comes first; its trailer
  (`=== session ended at unix …, exit N`) and completion document come last.
- The launcher drains the viewer's stderr on its own thread, so a full pipe
  can never block the viewer.

## 2026-11-18 — a Windows viewer never closes or loses control on its own

- Only the user (or `--seconds`) ends a viewer window now. When a session
  ends any other way, the window keeps its last picture and reconnects:
  - a host or network failure, or a report the host sent;
  - control lost mid-session, or an input push refused;
  - a presentation glitch, or a refused or expired replacement.
  Before, almost all of these closed the window. On 11-18 the owner's
  mac-studio sessions ended every 20-35 s
  (`LeaseRevoked { reason: SessionEnded }`, host `Renewal(Transport(Expired))`,
  `ReferenceRecovery(Transport(Backpressure))`).
- A reconnect waits 0.4 s after the old session finishes, doubling up to
  5 s while no picture arrives, with no limit on how many. A hidden window
  waits until it is shown. The 32-session limit per window is gone.
- Control comes back by itself. A `--control` session now keeps asking for
  control: on every internal reconnect attempt, when the app returns to the
  front, and in every replacement session. Before, an internal reconnect
  retired the one control request and nothing asked again, so the picture
  came back with the mouse and keyboard dead. Stop control is still
  respected until the user asks for control again.
- The shortcut hook (Windows key, Alt+Tab, Ctrl+Esc) runs on its own
  high-priority thread. Before, it ran on the window's UI thread, which can
  stall for hundreds of milliseconds drawing a large picture. Windows skips
  a low-level hook that answers late, and since Windows 7 removes it
  silently, so shortcuts could stop working mid-session. The thread also
  registers the hook again every 2 s.
- Viewer-side only; hosts need no rebuild.

## 2026-11-18 — a host's mid-session picture recovery no longer ends the session

- Since the first-view limit went to 6 s, every recovery a host admitted
  failed at once, and the session ended. The viewer saw
  `LeaseRevoked { reason: HostFailure }`, and the host logged
  `ReferenceRecovery(Expired)`. On mac-studio every session ended about 3 s
  in, at its first lost reference.
- The cause: the host's recovery deadline is now its 6 s sender horizon, but
  starting the replacement media channels still refused any deadline more
  than 5 s away. That cap is now the same 6 s first-view limit.
- A recovery test now runs with the shipped hosts' sender policy.
- Host-side: hosts need a rebuild.

## 2026-11-18 — a key queued during a preempted input turn no longer ends the session

- A key or click the UI thread queued while the input dispatcher was between
  reading its clock and taking the queue ended the session
  (`Control(Capture(Clock))`): the event looked stamped in the future. On a
  saturated CPU the dispatcher is often preempted there; on 11-18 two of the
  owner's Windows sessions ended this way, after 4 s and 40 s. It was not a
  clock-correlation fault: both stamps come from the viewer's own clock.
- Such an event is now fresh. Every push still refuses a stamp from its own
  future, and the event's 100 ms age limit still applies.
- The fr-client clock tests that still expected the exchange to end at the
  1 s bound now expect the previous fix's give-up behavior.
- No bound changed. Viewer-side only; hosts need no rebuild.

## 2026-11-18 — the host names why it refused an observation renewal

- One installed host's log shows 14 shares ending with
  `Renewal(Transport(Unauthorized))`, which can mean several things:
  - the observation authority lapsed because a challenge's round trip
    outlived its deadline (each answered challenge extends authority to its
    issue time plus 3 s, with one challenge in flight at a time);
  - the authority was revoked;
  - a session permission check failed;
  - the connection's own authority check failed.
- frd now prints which, once per share: `frd: observation renewal refused:
  <cause>`, beside the `frd: connection authority ended: <check>` line from
  the previous entry. Behavior is unchanged. Once the hosts are rebuilt, this
  decides whether the fix belongs in authority timing or elsewhere.

## 2026-11-18 — one slow clock probe no longer ends the session

- The viewer measures the host clock with a probe every second on the
  control stream, which it shares with the video. A reply later than the
  1 s exchange bound stopped the exchange and ended the whole session as
  `ClockSynchronization`, although the last measurement stayed valid for
  10 s. On a saturated link (the owner's 23 Mbit/s cellular path carrying up
  to 30 Mbit/s of video) one queued reply is enough. On 11-18 the Windows
  app's sessions to mac-studio ended after 10-40 s with
  `ClockSynchronization` and `Capture(Clock)` among the causes, and the
  host's log shows the viewer closing with a failure in 38 of its recorded
  shares.
- Now a probe past its bound is given up: its late reply is recognized and
  ignored, the next probe measures again, and an unsent probe keeps its
  sequence so the host still sees consecutive probes. The exchange still
  ends with `Expired` when the host answers no probe for a whole
  correlation lifetime (10 s by default), so a silent host still ends the
  session. The host drops a reply it could not send within its 1 s lifetime
  instead of ending the session.
- Diagnostics: the typed cause of a clock failure (`frd: clock
  synchronization ended: ...`) and of a failed host connection check
  (`frd: connection authority ended: ...`, behind the 14 host-side
  `Renewal(Transport(Unauthorized))` ends) are now printed, so the Windows
  app's sessions.log and frd.log name them.

## 2026-11-18 — a large first picture gets six seconds to start

- A first view gets six seconds, up from two, from capturing its startup
  IDR to the viewer's decoded-picture report.
  - On 11-18 a full-screen 3240x1822 first picture from a Windows laptop on a
    23 Mbit/s, 75 ms path took 1.9-2.0 s, so large first views failed
    intermittently even after the recovery-window fix.
  - The limits that bound that one picture move together:
    - frd `decoder_startup::MAX_STARTUP_US`, the host's capture-anchored cap
      and the largest decoder setup timeout;
    - a new decoder `startup()` stage on the host (shared and controlled
      publishers) and on the viewer; attachment stages keep 2 s;
    - the first viewer's source budget (`shared_viewers::FIRST_VIEW_TIMEOUT`);
      late joins keep `Policy::join_timeout`, 2 s;
    - the host's sender recovery horizon (capture to QUIC hand-off of the
      startup IDR, which is sent as a recovery picture), now 6 s through
      `decoder_startup::host_send_policy()`. Mid-session recovery IDRs get
      the same horizon. fr-media's own default stays 2 s; its validation now
      allows up to 10 s (was 5 s);
    - the unattended host startup deadline, from admission to first picture,
      now 10 s (was 5 s). Approval sessions keep their 30 s budget.
  - Unchanged: preparation, attachments, the source bootstrap (`BOOTSTRAP_US`)
    and late joins stay at 2 s. So does the viewer's receive recovery budget,
    2 s from decoder configuration to a complete IDR; on the 11-18 path the
    IDR crossed in 0.7-0.9 s. Raising it pins many fr-media timing tests.
  - The viewer's decoder stage takes effect when a client is rebuilt; the
    host-side limits, which were the binding ones, take effect when the host
    is reinstalled.

## 2026-11-18 — Windows: one late key or click no longer ends the session

- The viewer refuses input older than its 100 ms freshness budget (about
  84 ms after the 16 ms tick margin). A stale key, click or scroll, an
  unmapped key (media and vendor keys), a key the core refused, or a paced
  key queue that timed out each called `input_failed`, which closed the
  whole session.
  - On 11-18 the owner's sessions from the Windows app kept ending after
    tens of seconds; the host logged the viewer closing with a failure.
  - Mouse motion in full screen did nothing: stale motion is dropped
    silently, and a 3240-wide picture on the laptop's integrated GPU keeps
    the UI thread busy long enough for most motion to go stale.
- Now such an event is dropped and never sent late, and the session and its
  control continue. A dropped press needs nothing, because the core already
  ignores the release of a key or button it never pressed. A dropped
  release releases every hold instead, so nothing stays pressed on the
  host. Only a lost control grant (`ControlLost`) still ends control.
- Diagnostics:
  - Every forced close and input failure prints its source location
    (`fr-windows: closing from ...`, `input_failed at ...`).
  - At the end the viewer prints its longest message-pump turn and how many
    stale mouse events it dropped.
  - The app appends each session's arguments, status lines and completion
    document to `%LOCALAPPDATA%\FrankenRemote\sessions.log` (about 1 MB
    kept).
  - A failed session's notice now includes the typed detail, e.g. "failed
    after 32 s (Renewal(...))".
- Not fixed yet: if the UI thread really is that slow in full screen, motion
  still drops. The new numbers will show whether presentation needs to stop
  blocking input.

## 2026-11-18 — Windows: automatic control, full screen, shortcuts to the host

- Control is automatic in a `--control` (View and control) session: every
  replacement view requests it by itself as a fresh request in the new
  session. That covers a resize, full screen on or off, a display change, an
  automatic reconnect, and a return to a window whose control had stopped.
  Before, each of these came back view-only until **Input → Request
  control**. Only the user's own **Stop control** leaves a view without
  control. The text clipboard follows the same choice, unless an earlier
  clipboard owner ended abnormally.
- `--fullscreen` (the app's new **Full screen at this screen's resolution**
  option, on by default) opens a borderless window over the monitor before the
  first picture size is stated, so the host is asked for the monitor's exact
  pixel size and the picture is drawn 1:1. **Ctrl+Alt+Enter** toggles full
  screen while viewing.
- While a session controls the host and its window is in front, Windows
  shortcuts go to the host: the Windows key (Command on a Mac), Alt+Tab,
  Alt+Esc, Ctrl+Esc, Alt+Space, Alt+F4, F10. A low-level keyboard hook hands
  them to the viewer's key path. This is on by default; `--no-shortcuts` and
  the app's **Send Windows shortcuts** option keep them local. Ctrl+Alt+Enter,
  Ctrl+Alt+Del and Win+L stay local.
- The window title no longer flashes. The ready-view token and stale-view
  suspensions can flip the control state several times a second, and the
  title followed every flip. A new state now reaches the title and menu bar
  only after it has held for 750 ms; the first state, an opening Input menu
  and the end of a session still show at once.

## 2026-11-18 — `fr app`: a desktop app with a machine picker on macOS and Linux

- Windows had a Start-menu app (pick a machine, choose options, connect,
  return to the list); macOS and Linux only had `fr connect --choose-host`,
  whose window chose a machine but no options. `fr app` is the counterpart:
  the native picker now also shows View only / View and control, Share
  clipboard text and Ask which display to show, plus the previous attempt's
  outcome above the list. Connect runs the viewer with those choices; closing
  the viewer returns to the picker. Cancel or Escape quits. Choices are
  remembered in the same document as the Windows app's (macOS:
  `~/Library/Application Support/FrankenRemote/settings.json`; Linux:
  `$XDG_CONFIG_HOME/frankenremote/settings.json`).
- macOS shows radio buttons and check boxes under the list. The X11 picker
  has a footer line toggled by F2, F3 and F4 or a click. Toggles only accept
  fresh, focused input, like row navigation, and never confirm a row; a
  toggle between a press and its release cancels that confirmation.
- `fr app` runs two child commands of the same `fr`, so every connection
  takes the existing path: `fr connect --choose-host --pick-only` reports
  `host_selected` with the confirmed choices and opens no connection, then a
  plain `fr connect NODE ...` re-resolves that identity and connects.
  `--app-notice TEXT` (printable ASCII, at most 384 bytes) carries the
  outcome into the next picker.
- `fr app --install` adds `~/Applications/FrankenRemote.app` (macOS) or
  `~/.local/share/applications/frankenremote.desktop` (Linux); `--uninstall`
  removes only a launcher it wrote. `install.sh` installs the launcher with
  the client and removes it on `--uninstall`.
- On macOS a window from a process that `fr app` spawns directly may stay
  behind the app the user is in (cooperative activation): on 11-18 a
  returning picker opened hidden behind other windows. When the installed
  launcher runs the same `fr`, `fr app` therefore starts each step through
  Launch Services (`open -n -W -a FrankenRemote.app --args ...`), so macOS
  brings it forward like any app the user opens. A picker that reopens only
  because the last one expired unused (every two minutes; the app quits
  after half an hour unused) starts with `open -g` and stays behind.
- The macOS picker's rows showed only the node ID: the machine name, set
  through the table cell's `textField` outlet, never appeared. It is now a
  plain label, so each row shows the name above its ID.
- Checked on 11-18:
  - `cargo fmt`, `clippy --all-targets -D warnings` and tests for fr-native
    on macOS 27.0.1 (mac-studio: 235 library, 355 `fr`) and on Linux (ts1:
    470 library, 465 `fr`, and the 5 Xvfb picker tests, including the 2 new
    choice tests).
  - On mac-studio, `FrankenRemote.app` opened the picker in front. View
    only, a keyboard row choice and Connect saved the choices and ran
    `fr connect NODE --view-only`. That attempt failed (mac-mini-max, whose
    host still waits for macOS permissions), and the picker came back in
    front with the outcome and the remembered choices. Cancel quit.
  - On ts1, an Xvfb screenshot showed the X11 picker with the notice and
    the choice line.
- Not tested yet:
  - a successful session started from `fr app` (no host was reachable from
    mac-studio);
  - whether the viewer window comes to the front during a connection;
  - the Linux desktop entry in a real desktop session.

## 2026-11-18 — a large first view starts over a slower link

- A viewer's first picture (the startup IDR) arrives as 1150-byte records on
  the recovery stream, and the viewer set that stream's QUIC receive window
  to the 8 KiB per-record allowance. That allowed one 8 KiB window per round
  trip. On 11-17, a Windows laptop on a 75 ms, 23 Mbit/s cellular path asked a
  5120x2880 Mac for 2890x1624: the ~230 KB IDR came in at about 110 KB/s,
  missed the host's two-second startup deadline, and the host stopped the
  share (the viewer showed `ended internal`). Only pictures of about
  1600x900 or smaller worked.
- The recovery lane now gets the full stream window, as the clipboard lane
  already did. Record framing, negotiated record sizes, the aggregate
  connection window and lifetimes are unchanged. On the same path the IDR now
  arrives in 0.7-0.9 s, and full-size view and control sessions worked.
  Startup still has only about 0.3 s of margin at that size.

## 2026-11-18 — the Windows source install builds with TDM-GCC on PATH

- `install.ps1` took the first `gcc.exe` on PATH. TDM-GCC ships no
  `libgcc_eh.a`, which Rust's GNU target links, so the first build script
  failed with `cannot find -lgcc_eh`. The installer now checks each GCC it
  finds, skips the ones without that library, and puts the chosen one first
  on PATH for the build.
- rustup's stdout no longer leaks into the build step's return value, which
  had failed the install with `Cannot find drive` after a successful build.
- The pinned ISRG root PEMs are written only when they change, so a rerun no
  longer rebuilds fr-mobile-ffi and fr-windows from scratch.

## 2026-11-08 — the host log names how a controlled viewer ended

- When a controlled share's viewer ended on its own side, `frd` kept only a
  count, finished or failed, and dropped the error. `share_ended` now also
  shows `viewer_end: Some(...)`, the viewer's typed ending as the host saw it
  (codes only, no content). On 11-08, 8 live runs ended on the client with
  `Media(InvalidRoutes)` while the host logged just `failed: 1`, so it is
  not known which side ended them first. The next such run will say.

## 2026-11-08 — a key held across the end of a view suspension no longer ends control

- A stale view suspends control input (plan 11.3), and a key or button pressed
  during the suspension is dropped. If the view recovered before the user let
  go, the client tried to send that release for a press the host never saw.
  Its own input model refused the release as an invalid transition, which
  ended the session. The release, or a repeat, is now dropped with its press,
  as an aged press's already was. Not seen live (the harness's xdotool releases
  within a millisecond); a frd test shows it on the previous code.
- `fr` reports `input_dropped_suspended`: local events dropped because of a
  suspension. Before, nothing counted them. In a live ts2-to-ts1 run the host
  saw `hel` of a typed `hello` while the completion showed 10 suspensions and
  `input_dropped_aged` 0.

## 2026-11-04 — taking control survives a late picture report

- While the host started its native input owner for a new control grant, one
  late presentation report (the view aging past its 250 ms source-age bound)
  made the grant check refuse the lease's first ticket. The host then ended the
  whole session. A stale view now suspends this lease, as it already does a
  running one (plan 11.3): the grant stands, no input passes on the first
  ticket, and input resumes on a new ticket issued after fresh evidence.
  Revocation, a replaced owner and lease or observation expiry still end it.
- Seen on ts1 under load: in 8 timing runs of a controlled namespace e2e, 3
  sessions ended right after the grant, all this way. The first presentation
  report can leave about 43 ms of view margin. Two frd tests cover it: a view lapse during the owner's start, where
  press and release must reach the OS on the later ticket, and a revocation
  during the start, which must stay terminal.

## 2026-11-04 — long sessions keep audio through separate output failures

- The viewer and the host each allowed 8 audio output restarts per session.
  After 8 unrelated output stalls, even hours apart, audio stayed off until
  reconnection. The bound now applies to restarts without a healthy generation
  between them: 3,000 packets (30 s) of playback since the last restart earn
  the budget back. An output that keeps failing still ends after 8, typed.
- The two sides count independently (the host packets sent, the viewer packets
  received). Under loss the viewer can end first. The host then offers one more
  generation, which goes unacknowledged, and stops at its acknowledgement
  deadline.

## 2026-11-04 — captured system shortcuts keep their window title

- With `fr connect --control --capture-system-shortcuts` and display
  switching, the viewer's window thread could rewrite the controlled title
  after the confirmed keyboard grab had named itself. The window then hid
  "Remote system shortcuts" while they were captured. The two writers use
  unordered X connections; the window thread now leaves the title to a
  confirmed grab.
- The namespace test expects the capture title with the F7 hint. With the old
  write planted back it fails 2/2 on the title (ts1). The test now repeats the
  chord when a view lapse refused it: input on a stale view is refused by
  design and never replayed. Its key assertions are unchanged.

## 2026-11-04 — audio that outran its window is reported as late

- The completion counted a jitter window or concealment overrun, a missed
  device slot and receiver-side PCM expiry as `audio_output_failed`. Each is a
  freshness refusal: stale sound is not played and a replacement generation
  resumes. They now count as `audio_output_late`, like device-side expiry. A
  packet off the generation's timeline still counts as a failure.
- Seen on ts1: under full-suite load a capture gap longer than the 100 ms
  ceiling failed `every_lane_with_audio_recovers_from_repeated_capture_stalls`
  with `Jitter(WindowExceeded)` although playback recovered.

## 2026-11-04 — shared system audio survives a capture gap

- A hole in the host's PulseAudio monitor capture now spends the sequence
  numbers of the whole frames it lost, on the generation's fixed timeline. The
  viewer conceals them as ordinary loss. Before, the next packet arrived with a
  jumped timestamp, the jitter buffer refused it (`Timeline`), and the output
  generation was reported as `audio_output_failed`.
- The host's batch check now enforces the same sequence-to-sample spacing as
  the viewer, instead of passing a gapped timeline on for the viewer to refuse.
- A real Opus test feeds a three-frame gap through the real jitter buffer and
  expects packets 0-1, concealment for 2-4, then packets 5-6. With the previous
  encoder planted back it fails with `Timeline` (ts1). Gaps longer than the
  100 ms concealment ceiling still end that output generation.

## 2026-11-04 — sharper software video on text and fast motion

- Allow 60 seconds of active encoding between natural IDRs instead of two. Forced
  IDRs for startup, recovery, rate changes and late joins are unchanged. On
  synthetic 1080p text scrolling at 8 Mb/s, mean luma PSNR rose from 46.1 to
  60.5 dB at a lower actual rate.
- Add UMH motion search to x265's ultrafast preset. A 48 px/picture scroll at
  8 Mb/s rose from 30.0 to 43.5 dB. See `docs/video-rate-controls.md`.

## 2026-11-04 — fresh viewing after a checked local window resize

- Let ordinary interactive native viewers accept a real window-manager resize
  after first-frame readiness. Fence the old input mapping immediately, retain
  one bounded native resize request, and require positive cleanup before fresh
  admission, a new decoder epoch and a viewport fitted to the requested size.
- Preserve the display preference against the new catalog and retire original
  control, clipboard and file choices. Explicit resizes share the existing
  32-attempt lifetime ceiling without resetting the automatic retry allowance.
- Retain original resize/cleanup history even if a later connection fails. Add
  regressions for native event provenance, racing publishers, competing display
  changes, cleanup and terminal-cause precedence, fresh epochs and consent.
- New native tests remain unexecuted: the resource-constrained build environment
  disconnected, and hosted verification is pending. No physical desktop result
  is claimed.

## 2026-11-04 — clipboard continues after an unpublished incoming item expires

- Expire a complete queued or explicitly deferred clipboard record through the
  original receiver, without replacing its watcher, channel or controller grant.
  Canonical metadata retains the consumed sequence after the payload is cleared.
- Keep both original handoff clocks and distinguish queued work from a native
  operation already executing. Unknown effects, transport expiry, retained
  outgoing payload and privacy/authority changes keep their terminal handling.
- Add regressions for paused expiry, fresh later items, replay, malformed records
  and handoff races. New test execution is pending recovery of the build environment.

## 2026-11-04 — explicit native microphone on the controlled viewer

- The original viewer now owns a bounded AudioUp expectation, exact selected
  media binding, native capture factory and permission/talk/mute lifecycle.
  The setup deadline starts before the first host offer and remains on every
  clock, media and recovery I/O path.
- The opt-in `linux-microphone-session` bridge uses the real recorder and
  transmitter. Attachment stays muted; permission, an explicit talk action and
  the matching host acknowledgement are all required before capture starts.
- Exact original configuration and packet guards remain retained through native
  queue custody. Safe optional retirement preserves the terminal exchange;
  cancelling the original controller fences the same capture owner.
- Clipboard and file negotiation preserve the positively selected microphone
  exchange. CI now includes the new actual-worker session and ACK cases.
  Public command offers remain disabled; native session execution is pending.

## 2026-11-04 — microphone joins the original controlled host

- The running host can negotiate one separately approved microphone attachment
  and service the existing native device/codec owner alongside input renewal.
  Negotiation retains the original controller, connection, selection and deadline.
- Microphone stop, setup failure and session revocation retain native fences and
  actual child retirement receipts. A drained optional stop preserves control;
  foreign attachments and spent microphone slots cannot be silently reused.
- Original acknowledgement and setup guards remain on the connection even
  when clipboard, files or other media helpers perform I/O.
- This is the controlled-session library integration. Public command offers
  remain disabled pending visible client and host talk/stop controls. New
  session regression tests are authored; full execution remains a CI gate.

## 2026-11-04 — bounded CLI help and lifecycle regression repairs

- Keep an earlier source failure authoritative when a later resize arrives
  before its outer fence; shared viewers cannot inherit a false geometry retry.
- Keep the complete `fr --help` output below its existing 16-KiB ceiling while
  retaining the documented commands, permissions and recovery limits.
- Correct microphone, control-bootstrap and shortcut-capture fixtures to assert
  their current production contracts: fresh consent after a retired epoch,
  required visibility before control, and the actual controlled-window title.
- Drain the original duplex file payloads with their real receive/receipt owners
  still running. Both sources stay paused; the original drain deadline and
  unchanged-byte assertions prevent the fixture from hiding extra admissions.

## 2026-11-04 — fresh viewing after host display geometry changes

- Preserve a checked native resize/topology failure through source teardown,
  shared viewers and terminal session/input reports. Retire the old source and
  recreate the host catalog after cleanup, with a separate bounded restart
  policy (`fr-s7o1`).
- Reconnect native viewing through fresh identity, admission, display selection
  and native owners. Control and file choices remain spent; failed cleanup,
  local stops and other adverse host reports still prevent retries.

## 2026-11-04 — private recovery retains the original feedback scope

- Validate obsolete presentation, decoder metrics and repair records before
  discarding them during private reference recovery. Foreign, malformed and
  unnegotiated traffic now ends the session through the normal protocol handler.
- Both local-failure admission and replacement waiting retain the old binding,
  capability selection and original recovery deadline. Drained reports cannot
  refresh presentation evidence or refill recovery allowance (`fr-c4i5`).

## 2026-10-01 — host failures in the granting round are named too

- The `host_failure` label (134b6d7) missed the common case. When control
  is granted in the same serving round that then fails, the lease still read
  `local_revoke`, as two namespace matrix rows showed.
- The granted lease now registers on the session's observation owner. The
  serve fence names the failure on it before revoking: revoking cancels the
  session, and the input agent would otherwise stop the lease as merely
  cancelled.

## 2026-10-01 — aged local input is dropped instead of ending control

- Cause of `input_dispatch_stalled` (fr-1r40):
  - While a new picture waits for its visibility confirmation, the
    controlled viewer holds input, because which picture the user saw is not
    yet known.
  - Under CPU load that wait took 145-161 ms, and an event captured behind
    it passed its 100 ms bound, which ended control.
  - Found with a scratch build that timed every await of the viewer loop and
    the wait itself. No poll gap exceeded 40 ms and no single poll exceeded
    20 ms; the loop was running the whole time, only holding input.
- Now an aged press, repeat, motion, scroll or text is dropped, never sent
  late. The release or repeat of a dropped press goes with it.
- A release of a key or button the host holds, and held-state
  reconciliation, are sent late rather than leaving input held.
- Capture no longer stops on an aged event.
- `fr` reports `input_dropped_aged` in its control completion.
- The `input_dispatch_stalled` code (46a5891) is removed: that end can no
  longer occur, so `fr` no longer lists it.

## 2026-10-01 — host failures are no longer reported as local revokes

- When a controlled stream fails on the host while its authority is still
  live, the lease's terminal report now says `host_failure`. `fr` shows it as
  `host_control_failure`. Media, transport and renewal failures used to read
  `host_control_revoked` with reason `local_revoke`, as if someone at the
  host had revoked control.
- A reason named earlier, such as the session agent's local revoke, is kept.
- A viewer clock probe whose reply missed its deadline is now named
  `transport_deadline_expired` instead of `native_session_failed`. It had
  aborted one control matrix run at 120 ms RTT with 1% loss.

## 2026-10-01 — controlled sessions negotiate reference recovery

- A controlled session now runs the same reference recovery as a view-only
  one when both sides negotiate it (RECOVERY_HOST.md, Controlled sessions).
  Until now a lost reference ended control (`video_reference_lost`).
  - The loss suspends the grant's input; renewals continue.
  - After the handoff, the host advances the view's recovery generation in
    the authority. Every ticket is invalidated, and an action decided on an
    older picture is refused as a stale view, even with a new ticket.
  - The same lease resumes on the recovered picture once a new ticket
    arrives. Nothing is replayed and no new grant is made.
  - A viewer still acquiring control keeps the terminal refusal.
- Found on the way:
  - The round wrapper must forward `input_submitted`, or a controlled
    host's input no longer wakes an idle capture.
  - A recovery fences the old receiver at the loss itself, so the grant
    must suspend there rather than end on the fenced tracker.
- Not yet shown end to end. Namespace control matrix, two runs against two
  of 67acc95: no row recovered a reference. Host diagnostics show why: at 40
  and 120 ms RTT with loss, the host's own sender usually expires a picture
  it could not finish sending within the 250 ms reference horizon
  (`OriginalExpired`) and ends the session first. Full suite 63/63; the
  stale-view suspend e2e passed 2 of 2.

## 2026-10-01 — two more control ends are named

- Two local ends of a controlled session that `fr` reported as the generic
  `native_session_failed` now have their own codes. The session still ends;
  only the report changes.
  - `input_dispatch_stalled`: a captured key, text or pointer event waited
    past its 100 ms dispatch bound before the viewer could send it.
  - `control_renewal_failed`: the control lease could not be renewed, or
    proven live by a newer ticket, before its deadline.
- `fr --help` now lists every named end, including `video_startup_expired`.
- Namespace control matrix: the stalled dispatch had aborted most runs at
  the 6 ms RTT, 5% loss row as an untyped end. With the name, the matrix
  reaches its 40 and 120 ms rows. The stall itself (the viewer loop not
  driven for 108-140 ms) is not fixed.

## 2026-09-30 — a stale view suspends control instead of ending it

- When the presented view goes stale under a held control lease, input is now
  suspended and the lease survives (plan 11.3; PRESENTATION_READINESS.md,
  Suspension).
  - The host releases every key and button it holds for the client at the
    lapse. Fresh readiness waits until that release is confirmed.
  - Control renewals, media and presented reports continue.
  - The client refuses input while suspended (`ViewSuspended`) and never
    queues it.
  - Input resumes on the same lease once the view is fresh and a ticket
    issued after the host's lapse arrives: at least about one second.
  - A suspension longer than 10 s ends control (`view_stale`).
  - `fr` reports `input_suspensions` and `input_suspended_ms`.
- Namespace e2e: a held key is released by the host at the lapse, input on
  the suspended view never reaches the host, and the same lease resumes. It
  passed 3 of 3 runs, each with one suspension of 1.05-1.24 s.
- Two faults found while qualifying it:
  - A capture stalled past the 250 ms reference horizon produced an
    unchanged-source result that ended the session. It is now dropped as
    obsolete evidence, like an expiring queued observation.
  - The client refused control-renewal challenges while suspended.
- Namespace control matrix, A/B against 33c2bd0 at load 21-45 (3 runs
  against 2):
  - 6 ms RTT with 5% loss held in 5 of 6 rows (0 of 3 without).
  - 120 ms RTT without loss held 2 of 2 (0 of 1 without).
  - No row ended with `view_stale` (5 did without).
  - Rows with loss at 40-120 ms RTT still end, now with
    `video_reference_lost` or `transport_deadline_expired`. Controlled
    sessions have no reference recovery yet.
  - An untyped `native_session_failed` still aborted 1 of 3 runs (2 of 2
    without).

## 2026-09-30 — slow paths keep their connection

- The presented-view source-age bound follows the measured path instead of a
  fixed 250 ms (PRESENTATION_READINESS.md, PRESENTATION_FRESHNESS.md).
  - The viewer allows 250 ms plus three smoothed round trips; the host allows
    250 ms plus four. Both are capped at 1 s, which is also the wire ceiling
    for a sample's age.
  - An unmeasured path keeps 250 ms.
  - A smaller bound never shortens a confirmed host deadline.
  - Namespace matrix, four runs with the path bound:
    - Control at 120 ms RTT without loss held in 2 of 8 rows, against 0 of 2
      with the fixed bound.
    - Control at 40 ms RTT with 1% loss held in 2 of 6 rows at the final
      multiples.
    - View-only at 120 ms RTT without loss held in 5 of 8, against 1 of 2.
  - Control still ends at those points. The usual cause is the input client's
    own stale-view stop after a lost packet delays the source observation.
    That stop is terminal today; the plan asks for suspension, which is next.
  - At 120 ms RTT control also ends with `transport_deadline_expired`, and
    view-only at 120-200 ms with `host_not_heard`.
- A viewer whose decoder start or restart runs out of time now ends with the
  named `video_startup_expired` instead of the generic
  `native_session_failed`.

- An admitted reliable QUIC record had to be acknowledged by its send-by, and
  missing that closed the connection. At 120 ms RTT a 250 ms record could miss
  it on a healthy path, because records on one stream wait for the previous
  epoch. Such sessions ended with `transport_deadline_expired`. A record now
  has max(send-by, admission + a path allowance). The allowance is three RFC 9002
  probe timeouts from the measured RTT, clamped to 250 ms-2 s (QUIC_RECORDS.md).
  Admission still refuses records past their send-by, and a peer that stops
  acknowledging still closes the connection.

## 2026-09-29 — a repaired picture on a static screen is shown; control without a wheel

- A picture whose repair completed after its 50 ms display budget was decoded
  without being presented even when it was the newest picture. On a static
  screen nothing replaced it, so a view-only session showed the previous
  picture indefinitely without a named end (12 of 61 diagnostic repetitions
  at 40 ms RTT with 5% loss). The newest late picture is now presented; a
  superseded one is still decode-only, and late presentation is never fresh
  evidence (PRESENTATION_FRESHNESS.md). With the fix: 0 of 24.
- Control capabilities are a meet, not a static grant (fr-rc2-control-capability-meet-sf8).
  `frd run --input-agent` probes its executor once at startup (the same display
  open, no indicator, no input). Missing keys, repeat, absolute pointer or
  buttons refuse at startup (`control_capability_missing`); a failed probe is
  `input_agent_unavailable`. Line scrolling is offered as the optional
  `native-input-line-scroll` v1 capability only when the executor has it, and
  `fr` asks for the wheel only when both peers selected it. Previously a host
  with several X screens or unmapped horizontal wheel buttons failed every
  grant. `fr`'s control completion reports `control_capabilities_granted` and
  `wheel_unavailable`. Namespace e2e: a viewer wheel notch reaches the host as a
  button-4 press/release; a host without horizontal wheel buttons keeps control
  without the wheel (planted negative: the static grant fails it).

## 2026-09-28 — impairment limits, named session ends, installer options

Namespace evidence only, as below.

- Real `tc netem` impairment rows for control and view-only sessions (7832436,
  cfb2402). Control holds on a clean link and at 40 ms RTT, and ends at 60 ms RTT
  or more, or with 1% loss. View-only holds with 1% loss and at 100 ms RTT, and
  exits at 200 ms RTT. Causes isolated: near 60 ms RTT the fixed 250 ms
  source-age bound ends control, and beyond it the transport record deadlines
  do (PRESENTATION_FRESHNESS.md). The owner decision on view-lapse semantics
  and an RTT-aware bound is pending.
- `fr` names local session ends: `view_stale`, `transport_deadline_expired`,
  `host_not_heard`. A stale view met by the transport's view gate is no longer
  a bare `Unauthorized`, and any remaining generic end prints its typed chain
  on stderr.
- Transport: critical session records (renewal challenges, control responses,
  presented reports) are no longer held behind queued media datagrams. Under
  loss, that wait expired host renewal challenges and closed view-only sessions.
  View-only now holds at 40 ms RTT with 5% loss in the netem rows.
- `frd install [flags] -- <frd run options>` (systemd), validated at install
  time. User units start with the graphical session, and configuration
  refusals do not restart-loop (514af9b).
- Test oracles instead of races: the CA-bundle link policy, and UDP socket
  release checked by this process's own descriptors (e1f9c54, b2abf66).

## 2026-09-25 to 2026-09-27 — control, clipboard, audio, files, second reality check

All namespace end-to-end evidence (real processes, QUIC, X11 and codecs; fixture
LocalAPI and CA); no client on a second tailnet machine has connected yet.

- Remote control: `frd run --input-agent` + `fr connect --control` (keys, buttons,
  absolute pointer, discrete wheel) through a per-lease `fr-input-agent` XTest
  child with a mandatory indicator; consent surfaces reject controller-injected
  XTest events (a7b29ce).
- Cursor forwarding with one renderer (view-only and control); opt-in text
  clipboard; viewer-to-host file sending (and both together since 41f2201); host
  playback audio for view-only sessions, client Opus decode in the restricted
  `fr-opus-worker`.
- LeaseRevoked, Closed and CloseRequest wire kinds and the bounded close exchange.
- Root `frd ingress-helper` so `frd run` can run unprivileged (6019af0).
- Control-session deaths root-caused to IDR fragments paced four per turn and
  fixed without weakening any deadline (eb8f450, bd177a8).
- `frd run --logind-session`: lock, logout, switch or suspend of the selected
  session ends sharing and the run with a typed cause (5a8e12d).
- Operator/diagnostic strings no longer understate control or advertise the
  unserved HTTPS/h3 endpoints (e55e56e).
- Second reality check (2026-09-27): 19 new beads (fr-rc2-*), three false
  closures reopened, done-at-namespace beads blocked on a live lane (71541f6).
  Narrow CI workflows moved to nightly/dispatch (91d4e3f).
- Still open: a live two-machine run, transport interoperability (Asupersync
  pin), more than one peer at a time, hardware HEVC, Wayland/macOS/Windows/
  browser/mobile, and size (about 312,800 lines against the 240,000 planned
  maximum; the hard stop is 500,000 since 2026-09-24).

## 2026-09-12 to 2026-09-24 — audit, first host composition, withdrawn claims

- A reality check on 2026-09-23 (at e1d9cc2) found that no binary could host. It
  also found that a September 19-22 burst had closed 65 beads, including every
  phase gate, and had added fabricated success surfaces. 46 false closes were
  reopened (6fbd380).
- `frd run --software-explicit` now composes the existing host library into a
  real listener. It has been verified only in the namespace end-to-end suite,
  not on a live tailnet. Fixes along the way:
  - tailnet admission, which refused every real peer;
  - acceptance of the installed daemon's `"Peer": null`;
  - the 3-second session death (QUIC receive batching);
  - headless Xvfb cookie authentication;
  - `fr` trust roots now default to the distribution CA bundle.
- Withdrawn fabrications:
  - the `frd status` fixture output;
  - the `fr doctor` granted/passed rows;
  - the `fr robot`/`fr disconnect` success envelopes;
  - the GNOME/KDE/Hyprland and Windows GPU qualification tables (docs and runtime);
  - the Windows/macOS sandbox claims.
- CI runs every lane independently. Missing native libraries and two flaky tests
  were fixed.
- Later on 2026-09-24: departing viewers are peer outcomes (fr-704 closed);
  `frd run` started and stopped cleanly on a real tailnet host after the nft
  readback fix (real nftables prints `meta iif` by name).
- Still open: remote control (input-agent process), a live two-machine run, and the size gate (272,665 at 0b543db against
  a 250,000 hard stop).

## Native host publication bootstrap

- Join approved observation, native display discovery, explicit selection,
  channel attachment, decoder startup, and continuous capture through
  `HostSession::publish_display`. Preserve the original connection and worker,
  call-time deadlines, renewal during native waits, and fence-before-cleanup.
  [HOST_PUBLICATION.md](HOST_PUBLICATION.md) records the native evidence and limits.

## Continuous decoder-load feedback integration

- Connect the canonical negotiated decoder-metrics protocol to continuous
  host/viewer service and actual capture pacing, including input and receipts
  during supervised decoder waits. Preserve original query/reply deadlines and
  exact connection/view ownership. See [RECEIVER_FEEDBACK.md](RECEIVER_FEEDBACK.md).


## 2026-09-10 — Native control-lease renewal

Join observation delivery and native input to the same approved authority and
renew control outside the native-call mailbox. Preserve original challenge
expiry, independent revoke/cleanup, fixed transport storage, action receipts and
all ticket/view boundaries. A consumed native attachment cannot reset its replay
ledger; old owners remain fenced even across reused numeric lease identities.

The viewer's control responder belongs to its input/view lifetime, not generic
network liveness. Real UDP/TLS and X11 tests keep observation, control and tickets
alive beyond the initial lease, and verify releases, expiry, replay, backpressure
and cancellation while native calls are blocked. See
[CONTROL_LEASE_RENEWAL.md](CONTROL_LEASE_RENEWAL.md) for exact integration and
verification scope. Initial grants and the complete application loop are not
claimed implemented. No Asupersync pin or dependency changes.

## 2026-09-09 — Observation renewal over session control

- `d6b01fc`: implement the bounded Challenge/ChallengeResponse codecs and a one-response viewer owner. Preserve full session/scope bindings, opaque host deadlines, exact bytes across backpressure, replay refusal and redacted diagnostics.
- `aca5fa2`: attach one renewal owner to the existing approved observation and actual QUIC control streams. Timely matching responses install the original issue-time deadline. Queuing, ACKs, media traffic and control-scope responses do not renew observation. Unsent expiry, peer FIN/RESET, failed or abandoned I/O and owner drop end observation; connection replacement cannot redirect old challenges.

Twenty new codec, responder and real localhost UDP/TLS tests pass. The local selection totals 295 tests, zero failed or ignored, with strict selected Clippy and formatting. Runtime-bound checks rebuild first-party sources against retained matching Asupersync TLS libraries, not a fresh dependency build. Source `d6b01fc` also passed complete native-workspace GitHub verification in run 34373509056; the later combined revision is checked separately. No Asupersync release or dependency pin was changed.

Observation renewal is separate from Tailscale revalidation, local consent, source/presentation freshness, input tickets and control-lease renewal. This adds no listener or graphical lifecycle. See [PROTOCOL_AUTHORITY.md](PROTOCOL_AUTHORITY.md) for exact bytes, integration and evidence.

## 2026-09-09 — Installed Tailscale admission through final media/input effects

- `a1e3c1`: implement the Linux installed-LocalAPI boundary with root Unix peer credentials, bounded Asupersync HTTP, consistent status/WhoIs snapshots, exact node addresses, explicit sharing scope and per-connection app-capability grants. No prefix/DNS membership inference, embedded VPN, new runtime, policy mutation or public listener.
- `204e952`: add a single owned admission lifetime with shared non-renewing checks. Expiry, revoke, dropped owner/refresh, identity changes and permission changes are terminal. Bind approved observation, capture deadlines and the existing prepared-media final-send guard to that lifetime.
- Connect the same gate to `Seat::start_admitted`, canonical input enqueue/idle service, and every post-preparation native submission check. Read-only grants never invoke the native input factory; already submitted effects keep their original receipts and release-only cleanup.

The initial admission sources passed clean CI run 34314791348. The exact shared-lifetime/media objects passed full pinned-toolchain workspace verification, docs, 23 LocalAPI/lifetime tests and five root-owned synthetic media scenarios in run 34315921329. Local input-extension checks passed all eight media/input scenarios, strict first-party/test Clippy and 36 existing owner/watchdog/egress regressions. Removing only the final post-preparation gate caused the unchanged input scenario to fail on a revoked key press. The full input-extension run is 34318181209, separately revision-bound.

These fixtures exercise real Unix/HTTP/peer credentials and the production policy/owners with synthetic authority metadata, opaque media packet bytes and a recording input sink. They do not establish live Tailscale sharing semantics, TUN ingress, OS input effects or a complete network session. Primary Asupersync QUIC and concurrent decoder/critical-stream work are preserved. See [TAILNET_ADMISSION.md](TAILNET_ADMISSION.md) for the explicit app-grant profile, integration, evidence and remaining gates.

## Native HEVC over primary Asupersync QUIC

- `52bd52d`: retain one exact prepared media record across transport backpressure, with unchanged owner-bound packet identity and deadline. Service cache and pending-record expiry on idle turns; terminal close releases viewer-owned storage without revoking another viewer's observation.
- `ed14f73`: connect that canonical sender to `fr-transport::quic::QuicRecords`. Actual native capture and presentation workers now exchange HEVC through real UDP/TLS QUIC, including selective repair and cancellation-terminal I/O. The surrounding session retains ownership of unrelated control/input streams.
- `3877a9f`: fix the imported transport test's pending-offer move across retries by borrowing it; preserve the immutable capability and every assertion.

Six new native/QUIC integrations and seven egress tests passed locally. Three scenarios each present six changing images through X11 capture, direct software HEVC, real QUIC, reassembly, supervised decode and X11 readback. They force actual transport backpressure and recover both a missing reference fragment and an entirely lost final picture before idle. Separate tests cover revocation, dropped I/O futures and route substitution. The eight canonical live-QUIC tests also pass after the borrow correction.

The current core/wire/media/client Cargo suites passed 237 tests with zero failures or ignored tests; selected source/test Clippy and formatting passed. Native and runtime-bound local tests rebuild first-party code against the pinned compiler and exact retained Asupersync libraries, not a fresh full Cargo-workspace build. Combined-revision CI is a separate gate. There is no claim of live Tailscale admission, independent-peer interoperability, GPU performance, optical latency or an installable desktop. See [MEDIA_QUIC.md](MEDIA_QUIC.md) for ownership contracts, reproduction commands and measured scope.

## Native input cancellation and result delivery

- Check parent Asupersync cancellation after each native preparation and before
  final input submission; preserve an already submitted text prefix. Two
  deterministic negative controls fail on unchanged `3f4f39b` and pass after
  the fix.
- Return actual native receipts through the existing `InputResult` codec with
  their original request binding, even across cleanup and handoff. Preserve
  explicit evicted/missing/unknown outcomes and cancelled-wait semantics.
- Exercise the canonical native owner and watchdog with actual XKB/XTest,
  private Xvfb servers, idle expiry, blocked preparation, lifecycle stops and
  native repeat restoration. See [INPUT_AGENT_RESULTS.md](INPUT_AGENT_RESULTS.md)
  for locally verified test scope and remaining integration limits.

## 2026-09-08 — Input records through native submission

- `d9f8ea7`: seven bounded, allocation-free input action codecs with complete view/authority bindings, separate pointer/action sequence spaces and independent golden bytes.
- `3b8f7e9`: join wire actions to submission-time authority, replay accounting, pointer barriers, scalar-bounded text, explicit partial/unknown results and release-only held-state cleanup.
- `eca3253`: add the opt-in real X11 pointer/button adapter and native effect tests for dragging, duplicate suppression, stale pointers, expiry cleanup and mid-action revoke.
- `8f501c0`: account for FFmpeg ABI row alignment in decoder allocation admission; preserve exact wire geometry limits and expand cropped-frame regressions.

The input features add no Rust dependency, runtime, alternate transport or network listener. The native adapter does not guess keyboard layouts or claim unsupported text injection. Watchdog, identity/transport and full client/agent integration remain open. See [PROTOCOL_INPUT.md](PROTOCOL_INPUT.md), [NATIVE_INPUT.md](NATIVE_INPUT.md), and [IMPLEMENTATION_STATUS.md](IMPLEMENTATION_STATUS.md) for exact verification and remaining boundaries.

## 2026-09-08 — Encoded-media delivery

Implemented the missing encoded-picture delivery path rather than adding another set of placeholder traits.

- `5d3b23f`: add `fr-wire` with executable FRD0 video fragments, reliable recovery chunks, progress announcements and selective-repair requests; allocation-free borrowed parsing, checked fragment geometry, actual transport-sized records, and independent golden bytes.
- `0210533`: add subscription-scoped reassembly, contiguous IDR recovery, reference-ordered decoder delivery, separate display/reference deadlines, missing-final-picture repair, and memory reservations that remain charged while a decoder owns compressed input.
- `7923591`: add the sender packetizer and bounded reference cache, original-fragment retransmission, repair cadence/byte limits, unsent-reference expiry fencing, and sender/receiver loss-reordering-duplication regressions.
- `b2a2888`: add a reproducible real HEVC corpus lane and independent software decode comparison; include example parser regressions in the normal verification lane. Adopt the reviewed sender formatting without changing behavior.

The workspace remains safe Rust without additional production runtime/codec dependencies. Linux pinned-nightly source checks and offline HEVC preservation tests do not establish live Tailscale/QUIC, capture, hardware acceleration, presentation latency or OS-input support. See [IMPLEMENTATION_STATUS.md](IMPLEMENTATION_STATUS.md) and [MEDIA_DELIVERY.md](MEDIA_DELIVERY.md).

## 2026-09-08 — Authority and input replay

Earlier source work added issue-time authorization renewal, exact-deadline expiry, refusal/closure cleanup, clock and suspend fencing, bounded input receipts with a persistent consumed-sequence floor, and redacted diagnostic formatting. Source `e7d57d5` passed 76 Rust tests before the encoded-media additions. These policy components still require authenticated session and native input integration.
