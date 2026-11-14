# Bounded native viewer cadence

Plan sections 11.2 and 19.3 require bounded diagnostic metadata as well as bounded
media. The native Windows viewer previously retained one `u64` per drawn-frame
interval for the whole invocation, then cloned and sorted that history when
reporting completion. Its memory and exit-time work grew with session length.

Windows now uses the same fixed-size `fr_client::cadence::FrameCadence` summary
as macOS. The existing `fr_native::macos::cadence` public path re-exports that
implementation. Recording a gap performs no allocation; report construction
scans at most 251 buckets. The summary occupies at most 2,080 bytes, independently
of the number of frames or viewer resizes.

## Measurement meaning

These are intervals between successful native compositor submissions, not
measurements of physical refresh, capture latency or input-to-photon latency.
An unchanged desktop intentionally submits fewer pictures, so its long gaps do
not by themselves establish poor moving-content performance.

`frame_interval_ms` in the Windows completion document retains `p50`, `p95`,
`p99` and `max`, and adds:

- `percentile_kind: "upper_bound"`: the percentile values are conservative
  nearest-rank bounds, not exact sorted samples;
- `bucket_ms: 1`: gaps below 250 ms use one-millisecond buckets;
- `samples`: the number of intervals represented by the summary.

The overflow bucket reports the actual longest interval, rounded up to whole
milliseconds, rather than clipping longer stalls at 250 ms. `max` keeps the
exact recorded microsecond value converted to milliseconds. If fewer than two
frames were drawn, the four timing values are `null`; a missing measurement is
not a measured zero. Windows keeps its accumulated invocation-wide summary,
resets the previous-draw timestamp on reopen, and reports the reopen delay
separately; macOS keeps its existing per-attempt scope.

Counts use `u64` and total duration uses `u128`, so long durations cannot
saturate into a misleading mean. At the `u64` sample-count ceiling the original
complete prefix is retained unchanged; no counter wraps or histogram mass is
silently lost.

## Verification boundary

The shared production module has deterministic tests for bucket edges and all
1,001 permille ranks against exact sorted observations, long-session fixed
storage, rare stalls, empty/zero-duration data, extreme durations and counter
exhaustion. Native presentation, driver behavior and long-running physical
Windows/macOS sessions remain separate platform qualification.
