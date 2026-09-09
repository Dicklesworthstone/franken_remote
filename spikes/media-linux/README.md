# Linux hardware-media preflight

This bounded installed-driver experiment belongs to `fr-p0-media-linux-98h`
and plan sections 8.3, 10.1 and 11.1. It starts VAAPI and requests two synthetic
1080p HEVC Main frames. It does not capture a desktop, use PipeWire buffers,
verify an IDR, decode or present. The installed FFmpeg executable is a
qualification tool, not a proposed production subprocess or shipping bundle.

Run on the machine containing the render device:

```sh
bash spikes/media-linux/probe_vaapi.sh /dev/dri/renderD128
```

The command preserves FFmpeg stderr and its process exit status; timeout exits
124. A successful exit would establish only this small encode preflight, not
the Phase 0 media path. There is no software fallback. No portal session or
desktop consent request is made.

## Observed 2026-09-09

The tested desktop had Intel Haswell-ULT graphics (PCI 8086:0a26, revision 09),
the i915 kernel driver and `/dev/dri/renderD128`. The installed environment was:

| Component | Version or state |
| --- | --- |
| Kernel | 6.19.8-arch1-3-surface |
| Hyprland | 0.56.2-1 |
| PipeWire | 1:1.6.8-1; active |
| Desktop portal | 1.22.1-2; active; ScreenCast interface version 6 |
| Hyprland portal backend | 1.4.1-1; active |
| Mesa | 1:26.2.1-1 |
| libva | 2.24.1-1 |
| FFmpeg | n9.0.1; libavcodec 63.1.101, libavutil 61.1.101 |
| Intel VAAPI driver packages | `intel-media-driver` and `libva-intel-driver` absent |

The installed FFmpeg lists `hevc_vaapi`, but the actual command exited **251**
(FFmpeg's signed diagnostic was -5) before encoder configuration. VAAPI tried
`iHD_drv_video.so` and `i965_drv_video.so`; both were absent, and both driver
opens failed. The [retained stderr](vaapi-preflight.stderr) records device
initialization failure. No frames were encoded. This does not establish
whether this GPU can encode or decode the required HEVC subset with a qualified
driver. No packages, driver settings or desktop permissions were changed.

| Qualification row | Result |
| --- | --- |
| This Hyprland profile: installed VAAPI initialization | failed |
| This Hyprland profile: portal → PipeWire → GPU copy → HEVC → decode → present | blocked before encoding |
| GNOME | not tested |
| KDE | not tested |
| NVENC | not tested |

The Linux media bead remains in progress. Its owner consumes this negative
result before attempting the full path: a usable, qualified driver/device
profile is required first. Then qualify stream identity, DMA-BUF ownership and
fences, initialized padding, real HEVC output/configuration, hardware decode,
native presentation and timing/copy measurements separately on GNOME and KDE.
Supersede this row when that exact profile is retested; this preflight cannot
satisfy the downstream Phase 0 gate.
