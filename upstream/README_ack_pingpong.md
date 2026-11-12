# asupersync: ACK ping-pong between bounded-window peers (landed as fa43c85f4)

Status: landed on asupersync main as fa43c85f4 (2026-11-11). FrankenRemote
has pinned revisions that contain it since dcb31f51 (079214d3f, then
becc05376). The diff below is the
October prototype, kept as the record.

- The hub late-joins failure under Evidence was a test-harness bug: the
  harness dropped a peer's turn mid-drive, which closes that viewer. Quiet
  idle connections made it fail every run. Fixed in dcb31f51.
- The namespace clipboard regression under Evidence is still unresolved. On
  trj (2026-11-11), that test failed at both pins in all four runs with
  `clipboard_receive_expired`, not `clipboard_item_expired`. The October
  symptom (`clipboard_ended`) did not appear, but a test that fails on both
  arms cannot show whether the fix changes it.

## Finding (FrankenRemote, ts1 namespace, 2026-10-16)

An established, idle view-only session (static desktop) exchanged ~940 UDP
packets per second in EACH direction between frd and fr, indefinitely:
strict alternation of 62-byte packets at loopback round-trip speed. Debug
builds: the frd host thread at 84-97% of a core, the fr main thread at 44-48%.
A frame trace (scratch build) showed 15,682 of 15,836 client packets were
`ACK + MAX_STREAM_DATA(3) + MAX_STREAM_DATA(7)`; application traffic over the
whole ~20 s session was 17 datagrams and ~300 stream frames.

Cause: `NativeQuicConnection::queue_ack_frame` re-attaches every bounded-window
stream's current MAX_STREAM_DATA to EVERY outgoing ACK (the MATRIX-227
anti-wedge choice). MAX_STREAM_DATA is ack-eliciting, and FrankenRemote bounds
every route stream's receive window (fr-transport `configure_stream_receive_window`
with `policy.stream_window`), so each side's ACK elicits the other's ACK.

## Prototype (`asupersync_ack_pingpong.diff`, against rev ee72b665e)

In `process_packet_frames`: when an ack-eliciting packet carried only
ACK / MAX_STREAM_DATA / MAX_DATA / PADDING, queue a plain ACK (no re-attached
advertisements). Data, PING probes and STREAM_DATA_BLOCKED still get them, so
a credit-blocked sender's probe cannot wedge on a lost update. Unit test
`a_window_update_only_packet_gets_a_plain_ack_but_a_ping_gets_advertisements`.

## Evidence

- Idle session with the prototype: 85 datagrams/s total (was 1,853-2,166);
  host thread 20.7% (was 84-97%), fr 9.7% (was 44-48%), debug builds.
- asupersync `net::quic_native::connection*` tests: 162 passed + the new one.
- FrankenRemote CI lane: 3822 passed / 1 failed (hub late-joins test, a
  known load flake that also failed on CI without the patch).
- Namespace suite 81/82: `real_clipboard::a_copy_that_outlives_its_deadline_
  on_a_slow_path_ends_the_clipboard_not_control` (120 ms RTT, 200 KB copy)
  ended `clipboard_ended` instead of `clipboard_item_expired` in 2 of 3 runs
  with the patch, 0 of 3 without. Not diagnosed. Hypothesis: the endless
  exchange was also delivering window credit or wake-ups every RTT, so bulk
  pacing on slow paths changes. A more conservative variant may be needed,
  e.g. re-attach an advertisement only when its value changed since it was
  last sent, relying on loss recovery to retransmit lost MAX_STREAM_DATA.
