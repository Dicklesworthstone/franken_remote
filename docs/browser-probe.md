# Local browser HEVC playback qualification

The served shell's **Test video decoding** action executes a fixed public
320 × 240 HEVC Main, 8-bit 4:2:0 sample. It loads the installed `fr-web` WASM
package, creates the production `BrowserMediaAdapter`, and uses the production
`WebCodecsHevcPlayer` to configure, admit, decode, draw and release the sample.
The native browser must execute the sequence; `isConfigSupported` alone cannot
produce a passing result. This implements the local decode-and-present part of
plan §16.3, tracked by `fr-p3-browser-qualification-hf1`.

## What runs

The sample is self-hosted with the page and contains no desktop pixels or
credentials. The test does not bootstrap a session or allocate receiver credit.
Open the installed host's HTTPS page and choose **Test video decoding**:

1. Configure the exact Rust-validated `hvcC` and canonical codec configuration.
2. Decode and draw one isolated IDR without flushing the decoder.
3. Wait one second, then decode and draw its first dependent P picture.
4. Submit a burst of three P pictures and require the burst's last picture to
   reach the canvas. Earlier pictures may be skipped for presentation, but
   every admitted picture must be released by its original Rust owner.
5. Wait another 250 milliseconds, then draw the last P picture.
6. Close the original decoder, create a new decoder configuration, and replay
   the IDR and first P picture. Their canvas hashes must match the original run.
7. Confirm zero charged access units/bytes and positive native cleanup.

Canvas readback must contain opaque, nonblank pixels, and the first P picture
must change them. Hashes remain local and are never logged. A passing result
reports the actual presentation and release counts; eight pictures are
submitted, with at least six canvas presentations and eight releases.

This result covers this small Main-profile sample, its idle/burst/reset behavior,
and canvas drawing. It does not qualify hardware acceleration, high-resolution
or high-rate performance, physical display presentation, other profiles or
pixel formats, authenticated remote viewing, tailnet transport, microphone,
clipboard or desktop input. The host still reports `desktop_available: false`,
and the page's observation and control states remain unchanged.

## Bounds and teardown

One probe owns one media adapter, at most four admitted pictures and 2 MiB of
compressed-picture custody. The test has a 12-second total deadline and a
two-second deadline for each requested picture's draw and positive release.
It never calls decoder `flush()`. At most eight per-picture metadata entries
exist. The sample and canvas dimensions are fixed.

Cancel, hidden-page events, pagehide, bfcache restore and a new connection
attempt fence the test before teardown. Native decoder and frame ownership
remain with the original Rust adapter until `reset()` confirms cleanup. A
timed-out `isConfigSupported` call cannot be cancelled, so its owner stays
retained and another test cannot allocate a replacement. Its late completion
only cleans up that original attempt. A failed native close remains retained;
**Retry cleanup** retries under the same owner. A browser-internal synchronous
stall cannot be preempted by JavaScript, but cannot produce a passing result
after the deadline when it returns. No result claims to bound hidden driver
allocations inside a defective browser implementation.

## Public fixture provenance

`web/hevc_probe_sample.js` embeds an `hvcC` record and the first six complete
length-prefixed access units of an eight-frame FFmpeg `testsrc2` sequence. Its
wire IDR includes the exact VPS/SPS/PPS from that record. Rust validates and
removes those repeated parameter sets before native `hvc1` submission. The
source was generated with FFmpeg 7.1.5 and libx265 using:

```sh
ffmpeg -nostdin -hide_banner \
  -f lavfi -i testsrc2=size=320x240:rate=30 -frames:v 8 -an \
  -c:v libx265 -preset ultrafast -tune zerolatency -pix_fmt yuv420p \
  -color_range tv -colorspace bt709 -color_trc bt709 -color_primaries bt709 \
  -x265-params 'bframes=0:ref=1:keyint=30:min-keyint=30:scenecut=0:repeat-headers=0:info=0:frame-threads=1:pools=1' \
  -tag:v hvc1 sample.mp4
ffprobe -v error -show_streams -show_packets -show_data -of json sample.mp4
ffmpeg -nostdin -v error -i sample.mp4 -f framemd5 decoded.framemd5
```

The existing extraction helpers in `spikes/browser-hevc/run.py` decode ffprobe
hex records and derive the codec string. On the tested libx265 build, the
FFmpeg color options above left SPS VUI primaries and transfer unspecified
(value 2). The fixture explicitly describes both fields as BT.709 (value 1):
within this exact 111-byte `hvcC`, bytes 87 and 88 each change from `0x40` to
`0x20`. Every other configuration byte and all six compressed picture NALs stay
bit-exact. The three corrected parameter NALs are prepended to the wire IDR,
each with a four-byte length, as required by the production admission boundary.
These offsets describe this fixed sample, not a general SPS parser.
The original unspecified VUI is rejected by the production Rust owner; its
color checks remain unchanged. No remux or regeneration is claimed for this
metadata correction.

The sample contains one IDR followed
by five P pictures, with no B frames or reordering. The emitted native `hvc1`
chunks contain no in-band parameter sets. Its `hvcC` is 111 bytes; the wire
access units are 7,858, 1,707, 2,343, 2,288, 2,411
and 2,369 bytes. The SHA-256 of the exact `hvcC` followed by these six access
units is:

```text
241e3b10a079ab3f8d946fd731fc424c25bb2b2dd104a33f41c150bf7fe16519
```

## Automated execution

`node scripts/build-web.cjs NEW_OUTPUT_DIRECTORY` includes
`web/tests/wasm_playback_probe.cjs`. That suite executes the real generated
Rust/WASM guard and resource owner against the exact packaged sample, with
explicitly simulated native WebCodecs and canvas callbacks. It covers the
successful sequence, missing output, unsupported capability, stalled native
capability discovery, cancellation, blank/unchanged/unreadable canvas output,
hidden pages, clock regression, deadline expiry and retained failed cleanup.
These tests are Rust/WASM boundary evidence. Native browser results must still
be obtained using **Test video decoding** on the actual browser/OS/GPU being
qualified; a passing simulated test is never a browser support claim.
