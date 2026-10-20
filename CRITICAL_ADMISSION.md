# Critical admission under frame-metadata pressure

`QuicRecords::send_prepared` now prevents repeated `Progress` and `PresentedState`
records from consuming the entire critical allowance of a connection with an
outbound critical `SessionControl` route. The test is by validated **record kind**,
not stream: the viewer's presented reports and renewal responses share one stream.
A per-lane reserve would restrict the response along with the metadata.

At the default 16-record / 8192-byte policy, new metadata stops at 14 retained
critical records or 7680 retained critical bytes. The remaining two records and
512 bytes are within the existing allowance, not additional storage. Challenges,
responses, input, tickets, attachment and terminal records still use the original
full critical budget. Other non-metadata critical traffic can consume this room;
this is protection from frame-metadata saturation, not guaranteed renewal delivery.

Both limits count **all** retained critical work, including records already staged
in an unacknowledged native epoch. No count is reclaimed at flush, admission,
batching, or a partial acknowledgement. Existing native-emptiness retirement is
unchanged. The guard allocates nothing and adds no queue or per-record ledger.

Small policies reserve at most one quarter of each configured limit, capped at
two records and 512 bytes. A single otherwise-valid record can start an entirely
empty critical flight even when it is larger than the reduced byte cap. This
preserves whole-record progress for small/custom profiles; it is an explicit
exception to the byte reserve, never permission to exceed the original allowance.
Standalone transports without an outbound critical session-control lane, bulk
records, and datagrams retain their prior admission behavior.

The check follows framing, installed-route and total-budget validation and precedes
allocation. Refusal is ordinary backpressure: retry the same bytes and absolute
send-by, not a newly dated record. Original authority and prepared-record checks,
retirement checks, sender/receiver deadlines, stream FIFO, congestion control and
flow credit remain in force. A response may still wait behind earlier bytes or
an acknowledgement epoch on its stream. Nothing grants input or extends a lease.

## Evidence and remaining scope

Six added pure Rust tests cover exact kind classification, the installed-lane
condition, both resource boundaries, repeated refusal, small policies and arithmetic
faults. The earlier critical-epoch regression is corrected to compare total retained
charges with critical charges when its traffic is entirely critical; total retained
usage has never meant bulk-only usage.

Local verification for this increment is source-hash reconstruction, patch/whitespace
validation and an independent Python arithmetic boundary model. **Rust compilation,
the Rust tests, rustfmt and Clippy were not executed here: no Rust toolchain is
available.** Committed-source CI results must be checked separately.

This implements the record-class reservation suggested in the diagnostic comments
on `fr-6hbo`; that bead's original clipboard-projection failure is already closed.
It does not complete `fr-00nc`'s per-record acknowledgement accounting or `fr-xfvs`'s
lossy recovery-speed qualification. No WAN latency or sustained-control improvement
is claimed, and no recovery, freshness or authority deadline is relaxed.
