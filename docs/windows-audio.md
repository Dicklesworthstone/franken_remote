# Windows client playback

The Windows desktop client can receive an approved host playback stream. Enable
the session's audio option or pass `--audio` to `fr-windows view`. Local mute and
volume affect only received playback; they grant no capture, microphone, input,
clipboard or file authority. Playback remains off for a connection which did
not request it, and the host must independently offer and approve AudioDown.

The matching `fr-opus-worker.exe` must be installed beside both Windows entry
points. The release package and per-user installer include it, along with
`OPUS-LICENSE.txt`. Missing workers produce a local refusal; there is no codec
download, PATH search, or in-process decoder fallback at connection time.

## Native behavior

The session uses the same bounded output proxy as Linux and macOS. Its eight
fixed-size packet slots retain original arrivals. Construction, device calls,
decoding and cleanup stay off the session executor. The one output worker slot
is released only after actual native and child retirement, so a stuck foreign
call cannot accumulate new workers across reconnects. Configuration admission
requires both an advancing WASAPI device clock and the configured Opus child.

Each generation pins the locally selected default render endpoint. The audio
engine accepts an explicit 48 kHz mono/stereo PCM16 stream and owns device-rate
conversion. Endpoint changes, suspension, inaccurate or stale device clocks,
and unsupported buffer sizes retire the generation. Native time is read from
`IAudioClock::GetPosition` and `GetFrequency`; elapsed wall time never becomes
synthetic consumed samples.

The original decoded-frame device slot and source deadline are rechecked after
the native buffer copy, immediately before `IAudioRenderClient::ReleaseBuffer`.
A fixed lead includes the native stream latency and a 5 ms scheduling margin,
and its largest drift-corrected packet must fit the 40 ms consumed-to-tail bound.
Every silence gap and PCM frame has an exact stream position; an empty or
inconsistent native queue retires rather than rebasing the stream on invented
engine silence. Windows interrupt and unbiased time also retain the original
suspend baseline. Devices whose latency or buffer requirements exceed these
limits refuse playback rather than queue late sound. Stop, revocation and disconnect fence the worker and discard queued sound
with Stop/Reset. A committed native write is counted as a submission, not proof
of audibility; cleanup remains incomplete when native disposal is uncertain.

The private Opus process uses the existing bounded protocol and decoder budgets.
Its supervisor pins the protected local worker image and its directory ancestry,
authenticates both endpoints of a logon-scoped private byte pipe, and checks the
original setup/decode deadline between every partial read and write. An explicit
inherited-handle list contains only that pipe and NUL stderr. A creation-time Job
attribute applies kill-on-close, one active process and a 256 MiB process-memory
ceiling before the child starts. The child verifies its handles, original parent
and Job before receiving codec input. Retirement requires both the original
process exit and an empty original Job. This is process containment, not a
Windows security sandbox claim; see [the process boundary](../NATIVE_OPUS_PROCESS.md).

## Building and checking

Provide an absolute `FR_NATIVE_OPUS_LIBRARY_DIR` containing a trusted static
`libopus.a` for the GNU target or `opus.lib` for MSVC. The release workflow and
PowerShell source installer provision the pinned MSYS2 GNU package
`mingw-w64-x86_64-opus-1.6.1-2`, verified against SHA-256
`fae981c518ed1ff2c5264eb5a0d590ec1d2ebb222defb2253ca85b2154fd8c04`.
The source installer also accepts an explicitly supplied GNU SDK with its
`COPYING` license. Cargo's build script never downloads native code.

```powershell
$env:FR_NATIVE_OPUS_LIBRARY_DIR = 'C:\trusted-opus-sdk\lib'
cargo build --locked -p fr-windows --bin FrankenRemote --bin fr-windows
cargo build --locked -p fr-native --no-default-features --features windows-opus-process --bin fr-opus-worker
cargo test --locked -p fr-native --no-default-features --features windows-audio --test opus_process_windows
cargo test --locked -p fr-native --no-default-features --features windows-audio --lib
```

Install the client, worker and codec license together. The portable output
policy tests exercise the same final checks as the native sink. The shared
worker tests cover bounded packet custody, acknowledgement, revocation and
retirement. On Windows the real process tests compare decoded PCM/PLC against
the local codec, reject malformed packets, enforce four-child admission, and
require actual reaping. Native pipe tests exercise the production Win32 boundary.

An ordinary Linux test or Windows cross-check does not qualify an audio endpoint.
Windows hardware acceptance still requires audible playback, mute/volume,
endpoint replacement, device loss, suspend/resume, disconnect during native
work, and process termination under the installed package. Record the exact
endpoint, Windows version and build used for that qualification.

API references: [WASAPI device position](https://learn.microsoft.com/en-us/windows/win32/api/audioclient/nf-audioclient-iaudioclock-getposition),
[render-buffer commit](https://learn.microsoft.com/en-us/windows/win32/api/audioclient/nf-audioclient-iaudiorenderclient-releasebuffer),
[named-pipe wait modes](https://learn.microsoft.com/en-us/windows/win32/ipc/named-pipe-type-read-and-wait-modes),
and [nested Job queries](https://learn.microsoft.com/en-us/windows/win32/api/jobapi2/nf-jobapi2-queryinformationjobobject).
