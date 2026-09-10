# Browser HEVC experiment

This is an executable Phase 0 experiment for `fr-p0-browser-hevc-gex`, owned by
plan §§8.1 and 16.3. It generates synthetic HEVC through installed FFmpeg's
hardware-required VideoToolbox encoder, extracts the actual hvcC and complete
length-prefixed access units, then runs real Chrome WebCodecs and canvas readback.
It is not the browser client or a shipping media path.

## Retained result, 2026-09-10

Host: Apple M4 Pro, 14 CPU cores, 64 GB, macOS 26.2 build 25C56;
FFmpeg 8.1.2. Chrome's installed version was 153.0.8010.36; its reduced user-agent
string in the result is not used to infer support. Safari's installed version
was 26.2.

| Combination / property | Result | Evidence boundary |
|---|---|---|
| Chrome, M4 Pro, visible browser, 3072×1728 HEVC | passed | [Nine decoded pictures and canvas readbacks](results/chrome-m4-pro-20260910.json) |
| Single IDR, P after two-second idle, four-picture burst including its tail, low-cadence P | passed | No per-picture flush; original reference chain retained |
| Delta immediately after reset, then IDR/P recovery | passed | Delta raises `DataError`; recovered IDR/P readback hashes match their originals |
| Physical scanout, full physical display resolution, hardware decoder selection | not tested | Browser reports 3072×1728 logical screen and scale factor 2; canvas readback is not optical evidence |
| Native decoder surface counts/bytes, worker decoding, background/resume, long idle, text quality, transport loss | not tested | API queue counts do not include native decoder internals |
| Safari 26.2 | blocked | WebDriver session creation returned HTTP 500: `Allow remote automation` must be enabled in Safari Settings |
| Windows Chrome/Safari and other OS/browser combinations | not tested | No inference from the Mac result |

Safari's setting was not changed. The separate Windows worker was reachable
during read-only preflight (Windows 11 Home build 26200, Intel Iris Plus driver
31.0.101.2125); no browser or hardware codec test ran there.

The result's `status` applies only to its explicit `scope`: synthetic HEVC decode
and canvas readback. It does not close the parent Bead. A hardware preference
does not prove the selected decoder; the actual selected backend still needs
independent instrumentation. The clock values measure JavaScript submission to
decode callback and canvas readback in this single experiment, including startup
cost. They are not workstation input-to-photon or steady-state performance claims.

## Run on the Mac

Use a new output directory for every run. Existing files are never removed.

```sh
/opt/homebrew/bin/python3 spikes/browser-hevc/run.py \
  --output /tmp/fr-browser-new-run \
  --chrome '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome' \
  --ffmpeg /opt/homebrew/bin/ffmpeg \
  --ffprobe /opt/homebrew/bin/ffprobe \
  --width 3072 --height 1728
```

`--headless` permits diagnostic decode runs, with that limitation recorded in
the output. The runner launches a separate temporary Chrome profile, serves only
the synthetic fixture and probe over loopback, and terminates its process group
after the result or a 25-second timeout. It does not access an existing browser
profile, capture the desktop, modify permissions, install dependencies or compile
anything. Raw browser diagnostics, the generated MP4, fixture and temporary
profile remain in the chosen output directory; do not commit them. Native logs
can contain machine-specific paths. Only the sanitized result JSON is retained
here. Run compilation through RCH if extending this experiment with native code.

The generator accepts even dimensions from 64×64 through 4096×2160. The eight
encoded pictures are capped at 16 MiB total and 4 MiB per access unit. The probe
admits at most four pending pictures, bounds the decoded RGBA footprint to
4096×2160×4 bytes per frame, draws synchronously and immediately closes each
`VideoFrame`; no output frame survives its callback. This bounds application-held
frames, not unobservable codec or compositor allocations. Timeouts fail the run;
the probe never flushes to force missing output or submits extra dummy pictures.

## Bitstream and validation

The generator verifies Main 8-bit 4:2:0, matching PTS/DTS, four-byte NAL lengths,
no in-band VPS/SPS/PPS, an initial IDR (NAL type 19/20), followed by non-IDR
pictures (type 1). It derives the complete codec identifier from hvcC profile,
reversed compatibility bits, tier/level and constraint bytes. This is bounded
fixture extraction from our generated MP4, not a general hostile-media parser
or a replacement for `fr-media` admission. FFmpeg packet boundaries supply the
complete access units. Emitted fixture and probe hashes are in the result.

The implementation follows the [HEVC WebCodecs registration](https://w3c.github.io/webcodecs/hevc_codec_registration.html)
for hvcC/length-prefixed input and the [WebCodecs specification](https://www.w3.org/TR/webcodecs/)
for reset, key-chunk requirements and frame lifetimes. The browser's acceptance
and actual decode are both checked. Readback must be nonblank, the first P must
change pixels, and resetting/replaying the same IDR/P must reproduce their hashes.

Remaining qualification: Safari execution, representative workstation text and
color at the full intended physical geometry, independently observed display,
decoder hardware attribution and hidden surface bounds, worker lifecycle,
longer idle/reclamation, and real transport-loss recovery. The reset scenario
does not simulate lost network packets or qualify the production recovery path.
