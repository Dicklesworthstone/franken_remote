# Executable v0 media record schemas

This companion instantiates the media kinds in [PROTOCOL.md](PROTOCOL.md), which remains authoritative. The `fr-wire` crate implements these byte layouts; other kinds remain unavailable to this codec. This is draft application framing, not QUIC/WebTransport, HEVC syntax, identity, or platform qualification. An adapter must supply the admitted immutable nonzero binding and correct channel/direction. All media records except RepairRequest flow host-to-viewer; RepairRequest flows viewer-to-host.

The 24-byte FRD0 header is unchanged. Integers below are big-endian. Every field appears in listed order, without alignment padding. `data` means u32 length followed by that many bytes. `reference` means u8 0 for declared IDR or u8 1 followed by the referenced u64 frame. A declared IDR still requires bitstream validation before the decoder trusts it. Unknown required extensions refuse; unknown optional flat extensions may be skipped, subject to ordered unique tags and 128 entries maximum.

| Kind | Fixed payload followed by data |
|---|---|
| 0x0034 AccessUnitFragment | frame:u64, total:u32, offset:u32, index:u32, count:u32, stride:u32, capture_us:u64, reference, data |
| 0x0032 RecoveryAccessUnit | frame:u64, total:u32, offset:u32, capture_us:u64, data |
| 0x0037 MediaProgress | frame:u64, total:u32, stride:u32, capture_us:u64, reference, observed_us:u64, observation:u8, pipeline:u8 |
| 0x0035 RepairRequest | frame:u64, count:u32, count pairs of start:u32/end:u32 |

Progress observation values: 0 unknown, 1 serviced capture, 2 OS-qualified unchanged observation. Pipeline values: 0 opening, 1 running, 2 idle, 3 failed. These are reported evidence categories, not a parser-derived assertion of source freshness. Progress repeats immutable descriptor fields and can announce an entirely lost final picture. Repair ranges are nonempty, sorted disjoint half-open fragment-index ranges within the cached descriptor's count.

Selected `MediaLimits` cap complete records by both the actual transport allowance and core C; fragments by a downward-selected maximum <=16,384; repair ranges by a downward-selected maximum <=64. Total picture bytes remain <=core A. These are count ceilings, not permission to allocate their product. The receiver additionally admits picture count and payload/metadata capacity against W/B. Reliable profiles can use C; datagram profiles must pass the actual D, not a guessed MTU. A record cap <=73 cannot support this media profile.

For fragments, count=ceil(total/stride), offset=index*stride, and chunk length=min(stride,total-offset). The sender chooses stride <=record_cap-73, including the longest reference form. Recovery has 52 bytes complete-record overhead and requires ordered contiguous chunks. A repair response is an ordinary original fragment, not a new frame or new lifetime.

Golden IDR fragment: binding 9, frame 7, three bytes `01 02 03`, stride 8, capture 42:

```text
4652443000000034000000000000002c000000090000000000000000000000070000000300000000000000000000000100000008000000000000002a0000000003010203
```

Fixtures test every truncated prefix, wrong channel/binding, malformed sizes/index/count/offset, range bounds, required/optional extensions, and content-independent diagnostic formatting. No parser scans forward after malformed reliable framing. HEVC parameter-set and reference validation remain the codec boundary's job; these byte codecs do not claim a decoded or displayed frame.
