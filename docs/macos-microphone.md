# macOS client microphone forwarding

The macOS client can send an explicitly selected microphone through the existing
Opus AudioUp lane to a host that separately advertises, approves and implements
a virtual microphone endpoint. The current concrete host endpoint is Linux
PipeWire. This adds client capture; it does not create a macOS or Windows host
virtual input device.

## Build and select

Build the client and its matching worker with the same pinned toolchain and
trusted static Opus library used by Mac playback:

```sh
export FR_NATIVE_OPUS_LIBRARY_DIR="$(brew --prefix opus)/lib"
cargo build --release --locked -p fr-native \
  --features macos-desktop,macos-microphone \
  --bin fr --bin fr-audio-worker
```

Install both together. `install.sh --from-source --client-only --audio` builds
and signs the client audio helpers. Releases include the same helper. A manual
installation must sign **both actual binaries**, preserving stable identifiers
`com.frankenremote.fr` and `com.frankenremote.fr-audio-worker`, with hardened
runtime and `crates/fr-native/macos-microphone.entitlements`. Both binaries have
an embedded `NSMicrophoneUsageDescription`; the app launcher also supplies it.
The permission helper refuses a missing usage string or audio-input entitlement
before requesting access. A launcher's grant is never assumed to authorize a
differently signed helper.

```sh
fr connect NODE_ID --control --experimental-native --display choose \
  --microphone --microphone-source default
```

`default` is an explicit, one-time selection of the current CoreAudio input,
resolved before contacting the host. Its exact UID is then pinned to this
invocation. An exact CoreAudio UID can be supplied instead. There is no runtime
default-device fallback, device creation or source switching. Device removal,
route or format changes end capture. `--microphone-server` remains Linux-only.

## Consent and lifetime

The original controller gets a separate visible, nonactivating microphone panel:

1. **Allow microphone** runs a permission-only invocation of the exact installed
   capture helper. It may show the macOS permission prompt. No device is opened.
2. Once permission is granted and the negotiated lane is ready, a **new Talk**
   starts the original talk generation. A pending, stale or early Talk is never
   saved for later readiness.
3. **Mute** immediately withdraws that generation's local consent. The UI waits
   for the old capture child to exit before enabling another Talk.

The permission request carries its original launch deadline over private IPC;
a delayed process launch cannot invent a new prompt lifetime. The OS reply has
a separate, fixed 60-second maximum from the original choice. Mute, lost UI,
session shutdown or parent death stops the permission helper. A positive OS
reply leaves the microphone muted.

Actual capture starts only after the existing original controller lease,
host AudioConfigured acknowledgement, current OS permission and fresh local
Talk all agree. An independent 250ms visible-UI watchdog is terminal on late or
missing progress. The native worker independently checks the original authority
deadline and private revoke socket, even when the session task stops polling.
Its parent-death watcher terminates a stuck native helper. Reconnects, F8,
changed display, Stop control and restored OS permission cannot reuse old Talk.

## Native data path and bounds

The private helper uses CoreAudio Audio Queue input on one serial callback
queue. It copies 480 mono signed16 samples at 48kHz per packet and uses the
existing real Opus encoder at 64kbit/s. The immutable first profile is 10ms
mono, maximum 512 encoded bytes, with the existing 20ms host jitter target.
There are three fixed native buffers and a four-slot PCM ring. Overflow,
discontinuity, unknown clock, sleep or device changes stop the owner; there is
no growing audio queue or hidden resampler fallback in application code.

Original first-sample timestamps survive encoding and private IPC. Each pull
retains at most four packets and at most 100ms original custody. Native capture,
permission prompting, encoding and child reaping stay outside AppKit's authority
path. The main session services the local controls and shared AudioUp coordinator.
Capture and permission share a bounded four-worker admission population.

Stopping admission is distinct from cleanup. The original session retains
capture/permission retirement until the actual child exits, and retains its
native panel until that panel positively closes. A failed native disposal is
quarantined until process exit; it never becomes a fabricated cleanup receipt.
Process separation is crash/hang containment, not a same-user security sandbox.

## Evidence and current limits

Content-free completion separates requested capability, host selection, OS
permission and **native capture ready**. Ready means the capture helper was
configured; it is not a first-sample, speaker or host-application audibility
measurement. No device UID, source label or captured speech enters diagnostics.

Portable tests cover exact device selection, original gesture/permission
deadlines, visibility loss, mute fencing, native timestamp/callback arithmetic
and bounded ring behavior. The shared Linux process source and exact Mac Rust
capture source can be type-checked without Apple hardware. The native audio CI
job compiles the actual Objective-C sources and links the real Mac worker with
the Apple SDK. Those checks are distinct from physical microphone, TCC identity,
sleep/hotplug and live-tailnet qualification on a Mac; this change does not claim
those hardware or host-application checks have passed.
