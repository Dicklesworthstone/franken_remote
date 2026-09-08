#!/usr/bin/env python3
"""Exercise production media delivery with HEVC and independent software decode.

This is an opt-in offline verification lane, not a shipping FFmpeg integration,
network benchmark, hardware test, or authority adapter. Artifacts are retained
in a newly created caller-selected directory; existing files are not replaced.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import platform
from pathlib import Path
import shutil
import struct
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[1]
MAX_AU = 16 * 1024 * 1024
MAGIC = b"FRHEVC01"


def command(argv: list[str], directory: Path, name: str, timeout: int = 120) -> bytes:
    (directory / f"{name}.command.json").write_text(json.dumps(argv) + "\n")
    with (directory / f"{name}.stdout").open("xb") as stdout, (directory / f"{name}.stderr").open("xb") as stderr:
        result = subprocess.run(argv, cwd=ROOT, stdin=subprocess.DEVNULL, stdout=stdout, stderr=stderr, timeout=timeout, check=False)
    if result.returncode:
        raise RuntimeError(f"{name} failed with exit {result.returncode}; stderr retained in {directory}")
    output = directory / f"{name}.stdout"
    if output.stat().st_size > 16 * 1024 * 1024:
        raise RuntimeError(f"{name} output exceeded verification limit")
    return output.read_bytes()


def unhex(dump: str) -> bytes:
    chunks = []
    for line in dump.splitlines():
        if ": " in line:
            address, payload = line.split(": ", 1)
            if len(address) != 8 or any(c not in "0123456789abcdefABCDEF" for c in address):
                raise ValueError("invalid ffprobe hexadecimal address")
            chunks.append(bytes.fromhex(payload.split("  ", 1)[0]))
    return b"".join(chunks)


def nals(au: bytes) -> list[bytes]:
    """Parse the known test encoder's four-byte length prefixes, not HEVC syntax."""
    units, pos = [], 0
    if not 0 < len(au) <= MAX_AU:
        raise ValueError("test AU exceeds limits")
    while pos < len(au):
        if len(au) - pos < 4:
            raise ValueError("truncated NAL length")
        size = int.from_bytes(au[pos:pos + 4], "big")
        pos += 4
        if size < 2 or size > len(au) - pos or len(units) >= 1024:
            raise ValueError("invalid NAL length/count")
        nal = au[pos:pos + size]
        pos += size
        if nal[0] & 0x80 or ((nal[0] & 1) << 5 | nal[1] >> 3) or nal[1] & 7 != 1:
            raise ValueError("encoder emitted invalid/multilayer/temporal NAL header")
        units.append(nal)
    return units


def parameters(hvcc: bytes) -> list[bytes]:
    """Extract bounded parameter arrays from this fixture's actual hvcC record."""
    if len(hvcc) < 23 or hvcc[0] != 1 or hvcc[21] & 3 != 3 or len(hvcc) > 65536:
        raise ValueError("unexpected decoder configuration record")
    pos, units, kinds = 23, [], set()
    for _ in range(hvcc[22]):
        if len(hvcc) - pos < 3:
            raise ValueError("truncated hvcC array")
        kind = hvcc[pos] & 63
        count = int.from_bytes(hvcc[pos + 1:pos + 3], "big")
        pos += 3
        if count > 64:
            raise ValueError("excess hvcC parameter count")
        for _ in range(count):
            if len(hvcc) - pos < 2:
                raise ValueError("truncated hvcC parameter length")
            size = int.from_bytes(hvcc[pos:pos + 2], "big")
            pos += 2
            if size < 2 or size > len(hvcc) - pos:
                raise ValueError("invalid hvcC parameter length")
            unit = hvcc[pos:pos + size]
            pos += size
            if unit[0] >> 1 & 63 != kind:
                raise ValueError("hvcC array kind mismatch")
            if kind in (32, 33, 34):
                units.append(unit)
                kinds.add(kind)
    if pos != len(hvcc) or kinds != {32, 33, 34}:
        raise ValueError("incomplete/trailing hvcC configuration")
    return units


def write_corpus(path: Path, packets: list[tuple[bool, bytes]]) -> None:
    with path.open("xb") as file:
        file.write(MAGIC + struct.pack(">I", len(packets)))
        for idr, au in packets:
            file.write(struct.pack(">BI", idr, len(au)) + au)


def read_corpus(path: Path) -> list[tuple[bool, bytes]]:
    with path.open("rb") as file:
        if file.read(8) != MAGIC:
            raise ValueError("delivered corpus magic mismatch")
        count_bytes = file.read(4)
        if len(count_bytes) != 4:
            raise ValueError("truncated delivered corpus")
        count = int.from_bytes(count_bytes, "big")
        if not 1 <= count <= 1000:
            raise ValueError("delivered frame-count limit")
        packets = []
        for _ in range(count):
            record = file.read(5)
            if len(record) != 5:
                raise ValueError("truncated delivered picture header")
            key, size = struct.unpack(">BI", record)
            if key > 1 or not 0 < size <= MAX_AU:
                raise ValueError("invalid delivered picture header")
            au = file.read(size)
            if len(au) != size:
                raise ValueError("truncated delivered picture")
            packets.append((bool(key), au))
        if file.read(1):
            raise ValueError("trailing delivered corpus bytes")
        return packets


def annexb(path: Path, config: list[bytes], packets: list[tuple[bool, bytes]]) -> None:
    with path.open("xb") as file:
        for unit in config:
            file.write(b"\0\0\0\1" + unit)
        for _, au in packets:
            for unit in nals(au):
                file.write(b"\0\0\0\1" + unit)


def hashes(path: Path) -> list[str]:
    return [line.rsplit(",", 1)[1].strip() for line in path.read_text().splitlines() if line and not line.startswith("#")]


def run(directory: Path, frames: int) -> dict:
    for executable in ("ffmpeg", "ffprobe", "cargo", "rustc"):
        if shutil.which(executable) is None:
            raise RuntimeError(f"blocked: required verification executable {executable} is unavailable")
    directory.mkdir(mode=0o700, parents=True, exist_ok=False)
    versions = {exe: command([exe, "-version" if exe.startswith("ff") else "--version"], directory, f"version-{exe}").decode().splitlines()[0] for exe in ("ffmpeg", "ffprobe", "cargo", "rustc")}
    mp4 = directory / "source.mp4"
    command(["ffmpeg", "-hide_banner", "-nostdin", "-v", "error", "-f", "lavfi", "-i", "testsrc2=size=640x360:rate=30", "-frames:v", str(frames), "-an", "-c:v", "libx265", "-preset", "fast", "-tune", "zerolatency", "-pix_fmt", "yuv420p", "-x265-params", "pools=1:frame-threads=1:bframes=0:ref=1:keyint=30:min-keyint=30:scenecut=0:open-gop=0:repeat-headers=0:aud=1:log-level=error", "-tag:v", "hvc1", "-n", str(mp4)], directory, "encode")
    probe = json.loads(command(["ffprobe", "-v", "error", "-select_streams", "v:0", "-show_packets", "-show_streams", "-show_data", "-show_entries", "stream=codec_name,profile,width,height,pix_fmt,extradata:packet=data,flags", "-of", "json", str(mp4)], directory, "probe"))
    stream = probe["streams"][0]
    if (stream["codec_name"], stream["profile"], stream["width"], stream["height"], stream["pix_fmt"]) != ("hevc", "Main", 640, 360, "yuv420p"):
        raise ValueError("unexpected fixture profile/geometry")
    hvcc = unhex(stream["extradata"])
    config = parameters(hvcc)
    (directory / "configuration.hvcc").write_bytes(hvcc)
    packets = []
    for packet in probe["packets"]:
        au = unhex(packet["data"])
        kinds = [unit[0] >> 1 & 63 for unit in nals(au)]
        vcl = [kind for kind in kinds if kind < 32]
        if not vcl or any(kind in (32, 33, 34) for kind in kinds) or any(16 <= kind < 24 and kind not in (19, 20) for kind in vcl):
            raise ValueError("unexpected in-band configuration or non-IDR random access")
        idr = all(kind in (19, 20) for kind in vcl)
        if idr != ("K" in packet["flags"]):
            raise ValueError("packet key flag disagrees with emitted IDR NAL type")
        packets.append((idr, au))
    if len(packets) != frames or not packets[0][0] or sum(key for key, _ in packets) < 2:
        raise ValueError("fixture must include exact frame count and repeated IDRs")
    source = directory / "source.frhevc"
    delivered = directory / "delivered.frhevc"
    write_corpus(source, packets)
    stats = json.loads(command(["cargo", "run", "--offline", "--locked", "-p", "fr-media", "--example", "hevc_delivery", "--", str(source), str(delivered)], directory, "deliver"))
    recovered = read_corpus(delivered)
    if recovered != packets or stats["frames"] != frames or stats["dropped_packets"] <= 0 or stats["repair_packets"] <= 0:
        raise ValueError("delivery byte equality/loss/repair gate failed")
    for name, contents in (("source", packets), ("delivered", recovered)):
        bitstream = directory / f"{name}.h265"
        annexb(bitstream, config, contents)
        command(["ffmpeg", "-hide_banner", "-nostdin", "-v", "error", "-xerror", "-err_detect", "explode", "-f", "hevc", "-i", str(bitstream), "-map", "0:v:0", "-f", "framemd5", "-n", str(directory / f"{name}.framemd5")], directory, f"decode-{name}")
    expected = hashes(directory / "source.framemd5")
    actual = hashes(directory / "delivered.framemd5")
    if len(expected) != frames or actual != expected:
        raise ValueError("independent decoded-frame hashes differ")
    result = {"result": "passed", "evidence": "HEVC byte preservation and independent software decode under offline packet impairment", "not_tested": ["live Tailscale/QUIC", "hardware codec", "capture", "presentation latency", "OS input"], "versions": versions, "platform": platform.platform(), "profile": {k: stream[k] for k in ("codec_name", "profile", "width", "height", "pix_fmt")}, "idr_frames": sum(key for key, _ in packets), "decoded_frames": len(actual), "delivery": stats, "sha256": {name: hashlib.sha256((directory / name).read_bytes()).hexdigest() for name in ("source.frhevc", "delivered.frhevc", "configuration.hvcc", "source.framemd5", "delivered.framemd5")}}
    (directory / "result.json").write_text(json.dumps(result, indent=2) + "\n")
    return result


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output-dir", type=Path, required=True, help="new directory for retained synthetic corpus and logs")
    parser.add_argument("--frames", type=int, default=90, choices=range(61, 241), metavar="61..240")
    args = parser.parse_args()
    try:
        print(json.dumps(run(args.output_dir.resolve(), args.frames), indent=2))
        return 0
    except (OSError, ValueError, KeyError, RuntimeError, subprocess.SubprocessError) as error:
        print(f"HEVC verification failed: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
