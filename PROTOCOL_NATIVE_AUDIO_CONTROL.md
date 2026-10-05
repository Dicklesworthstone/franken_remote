# Native playback audio during control

Status: opt-in v0 extension; source implementation, not a hardware or end-to-end qualification report. Implements the playback portion of plan section 15.4. Microphone forwarding is unchanged and is not enabled by this extension.

## Capability and compatibility

The optional capability `native-audio-control` version 1 extends `native-audio-down` version 1 to a `RequestControl` selection. BOTH capabilities must be selected at their exact versions. The legacy `native-audio-down` capability alone retains its observer-only meaning from PROTOCOL.md; no existing record layout or meaning changes.

An `Observe` selection continues to require only `native-audio-down`. A `RequestControl` selection missing either capability continues without audio, without waiting for an audio attachment and without downgrading or refusing control. In particular, older hosts that advertise playback for observers but cannot supply it with control must not cause a new client's startup to hang. Missing optional audio is reported as an absence, not as successful playback.

The host offers the extension only when its operator locally enables both control and playback capture. A client offers it only when the local user explicitly asks for playback during control and the client supplies isolated audio decoding/output. Offering or selecting the extension is not an input grant, local approval, microphone consent, evidence of native readiness, or permission to broaden observation scope.

## Attachment and startup

On a controlled selection with both capabilities, the native bootstrap attaches Configuration, Recovery, Video, Input and AudioDown on the ORIGINAL admitted connection. Every role consumes its own original one-use ticket and binding. The input and audio attachments are distinct; neither attachment grants an input lease. Existing route count, byte, record, binding and generation limits apply unchanged.

The audio channel is joined to the same selected display/view proof and authenticated observation session. It retains the existing AudioDown directions: host reliable AudioConfiguration/AudioStop, viewer reliable AudioConfigured/AudioStop, and host-to-viewer AudioPacket datagrams. There is no uplink, second connection, alternate codec, or independently renewable authority.

Audio capture is demand-driven and uses the existing AudioSource, bounded AudioRing and per-subscription AudioLane. Native capture runs in the supervised audio child, independently polled alongside session service. No packet is sent until that viewer's exact configuration acknowledgement is admitted. Pre-acknowledgement sound is not replayed. Acknowledgements must match the original binding, direction, current epoch and actual negotiated format.

## Authority, scheduling and teardown

Original observation approval and expiry gate audio, including any observation before the local control request. Input still requires its separate explicit grant, presented-view checks, valid tickets and native submission checks. Audio never supplies evidence of screen freshness and cannot renew or resume input authority.

The managed input driver retains poll priority before and after each session turn. Audio source IPC is independently polled; waiting on it never becomes a wait on the input executor. On the client, PulseAudio operations and destruction stay on the bounded foreign-work thread and Opus decoding stays in the restricted per-epoch child. Packet queue time consumes the original receipt deadline; queues are bounded in bytes and count.

On session end, revoke the original authority first, close the audio lane and discard its retained packets, then stop and reap the original child using its retained retirement receipt. No new worker may replace an unreaped one. Source/device failures are typed audio stops, not grants or video freshness, and must not silently create a fresh audio lifetime. Confirmed process exit, transport admission and audible observation remain distinct evidence categories.

## Verification boundary

The source includes capability, route/epoch, acknowledgement, deadline and scheduling regressions. Those tests do not certify speakers, a physical microphone, an installed desktop audio server, live-tailnet interoperability, or audio/input timing under impairment. Bead `fr-rc2-audio-during-control-35e3` remains open until the real controlled-session audio and independent-input acceptance tests pass with retained evidence.
