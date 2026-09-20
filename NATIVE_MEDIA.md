# Native Linux media path

`fr-native` is the named, thread-confined FFI boundary for direct FFmpeg and X11
calls. Enable `linux-media` explicitly. No FFmpeg process, network downloader,
new Rust runtime, generated-binding dependency, or silently selected software
encoder is used. The small project-owned C shim is compiled against the selected
system SDK; foreign types never cross the Rust media/wire API.

The first path is explicit **CPU staging**: X11 root capture -> BGRA -> libswscale
BT.709 conversion -> HEVC encoder -> existing FRD0 sender/receiver -> HEVC software
decoder -> BGRA -> X11 presentation. NVENC and VAAPI encoder selections exist,
including real VAAPI hardware-frame allocation/upload; they refuse unavailable
hardware rather than switching to x265. Hardware support is not certified by
source presence. Software encoding requires `SoftwareExplicit` or the corresponding
example flag. This developer path does not yet satisfy the GPU-resident target.

Owners are neither Send nor Sync. Run them in the supervised media process, not
in a broker/authority task. Xlib can terminate a process on display-server failure;
process supervision and authenticated bounded IPC remain integration obligations.
The native libraries themselves are not memory-safe or sandboxed by these wrappers.
Submitted Rust pixels are copied before return. Encoder and decoder contexts own
reference-counted native inputs; `av_new_packet` supplies required zeroed padding.
Send/receive distinguish backpressure and EOF. Queue capacity is four inputs;
geometry and generation checks precede submission. Display resize is a refusal,
not permission to reinterpret stale dimensions. Diagnostic formatting omits pixels;
foreign library logs are suppressed inside the media process in favor of typed
categories, not reflected into protocol messages.

**Decoder boundary:** the safe HEVC guard validates exact canonical hvcC,
VPS/SPS/PPS, coded/cropped geometry, color and DPB demands before native decoder
configuration. It freezes parameter identities and separately validates every
complete AU before submission; configuration alone creates no reference history.
The private presentation worker requires this configuration before acknowledging
API readiness. See [the startup evidence](PRESENTATION_FRESHNESS.md#exact-native-decoder-startup).
This is a deliberately narrow baseline, not CABAC validation, a security sandbox,
hardware qualification or an authenticated network session.

## Build and reproduce

Install a C compiler, pkg-config, FFmpeg development packages (`libavcodec-dev`,
`libavutil-dev`, `libswscale-dev`), `libx11-dev`, and the system `libxdamage1` runtime. The x265 test requires an FFmpeg
build with that explicit software encoder. For a controlled matching SDK, set both
`FR_NATIVE_INCLUDE_DIR` and `FR_NATIVE_LIBRARY_DIR`; neither directory is downloaded
by the build. Cross builds without a qualified sysroot refuse.

```sh
cargo test -p fr-native --features linux-media
xvfb-run -a -s '-screen 0 640x360x24 -nolisten tcp' cargo run -p fr-native --features linux-media --example native_roundtrip -- --software-explicit
```

The example performs 12 changing-frame cycles, verifies root capture against the
submitted source pattern, feeds the actual delivery owners, decodes each delivered
access unit using libavcodec, and compares X11 window readback to decoded BGRA.
Decode completion is acknowledged only after actual decoder output. Source patterns
are synthetic; X11, codec calls, and readback are real. A passed Xvfb run is neither
physical-display/optical input-to-photon evidence nor GPU qualification.

On September 8, 2026 this path passed locally using the repository-pinned nightly,
Debian FFmpeg 7.1.5 and a 640x360x24 Xvfb screen. Three native unit tests passed,
including forced-IDR and encode/decode identity checks. NVENC/VAAPI, Windows/macOS,
real compositor permissions, full host/client integration, and latency remain
unqualified. System-library developer builds are not the curated signed distribution;
FFmpeg/x265 provenance and licensing remain packaging requirements.

## Curated decoder-only SDK experiment

[`native/build_ffmpeg.py`](native/build_ffmpeg.py) consumes a supplied FFmpeg
7.1.5 archive pinned by [`native/linux-ffmpeg.json`](native/linux-ffmpeg.json).
It refuses an existing output directory and builds shared avcodec/avutil/swscale
with only the HEVC decoder registered; no encoder or FFmpeg executable is built.

```sh
python3 native/build_ffmpeg.py /path/to/ffmpeg-7.1.5.tar.xz /new/output/directory
```

Use its `sdk/include` and `sdk/lib` with the paired SDK variables above.
The recipe uses a fixed configured prefix `/sdk` and `make install DESTDIR=<output>`:
installation remains under `<output>/sdk`, never the global `/sdk`. The manifest
records both configure and install commands. Generated pkg-config files retain
the configured `/sdk` prefix; use the paired explicit SDK variables above, or
configure the consuming build's pkg-config sysroot for the staging root.
On September 17, 2026, two clean builds on the same Linux builder at the same
absolute prefix produced identical hashes for all three libraries. A compiled
Rust probe normalized a real HEVC Main IDR, passed `HevcGuard`, configured
`HevcDecoder`, submitted it, and asserted frame identity, 64x64 geometry and
all 4096 opaque black BGRA pixels. The offline sample generator was system
FFmpeg/libx265, not a dependency of the stripped SDK. An initially incorrectly
tagged sample was refused with `ColorMismatch`; validation was not weakened.

A subsequent three-frame stream (one IDR and two predicted frames) passed the
same Rust boundary, with frame identity, geometry and every pixel checked for
black, white and gray output. These uniform synthetic frames do not qualify
representative desktop content, loss recovery, or presentation.

The archive's detached release signature passed GPG verification with fingerprint
`FCF986EA15E6E293A5644F10B4322F04D67658D8`, matched against FFmpeg's
[official release-key publication](https://ffmpeg.org/download.html).
This authenticates the source under that HTTPS-published key; it is not package
signing or independent web-of-trust certification. The recipe still checks only
the pinned archive digest.

The original recipe embedded its temporary install prefix in FFmpeg's configuration
string and data directories. Different prefixes produced different library hashes.
The fixed recipe preserves `/sdk` and stages with `DESTDIR`, without rewriting
generated metadata. Two fresh RCH builds on hz3 in different build/staging roots
produced identical hashes for all three libraries and identical `config.h` files.
Both resulting SDKs passed the existing single-IDR and I-P-P Rust probes with
`ldd` confirming their library paths. This proves same-builder reproducibility
across staging roots, not across different configured prefixes or builders.
The evidence record retains the original failure and the new hashes separately.
Upstream compiler warnings remain in build logs; they were not suppressed or
adjudicated, and a successful build does not establish media safety.

The existing `native_roundtrip --software-explicit` compiled and loaded this
SDK but returned `Unavailable`: this profile deliberately has no encoder.
The successful decoder probes do not turn that full roundtrip
into a pass. See the [evidence record](native/ffmpeg-7.1.5-linux-x86_64.evidence.json)
for hashes and scope. Cross-builder reproducibility, protected-path loading,
hardware encoding/decoding, presentation, signing and distribution review remain
unqualified. Test-time `LD_LIBRARY_PATH` binding is not a protected installation.


## Damage-aware CPU capture

The capture worker now enables DAMAGE 1.x on its **original X11 root capture
connection**. A synchronized, empty damage state can reuse the last successfully
encoded snapshot instead of allocating and reading another full BGRA image.
This is evidence about the X drawable in this explicit CPU-staged profile, not
optical presentation, GPU overlays, another compositor, or client visibility.
No input grant or presentation acknowledgement is derived from it.

The observer requests coalesced nonempty notifications, retains one dirty bit,
and bounds event draining at 128 events. It clears damage **before** the next
readback, never after capture/encode: changes during a pending encode remain
pending for the next capture. Only a completed, validated encoded frame becomes
the reference. Repainted identical pixels still compare exactly and emit no HEVC.
An absent server extension returns `false` from `enable_damage_tracking()` and
retains full pixel readbacks; failed observations do not become idle evidence.
The explicit system ABI is `libXdamage.so.1`, with no downloaded native library,
extra runtime, or reopened display connection.

Full pixel verification is mandatory at least once per 250 ms of requested
capture activity, independently bounded by the parent's monotonic timestamps
and the worker's real elapsed time. Force-IDR and unconditional captures always
read back and encode; damage never suppresses recovery. Capture cadence remains
owned by the existing host scheduler. `CaptureStats` reports actual readbacks,
damage observations, and encoded submissions without retaining screen content.
Selected RandR-monitor capture still uses its full-readback path in this slice.

`cargo test -p fr-native --features linux-media --test damage_capture` exercises
real Xvfb servers and the explicit software HEVC encoder, including a distinct
media-worker process. Seven tests passed locally with nightly-2026-08-31 and the
Debian FFmpeg 7.1.5 SDK/runtime. The focused harness was compiled from the exact
production libraries using rustc because compiling the workspace's test-only
Asupersync 0.5 dependency was killed by the container memory limit. Native library
and worker builds and strict Clippy passed; this is not a full-workspace, live
tailnet, GPU, physical-display, or compositor qualification result. This advances
plan sections 11.3/11.4 and `fr-p1-frame-pipeline-am1`; it does not close that gate.
