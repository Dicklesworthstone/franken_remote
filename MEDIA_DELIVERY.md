# Encoded-media delivery integration

`fr-media::delivery` implements the bounded encoded-picture -> byte records -> repair -> ordered-picture path over [PROTOCOL_MEDIA.md](PROTOCOL_MEDIA.md). It is not a network listener, codec, capture API, or authority adapter. Those qualification gates remain unchanged.

## Sender integration

Construct `SendCache` inside an admitted subscription with its actual transport-sized `MediaLimits`, installed channel bindings and `MediaEpoch`. Move an already encoded buffer into `push` together with capture-provided progress metadata. The first picture is a declared IDR on the reliable recovery channel. Subsequent pictures use datagrams; predicted pictures must reference the last inserted picture. A later independent IDR is allowed without resetting a healthy subscription.

Call `next_packet` only when the Asupersync transport has resource/congestion admission for another packet. It emits the reliable progress announcement before the picture's payload, then one fragment or recovery chunk into caller-owned storage. An emitted `PacketOffer` is NOT a transport acknowledgement. Retain at most the admitted bounded pending packet and check `send_by_micros` again at actual send. If an offered packet must be abandoned, repair it within the original deadline or fence/recover the affected chain; never assume that clearing an application buffer retracts bytes already submitted to a transport.

Forward incoming authenticated repair records to `queue_repair`, then ask `next_repair_packet` for one original cached fragment at a time. Actual cached fragment counts, sorted range bounds, one precharged pending repair job per cached picture, retry cadence, maximum rounds, and aggregate repair-byte allowance all apply. Repair traffic also passes the transport's normal congestion controller. Recovery-generation replacement does not replenish a session's repair-byte allowance. Retransmission never extends the original picture's retention deadline.

An announced datagram picture can retain an early repair request in its own
inline job slot. That fixed job becomes eligible once all original fragments
named in its ranges have been packetized; an unrelated tail no longer delays
repairs of an already-observed prefix hole. Requests naming unissued data remain
pending without consuming repair-byte allowance on repeated polls. The gate
uses the last validated range, not just the first, and never adds unrequested
fragments during packetization. Reliable recovery and unannounced pictures remain
ineligible. The original packet cursor and final transport-admission checks are
unchanged; packetization is not delivery evidence.

Repair requests for different pictures coexist within the admitted cache window
(default 32, hard maximum 64); a busy picture cannot discard another picture's
only request. The full inline range storage is charged as picture metadata before
capture admission, so requests allocate neither memory nor extra payload copies.
One picture still admits only one job and keeps its consumed round/cadence ledger.
At each packet, the earliest-deadline eligible reference wins, with frame order
breaking ties. Requests naming unissued originals are skipped without consuming
byte credit or delaying another eligible picture. Eviction, close and generation
replacement destroy the original picture's jobs; no allowance is replenished.
`repair_window` exercises burst losses across dependent pictures, reordered
requests, precharged capacity, unchanged deadlines and the shared byte allowance
with real cache/reassembly/wire owners and deterministic packet/clock fixtures.

A newer valid request for a picture with a pending job may coalesce into that
job's **unsent suffix**, rather than losing the receiver's last repair attempt
because the picture is busy. The bounded sorted union retains every outstanding
fragment and adds only explicitly requested work. An already emitted fragment
is reintroduced only if the newer request explicitly asks for it (for example,
a repair packet was itself lost). Adjacent ranges are normalized without
spending a round when no new fragment was requested. Duplicates/subsets remain
`RepairBusy`, without changing the pending cursor or its accounting.

Coalescing consumes the normal per-picture round and cadence allowance; it
never resets either ledger or the global byte window. A union exceeding the
negotiated range count refuses atomically. An unissued extension which would
raise the existing job's original-emission gate also refuses, so a peer cannot
postpone already eligible repairs by appending a future tail. Invalid ranges,
permanently oversized packets, clock failure and original expiry retain their
existing checks. No queued transport packet, payload ownership, hard deadline,
generation or authority is replaced. The `coalesced_repair` regressions exercise
newly discovered holes and lost repair packets through admission pauses and a
byte-window refill using the actual cache, reassembler and wire codecs. They do
not establish a live QUIC loss envelope.

The sender cache admits actual vector capacity and picture metadata, not just logical payload length. It has an explicit 32-picture default and 64-picture implementation ceiling, separate from receiver W: a 250-ms repair horizon at a 60-fps source cadence can require more than twelve sender pictures. A virtual-clock 180-frame regression exercises that distinction. It is not a throughput measurement. Expired fully emitted pictures leave the cache; an expired unsent reference fences dependent work instead of emitting a broken chain. On capacity refusal, the caller must preserve a valid dependency chain or recover, not silently skip that encoded reference.

Repair-byte exhaustion is bounded pacing, not a fatal media failure. An
otherwise valid request remains attached to its original picture, with its cursor,
round charge and deadline unchanged; `next_repair_packet` returns no packet until
the subscription-wide allowance refills. `next_deadline` exposes that refill only
when an eligible repair can still be useful before its original expiry, including
while idle. Missed windows do not accumulate a catch-up burst. A requested packet
whose full framing can never fit even one complete allowance is refused during
request admission, before consuming a job or round; it is never deferred forever
or discovered as a fatal error on the native send path. Window refill does not
bypass congestion, restore expired media, revive a closed owner, or replenish the
per-picture attempt ledger. Recovery-generation replacement still does not reset
spent subscription-wide bytes.

## Receiver integration

Construct a `ReceivePipeline` only inside an admitted subscription, with four distinct installed bindings and a `MediaEpoch`. Call `decoder_configured` after the real decoder API and its resource admission succeed. Deliver reliable recovery chunks on the recovery channel; call `take_decodable` to obtain a complete picture, validate its HEVC syntax/reference contract, and report actual decode completion with `acknowledge_decode`. Neither configuration, byte completeness nor decode completion grants visible-readiness or control authority.

Steady-state fragments can arrive out of order. The receiver retains complete dependents until their declared previous reference has decoded. Duplicates do not allocate again; conflicting duplicates and mutable frame descriptors fence the subscription. Initial recovery chunks must be contiguous, have immutable metadata, and fit a bounded chunk count. There is no partial-picture decoder output.

Call `repair_request` when its `next_deadline` becomes due, and send the resulting bounded record through the admitted reliable control channel. The receiver emits one request per call, at a bounded per-picture cadence and attempt count. `tick` must also run on timers with no incoming traffic: an entirely lost final frame is announced on the reliable progress path, so loss before a static screen cannot hide forever. Repeated announcements/duplicates never extend a picture's original deadline.

Repair scheduling distinguishes gaps before the highest received fragment from
an unseen tail that is still making progress. While unique fragments keep
arriving within the existing reorder allowance, requests cover only observed
prefix gaps. After that allowance passes without a unique fragment, requests
include the missing tail. An entirely lost announced picture still gets its
initial timed probe. Duplicates and repeated announcements cannot postpone the
soft probe; neither unique progress nor repair traffic extends the original hard
reference deadline. The retry interval, attempt count, range limit and byte
budgets remain unchanged. `next_deadline` follows the same rule rather than
repeatedly waking at a past probe time during contiguous progress. The two fixed
progress fields are charged through the existing assembly metadata accounting.

The `progressive_repair` tests exercise paced arrivals, interior gaps, duplicate
traffic, complete final-picture loss and hard reference expiry with actual
sender/receiver/wire owners and synthetic compressed bytes. Prefix tests also
reconstruct a 40-picture chain with lost originals and lost repairs, retaining
upstream early-request waiting, limits and expiry tests unchanged. They are not native
QUIC loss-envelope or hardware decode/presentation qualification.

A picture can exceed the local display-queue budget and still be necessary to decode newer frames. `within_display_queue_budget` expresses only that receiver-local scheduling property, not end-to-end freshness. Reference usefulness is separately bounded (at most 250 ms in the initial policy); startup recovery has its own bounded deadline. Host capture timestamps are never subtracted directly from an unsynchronized receiver clock.

`ReceivedPicture` owns an RAII memory reservation. Keep it alive while a decoder borrows its bytes; taking it out of reassembly or acknowledging decode does not return credit. A shared `MediaBudget` accounts for live pictures across replacement generations, including old decoder-held inputs. `replace` requires a newer epoch and newly allocated bindings; old completion callbacks cannot promote the new chain. Closing/replacing drops queued work but cannot falsely release externally held buffers. OS/GPU decoded surfaces require their own separate budget.

`MediaBudget` uses a short internal accounting mutex, never an OS/codec callback under a lock. Receiver storage has a fixed twelve-slot maximum plus bounded per-picture payload/bitmap storage; dynamic capacities and metadata are charged before publication. Fixed object storage is separate from the reported dynamic reservation counters. Allocation failure and count/byte pressure produce typed refusals, never unbounded fallback queues.

## Run the real HEVC delivery check

With the repository-pinned Rust toolchain and an installed FFmpeg/ffprobe build containing the test-only `libx265` encoder:

```bash
python3 scripts/verify_hevc_delivery.py --output-dir /tmp/fr-hevc-review-unique
python3 scripts/verify_hevc_delivery.py --frames 240 --output-dir /tmp/fr-hevc-review-long-unique
```

Each output directory must be new. The script retains the exact commands, tool versions, synthetic MP4, hvcC configuration, access-unit corpus, delivery statistics, independent decode hashes and stderr. A subprocess error, unavailable tool, corrupt/truncated input, failed repair, or hash mismatch is a failure, not a skipped success. No end-user screen or clipboard data is captured.

The lane encodes a synthetic 640x360 Main 8-bit 4:2:0 sequence with periodic IDRs and no B frames, extracts canonical length-prefixed access units, and moves them through the production sender, packet codec and receiver. Its bounded four-packet impairment queue reverses packet order, duplicates delivered video packets, drops selected video packets, and drops every video packet of the final frame. The receiver requests missing fragments from the sender cache. The lane compares every recovered access unit and then independently decodes the source and delivered streams with FFmpeg, requiring equal frame hashes and the expected frame count.

The Rust diagnostic models a decode completion after exact byte comparison so the delivery state machine can proceed; independent software decoding occurs in the Python lane afterwards. This is deliberately not a shipping decoder implementation or an assertion that an OS decoder callback occurred inside the Rust example. Its fixture format (`FRHEVC01`, bounded frame count, per-frame IDR flag/length/bytes) is offline test framing, not a second network protocol. Parameter extraction is for the controlled fixture, not a production SPS/PPS security validator. Capture/presentation and network timestamps remain unqualified; virtual-clock impairment does not establish a physical latency result.

## Remaining transport and codec work

An Asupersync task must provide actual channel admission, congestion scheduling, lifecycle cancellation, timers, receiver-credit integration, and source/presentation evidence. The wire codec does not yet execute the rest of the session protocol. The existing native QUIC qualification work and live HEVC/OS adapters remain separate requirements. The Linux native path now admits bounded exact parameter-set/profile/DPB configuration before decoder setup; [its startup evidence and remaining limits](PRESENTATION_FRESHNESS.md#exact-native-decoder-startup) are separate from this offline delivery harness. Normative network startup, native hardware and callback qualification remain outstanding. The tests establish encoded-media delivery and independent software-decode preservation, not a completed remote desktop or a passed live-transport/hardware phase gate.
