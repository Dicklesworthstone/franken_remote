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
`libavutil-dev`, `libswscale-dev`) and `libx11-dev`. The x265 test requires an FFmpeg
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
