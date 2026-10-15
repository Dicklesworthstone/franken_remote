# Goertzel over the recorder's 20 ms blocks (the namespace test's criteria):
# a tone is present when loud (>1e11) and 30x above probe bins and the other tone.
# Usage: analyze.py REC HZ1 HZ2 SWITCH_EPOCH
import math, struct, sys
PROBES = [300.0, 600.0, 750.0, 900.0, 1200.0, 1500.0]
rec, hz1, hz2, switch = sys.argv[1], float(sys.argv[2]), float(sys.argv[3]), float(sys.argv[4])
def power(b, hz):
    k = 2.0 * math.cos(2.0 * math.pi * hz / 48000.0); s1 = s2 = 0.0
    for x in b:
        s0 = x + k * s1 - s2; s2 = s1; s1 = s0
    return s1 * s1 + s2 * s2 - k * s1 * s2
def dominant(b, hz):
    t = power(b, hz)
    others = max(power(b, f) for f in PROBES + [hz1, hz2] if abs(f - hz) > 1)
    return t > 1e11 and t > 30 * others
blocks = []
data = open(rec, "rb").read()
for off in range(0, len(data) - 3847, 3848):
    (at,) = struct.unpack_from("<d", data, off)
    raw = struct.unpack_from("<1920h", data, off + 8)
    blocks.append((at, [(raw[2 * i] + raw[2 * i + 1]) / 2 for i in range(960)]))
first1 = next((at for at, b in blocks if dominant(b, hz1)), None)
first2 = next((at for at, b in blocks if at >= switch and dominant(b, hz2)), None)
print(f"blocks={len(blocks)} span_s={blocks[-1][0]-blocks[0][0]:.1f}" if blocks else "blocks=0")
print(f"first_hz1_block_epoch={first1}")
print(f"switch_to_client_detect_ms={None if first2 is None else round((first2 - switch) * 1000)}")
seen, window = 0, []
if first2:
    window = [b for at, b in blocks if first2 <= at < first2 + 2.0]
    seen = sum(dominant(b, hz2) for b in window)
    print(f"sustained_hz2_blocks={seen}/{len(window)} over 2 s after detection")
    pre = [b for at, b in blocks if first1 and first1 <= at < switch - 0.2]
    print(f"sustained_hz1_blocks={sum(dominant(b, hz1) for b in pre)}/{len(pre)} before the switch")
held = first2 is not None and first2 - switch < 2.0 and seen * 5 >= len(window) * 4 > 0
print("verdict=" + ("pass" if held else "fail"))
