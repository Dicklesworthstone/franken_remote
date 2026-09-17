#!/usr/bin/env python3
"""Synthetic Opus -> private PipeWire source -> real pw-cat recorder.

Run: python3 probe.py NEW_OUTPUT_DIR [--pipewire-root EXTRACTED_PACKAGE_ROOT]
Requires PipeWire 1.6.2 tools/modules, FFmpeg with libopus, Python 3.
No system audio configuration changes, microphone capture, or network listener.
This is endpoint evidence, not a FrankenRemote uplink or OS qualification.
Outputs are retained, including nonzero recorder status; nothing is deleted.
"""
import argparse
import array
import json
import math
import os
from pathlib import Path
import shutil
import subprocess
import time
import wave

CONFIG = """
context.properties = {
 core.daemon = true core.name = fr-mic
 default.clock.rate = 48000 default.clock.quantum = 480
}
context.spa-libs = {
 audio.convert.* = audioconvert/libspa-audioconvert
 support.* = support/libspa-support
}
context.modules = [
 { name = libpipewire-module-protocol-native }
 { name = libpipewire-module-spa-node-factory }
 { name = libpipewire-module-client-node }
 { name = libpipewire-module-metadata }
 { name = libpipewire-module-adapter }
 { name = libpipewire-module-link-factory }
 { name = libpipewire-module-access args = { access.socket = { fr-mic = unrestricted } } }
]
context.objects = [
 { factory = spa-node-factory args = {
   factory.name = support.node.driver node.name = Dummy-Driver priority.driver = 200000
 } }
 { factory = adapter args = {
   factory.name = support.null-audio-sink node.name = fr-virtual-mic
   node.description = "FrankenRemote synthetic test microphone"
   media.class = Audio/Source/Virtual audio.position = [ MONO ]
   monitor.passthrough = true
   adapter.auto-port-config = { mode = dsp monitor = true position = preserve }
 } }
]
"""


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("output", type=Path)
    parser.add_argument("--pipewire-root", type=Path)
    args = parser.parse_args()
    out = args.output.resolve()
    out.mkdir(mode=0o700)  # Refuse reuse/overwrite of previous evidence.
    runtime = out / "runtime"
    runtime.mkdir(mode=0o700)
    environment = os.environ | {
        "XDG_RUNTIME_DIR": str(runtime),
        "PIPEWIRE_RUNTIME_DIR": str(runtime),
        "PIPEWIRE_REMOTE": "fr-mic",
    }
    if args.pipewire_root:
        root = args.pipewire_root.resolve()
        environment["PIPEWIRE_MODULE_DIR"] = str(root / "usr/lib/x86_64-linux-gnu/pipewire-0.3")
        environment["PIPEWIRE_CONFIG_DIR"] = str(root / "usr/share/pipewire")

    def binary(name):
        if args.pipewire_root and name.startswith(("pw-", "pipewire")):
            return str(root / "usr/bin" / name)
        path = shutil.which(name)
        if not path:
            raise RuntimeError(f"required executable missing: {name}")
        return path

    sequence = 0

    def run(name, *arguments):
        nonlocal sequence
        sequence += 1
        command = [binary(name), *map(str, arguments)]
        result = subprocess.run(command, env=environment, capture_output=True, timeout=15)
        (out / f"command-{sequence}.json").write_text(json.dumps({
            "argv": command, "exit_code": result.returncode,
            "stdout": result.stdout.decode(errors="replace"),
            "stderr": result.stderr.decode(errors="replace"),
        }, indent=2) + "\n")
        if result.stderr:
            print(result.stderr.decode(errors="replace"), end="")
        result.check_returncode()
        return result.stdout

    processes = []
    logs = []

    def start(label, name, *arguments):
        log = (out / f"{label}.log").open("wb")
        logs.append(log)
        process = subprocess.Popen([binary(name), *map(str, arguments)], env=environment,
                                   stdin=subprocess.DEVNULL, stdout=log, stderr=log)
        processes.append(process)
        return process

    def ports_ready(names):
        deadline = time.monotonic() + 5
        while time.monotonic() < deadline:
            ports = run("pw-link", "-io").decode()
            if all(name in ports for name in names):
                return
            if any(process.poll() is not None for process in processes):
                raise RuntimeError("native process exited before ports were ready; inspect logs")
            time.sleep(0.05)
        raise RuntimeError("bounded port readiness deadline expired")

    try:
        run("pipewire", "--version")
        run("ffmpeg", "-version")
        (out / "server.conf").write_text(CONFIG)
        samples = array.array("h", (int(12000 * math.sin(2 * math.pi *
                         (400 * t / 48000 + 600 * (t / 48000) ** 2))) for t in range(96000)))
        if os.sys.byteorder != "little":
            samples.byteswap()
        with wave.open(str(out / "signal.wav"), "wb") as audio:
            audio.setparams((1, 2, 48000, 0, "NONE", "not compressed"))
            audio.writeframes(samples.tobytes())
        run("ffmpeg", "-hide_banner", "-loglevel", "error", "-nostdin", "-i", out / "signal.wav",
            "-c:a", "libopus", "-frame_duration", "10", out / "signal.opus")
        run("ffmpeg", "-hide_banner", "-loglevel", "error", "-nostdin", "-i", out / "signal.opus",
            "-c:a", "pcm_s16le", out / "decoded.wav")
        server = start("server", "pipewire", "-c", out / "server.conf")
        deadline = time.monotonic() + 5
        while not (runtime / "fr-mic").exists():
            if server.poll() is not None or time.monotonic() >= deadline:
                raise RuntimeError("private server failed readiness; inspect server.log")
            time.sleep(0.05)
        ports_ready(["fr-virtual-mic:input_MONO", "fr-virtual-mic:capture_MONO"])
        run("pw-cli", "ls", "Node")
        common = ["-v", "--target", "0", "--latency", "480"]
        recorder = start("recorder", "pw-cat", *common, "--record", "--rate", "48000",
                         "--channels", "1", "--channel-map", "MONO", "--sample-count", "192000",
                         "-P", "{ node.name = fr-recorder adapter.auto-port-config = { mode = dsp position = preserve } }",
                         out / "recorded.wav")
        player = start("player", "pw-cat", *common, "--playback", "-P",
                       "{ node.name = fr-uplink adapter.auto-port-config = { mode = dsp position = preserve } }",
                       out / "decoded.wav")
        ports_ready(["fr-recorder:input_MONO", "fr-uplink:output_MONO"])
        run("pw-link", "fr-virtual-mic:capture_MONO", "fr-recorder:input_MONO")
        run("pw-link", "fr-uplink:output_MONO", "fr-virtual-mic:input_MONO")
        run("pw-link", "-l")
        player_exit = player.wait(timeout=8)
        recorder_exit = recorder.wait(timeout=8)
        with wave.open(str(out / "decoded.wav"), "rb") as audio:
            expected = audio.readframes(audio.getnframes())
        with wave.open(str(out / "recorded.wav"), "rb") as audio:
            shape = (audio.getnchannels(), audio.getsampwidth(), audio.getframerate(), audio.getnframes())
            captured = audio.readframes(audio.getnframes())
        offset = captured.find(expected)
        content_passed = shape == (1, 2, 48000, 192000) and offset >= 0 and offset % 2 == 0
        result = {
            "scope": "synthetic file Opus decoded before playback; private PipeWire native source and pw-cat recorder",
            "player_exit": player_exit, "recorder_exit": recorder_exit,
            "exact_decoded_audio_present": content_passed,
            "decoded_samples": len(expected) // 2, "recording_samples": shape[3],
            "alignment_samples_not_latency": offset // 2 if offset >= 0 else None,
            "latency_measured": False, "device_change_tested": False,
            "desktop_session_manager_tested": False,
            "frankenremote_uplink_implemented": False,
            "macos_tested": False, "windows_tested": False,
            "bead_complete": False,
            "recorder_exit_note": "PipeWire 1.6.2 pw-cat sample-limit quits without drained; upstream returns 0 only when drained. Nonzero status retained, not hidden.",
        }
        (out / "result.json").write_text(json.dumps(result, indent=2) + "\n")
        print(json.dumps(result, indent=2))
        if not content_passed or player_exit != 0:
            raise RuntimeError("audio flow check failed")
        return recorder_exit  # Preserve native command failure even with matching audio.
    finally:
        for process in reversed(processes):
            if process.poll() is None:
                process.terminate()
                try:
                    process.wait(timeout=3)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait(timeout=3)
        for log in logs:
            log.close()


if __name__ == "__main__":
    raise SystemExit(main())
