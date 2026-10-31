"""Independent native PCM capture on a test-owned private PipeWire daemon.

There is deliberately no session manager: configure and link only the two
exact test-owned node IDs, never select a user's device or change defaults.
"""
import array
import json
import os
from pathlib import Path
import subprocess
import sys
import time
import wave

directory = Path(sys.argv[1])
source = int(sys.argv[2])
recorder_name = "frankenremote.mic.qualification.recorder"


def run(*args):
    return subprocess.check_output(args, text=True, timeout=3)


def inventory():
    return json.loads(run("pw-dump"))


def props(row):
    return row.get("info", {}).get("props", {})


path = directory / "capture.wav"
with (directory / "recorder.stderr").open("w") as log:
    recorder = subprocess.Popen([
        "pw-cat", "--record", "--target", "0", "--rate", "48000",
        "--channels", "1", "--format", "s16", "--properties",
        "node.name=" + recorder_name, str(path),
    ], stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=log)
    try:
        deadline = time.monotonic() + 8
        configured = False
        while True:
            assert recorder.poll() is None, "independent recorder exited"
            assert time.monotonic() < deadline, "private recorder port setup timed out"
            rows = inventory()
            nodes = {r["id"]: r for r in rows if r["type"] == "PipeWire:Interface:Node"}
            assert source in nodes, "the original source disappeared"
            assert props(nodes[source])["media.class"] == "Audio/Source"
            assert props(nodes[source])["node.virtual"] is True
            assert props(nodes[source]).get("node.autoconnect") is False
            candidates = [i for i, r in nodes.items() if props(r).get("node.name") == recorder_name]
            if len(candidates) == 1:
                target = candidates[0]
                if not configured:
                    for node, direction in [(source, "Output"), (target, "Input")]:
                        run("pw-cli", "set-param", str(node), "PortConfig",
                            "{ direction = " + direction + " mode = dsp format = { "
                            "mediaType = audio mediaSubtype = raw format = F32P "
                            "rate = 48000 channels = 1 position = [ MONO ] } }")
                    configured = True
                ports = [r for r in rows if r["type"] == "PipeWire:Interface:Port"]
                outputs = [r["id"] for r in ports if props(r).get("node.id") == source
                           and props(r).get("port.direction") == "out"]
                inputs = [r["id"] for r in ports if props(r).get("node.id") == target
                          and props(r).get("port.direction") == "in"]
                if len(outputs) == len(inputs) == 1:
                    run("pw-link", str(outputs[0]), str(inputs[0]))
                    break
            time.sleep(.02)
        (directory / "linked").touch()
        deadline = time.monotonic() + 10
        while not (directory / "finished").exists():
            assert recorder.poll() is None
            assert time.monotonic() < deadline, "qualification owner did not finish"
            time.sleep(.01)
        # Removal is separately witnessed by a second connection, not inferred
        # from the producer returning from Drop or its local statistics.
        deadline = time.monotonic() + 2
        while any(r["id"] == source and r["type"] == "PipeWire:Interface:Node"
                  for r in inventory()):
            assert time.monotonic() < deadline, "source lingered after owner close"
            time.sleep(.01)
        recorder.terminate()
        recorder.wait(timeout=3)
    finally:
        if recorder.poll() is None:
            recorder.kill()
            recorder.wait(timeout=3)

with wave.open(str(path), "rb") as stream:
    assert stream.getnchannels() == 1
    assert stream.getframerate() == 48000
    assert stream.getsampwidth() == 2
    samples = array.array("h", stream.readframes(stream.getnframes()))
if sys.byteorder != "little":
    samples.byteswap()
assert len(samples) >= 96000, "less than two seconds independently captured"
audible = [value for value in samples if abs(value) > 100]
assert len(audible) > 48000, "synthetic source audio never reached the recorder"
assert max(map(abs, samples)) <= 12001
matching = sum(abs(abs(value) - 12000) <= 1 for value in audible)
assert matching / len(audible) > .95, "recorded PCM does not match submitted synthetic sound"
# The final quarter second must stay silent even after fresh audible content
# was queued immediately before the terminal fence.
tail_peak = max(map(abs, samples[-12000:]))
assert tail_peak <= 1, "expired or stopped sound reached the independent recorder"
print(json.dumps({"independent_capture_passed": True, "recorded_frames": len(samples),
                  "matching_audible_frames": matching, "tail_peak": tail_peak,
                  "exact_source_removed": True}, sort_keys=True))
