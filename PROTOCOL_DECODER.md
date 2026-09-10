# Decoder configuration records (v0)

`fr-wire::decoder` implements the three bounded media-configuration records in
PROTOCOL.md sections 6 and 7. These run only on an already authenticated,
authorized, attached reliable media-config channel. They do not install a
binding, grant observation/control, or establish native decoder readiness.
The capability name is `hevc-decoder-startup`, version 1.

Every record uses the existing 24-byte FRD0 envelope. The complete record must
fit the negotiated control-message limit. Its nonzero binding identifies an
immutable media-config tuple. The 96-byte payload prefix repeats host boot,
OS session, remote session and display handle (four 128-bit values), followed by
geometry, codec configuration, subscription recovery and viewport generations
(four u64 values). Every field must equal the locally installed tuple. The
display handle is opaque and scoped to that host/OS session, not an OS pathname
or untrusted display name. Generation zero remains a valid initial generation;
it does not mean absent. No input lease applies to this observation channel.

All integers are big-endian. After the tuple:

| Kind | Payload suffix |
|---|---|
| `0x0030 DecoderConfiguration` | coded width/height and zero-offset crop width/height (4 x u32), fps (u16), CICP primaries/transfer/matrix, full-range boolean and DPB-picture cap (5 x u8), codec identifier and hvcC (each u32 length followed by bytes) |
| `0x0031 DecoderConfigured` | Empty; acknowledges the exact installed configuration after successful API configuration and resource admission, NOT a decoded frame |
| `0x0033 FirstFrameDecoded` | Frame identity (u64), decoder-local monotonic microseconds (u64); NOT host time, visibility, presentation freshness or input readiness |

Configuration travels host to viewer; acknowledgements travel viewer to host.
All datagram uses refuse. Native wire access units retain repeated parameter
sets on IDRs, so this profile declares `hev1`, not `hvc1`. The exact identifier
must subsequently be checked against parsed hvcC facts. Codecs are at most
64 bytes; hvcC at most 12,326 bytes (three 4,096-byte parameter sets plus framing).
The parser borrows both byte fields. Dimensions use the canonical protocol
limits, crop must fit, fps is 1..240, and DPB cap is 2..12 independently of the
compressed reassembly window. Supported declaration codes are primaries 1/9,
SDR transfer 1/13, matrix 1/9 and range 0/1. Actual endpoint support is narrower
unless qualified. Parsing does NOT establish bitstream validity or allocate
native surfaces; actual VPS/SPS/PPS, canonical identifier and native budgets
must be validated before configuration. Aggregate surface-memory accounting
remains the platform/resource owner's responsibility.

Seven new tests cover independent golden bytes for all three records, borrowed
payloads, every truncation, full tuple substitution, zero binding, role and
transport violations, malformed lengths/declarations, output/control ceilings
and redacted diagnostics. The full wire test selection passed 78 tests on the
pinned nightly, plus strict all-target Clippy. These are parser/source tests,
not a network/codec qualification result. Reproduce with:

```sh
cargo test -p fr-wire --locked
cargo clippy -p fr-wire --all-targets --locked -- -D warnings
```

## Implemented native startup

Source `6dcfc2b` connects these records to the real supervised native decoder and
Asupersync QUIC. Its nine integration tests include network-carried configuration,
first-IDR decoding and actual X11 readback, then a dependent picture through the
same decoder and receiver. The already installed channel/admission setup remains
an explicit fixture, not a live-tailnet claim. See [DECODER_STARTUP.md](DECODER_STARTUP.md)
for exact ownership, timeouts, supported native subset, verification provenance,
and remaining application-level integration.
