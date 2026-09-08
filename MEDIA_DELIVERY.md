# Encoded-media delivery integration

`fr-media::delivery` implements the actual bounded byte-record-to-picture path over the media schemas in [PROTOCOL_MEDIA.md](PROTOCOL_MEDIA.md). It is not a network listener, codec, capture API or authority adapter. Those qualification gates remain unchanged.

## Receiver integration

Construct a `ReceivePipeline` only inside an admitted subscription, with four distinct installed bindings and a `MediaEpoch`. Call `decoder_configured` after the real decoder API and its resource admission succeed. Deliver reliable recovery chunks on the recovery channel; call `take_decodable` to obtain a complete picture, validate its HEVC syntax/reference contract, and report actual decode completion with `acknowledge_decode`. Neither configuration, byte completeness nor decode completion grants visible-readiness or control authority.

Steady-state fragments can arrive out of order. The receiver retains complete dependents until their declared previous reference has decoded. Duplicates do not allocate again; conflicting duplicates and mutable frame descriptors fence the subscription. Initial recovery chunks must be contiguous, have immutable metadata, and fit a bounded chunk count. There is no partial-picture decoder output.

Call `repair_request` when its returned `next_deadline` becomes due, and send the resulting bounded record through the admitted reliable control channel. The receiver emits one request per call, at a bounded per-picture cadence and attempt count. `tick` must also run on timers with no incoming traffic: an entirely lost final frame is announced on the reliable progress path, so loss before a static screen cannot hide forever. Repeated announcements/duplicates never extend a picture's original deadline.

A picture can exceed the local display-queue budget and still be necessary to decode newer frames. `within_display_queue_budget` expresses only that receiver-local scheduling property, not end-to-end freshness. Reference usefulness is separately bounded (at most 250 ms in the initial policy); startup recovery has its own bounded deadline. Host capture timestamps are never subtracted directly from an unsynchronized receiver clock.

`ReceivedPicture` owns an RAII memory reservation. Keep it alive while a decoder borrows its bytes; taking it out of reassembly or acknowledging decode does not return credit. A shared `MediaBudget` accounts for live pictures across replacement generations, including old decoder-held inputs. `replace` requires a newer epoch and newly allocated bindings; old completion callbacks cannot promote the new chain. Closing/replacing drops queued work but cannot falsely release externally held buffers. OS/GPU decoded surfaces require their own separate budget.

`MediaBudget` uses a short internal accounting mutex, never an OS/codec callback under a lock. Receiver storage has a fixed 12-slot maximum plus bounded per-picture payload/bitmap storage; dynamic capacities and metadata are charged before publication. Allocation failure and count/byte pressure produce typed refusals, never unbounded fallback queues.

## Remaining transport and codec work

An Asupersync task must provide actual channel admission, congestion scheduling, lifecycle cancellation, timers, and source/presentation evidence. Current wire codecs do not execute the rest of the session protocol. The native QUIC Phase 0 defects and live HEVC/OS qualification remain separate work. The deterministic receiver tests exercise real packet codecs and ownership; patterned test bytes are not labeled HEVC or live-network evidence.
