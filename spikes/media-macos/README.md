# macOS VideoToolbox codec preflight

This experiment belongs to `fr-p0-media-macos-4mj`, under plan sections 8.1–8.3
and 10.2. It exercises the installed codec before building the full
ScreenCaptureKit → encoder → decoder → native presentation path. It does not
complete that path or close the Phase 0 task.

## Reproduce

On macOS with `ffmpeg`, `ffprobe`, `python3` and GNU `gtimeout` installed:

```sh
bash spikes/media-macos/probe_videotoolbox.sh /tmp/fr-vt-new-result
```

The output directory must not exist. The script retains logs and a synthetic
HEVC stream there and deletes nothing. Each media command has a 20-second
timeout followed by a two-second forced-kill grace period. Encoding stops after
60 frames and requests a 16 MiB file-size limit, which FFmpeg can overshoot by
a packet. The header inspector enforces the 16 MiB acceptance bound. Decode
errors fail immediately, and final progress must report all 60 frames.
Failures preserve stderr in that
directory and exit nonzero. No software-encoder fallback is enabled.

The source is a moving 1920×1080 test pattern at 30 frames per second, converted
to NV12 in software. This is a desktop-sized codec workload, **not** a captured
desktop or representative text-quality workload. The installed general-purpose
FFmpeg binary is an experiment tool, not a curated shipping dependency or a
proposed production subprocess.

## Result: Apple M4 Pro, macOS 26.2 (25C56)

The final script ran on 2026-09-09 and exited 0. Installed FFmpeg was 8.1.2,
Homebrew package 8.1.2_1, built with Apple Clang 21.0.0. The retained
[environment](results/m4-pro-macos-26.2-verified/environment.txt),
[encoder log](results/m4-pro-macos-26.2-verified/encode.log),
[decoder log](results/m4-pro-macos-26.2-verified/decode.log) and
[stream inventory](results/m4-pro-macos-26.2-verified/stream-summary.json) establish:

- `hevc_videotoolbox`, `allow_sw=0`, real-time hint, Main profile, no requested
  reordering and maximum one requested reference produced 60 frames and
  2,019,326 bytes. Those are requested settings, not complete native-property
  readback. **The encoder explicitly reported that `max_ref_frames` was
  unsupported and ignored the requested value.** The summary exposes that
  result as `reference_limit_request: ignored`. FFmpeg's [reference implementation](https://ffmpeg.org/doxygen/trunk/videotoolboxenc_8c_source.html)
  maps disabled software fallback to its hardware-required encoder selection;
  that reference is not a source-provenance audit of the installed binary.
- Bitstream header inspection found VPS/SPS/PPS, one startup IDR NAL (type 20)
  and 59 subsequent type-1 VCL NALs. FFprobe reported Main, 8-bit 4:2:0,
  1920×1080, no B frames, one reference, and one I plus 59 P frames. This is
  header inventory plus FFmpeg inspection; the parser's `refs=1` output does
  not establish that the rejected encoder setting was honored. It is not the project's full HEVC subset
  admission or proof of every effective reference/DPB constraint.
- Decoding the emitted stream through VideoToolbox produced 60
  `videotoolbox_vld` frames with zero reported decode errors. The null sink
  does not present them, measure copies, or read back the native decoder's
  hardware-acceleration property. The raw elementary stream does not preserve
  the encoder's frame-rate metadata; the decoder log's inferred 25 fps is not
  a throughput or timing measurement.

The earlier `results/m4-pro-macos-26.2` run predates strict decode-count and
kill-grace checks and is not the final-script evidence. Retained logs preserve diagnostics; only trailing whitespace and terminal
progress carriage-return formatting are normalized. Synthetic video remains
in the probe's output directory rather than source control.

## Capture and qualification boundaries

The [capture permission preflight](results/m4-pro-macos-26.2-verified/capture-permission.txt)
returned false for the invoking SSH/Python execution context. It called
`CGPreflightScreenCaptureAccess` only: no permission request, desktop capture or
settings mutation occurred. This result does not establish permission status
for a future signed helper. Plan §10.2 requires stable signed helper identity
and a user-visible setup path for consent; this probe cannot substitute for it.

ScreenCaptureKit acquisition, capture-buffer ownership, GPU conversion/import,
native visible presentation, color fidelity, actual reference/DPB bounds,
per-frame encode/decode delay, first-packet latency, copy counts,
reconfiguration, long idle and cancellation remain **not tested**. No
zero-copy, latency, full hardware-decoder or workstation-support claim follows
from this result. The configured RCH fleet currently contains no declared
Darwin worker; a qualified Apple SDK build route also remains necessary for
the native helper. This script itself performs no compilation.

The macOS bead owner consumes this partial result when selecting the real
capture/codec path. Supersede this row when a stable signed helper and the full
capture-to-presentation experiment qualify the exact device/OS profile; retain
historical evidence unless the repository owner authorizes removal. The Phase 0
task and downstream gates remain open.
