#!/usr/bin/env python3
"""Compile the exact shipping XCB boundary and test it on fresh owned X servers.

No test warms up XTest before input capture. Every case gets a fresh server so
another test's remapping or device activation cannot conceal cold-start bugs.
This exercises native keyboard capture, not authenticated remote authority.
"""
import os
from pathlib import Path
import selectors
import shlex
import signal
import subprocess
import tempfile

HERE = Path(__file__).resolve().parent
CASES = (
    "cold-keyboard", "real-remap", "map-revalidation", "name-revalidation",
    "invalid-notifications", "malformed-replies", "stalled-server",
)


def run_case(binary: Path, case: str, directory: Path) -> None:
    read_fd, write_fd = os.pipe()
    with (directory / f"{case}.xvfb.log").open("wb") as log:
        try:
            server = subprocess.Popen(
                ["Xvfb", "-displayfd", str(write_fd), "-screen", "0", "640x480x24",
                 "-noreset", "-nolisten", "tcp"],
                pass_fds=(write_fd,), stdin=subprocess.DEVNULL, stdout=log, stderr=log,
            )
        finally:
            os.close(write_fd)
        try:
            with selectors.DefaultSelector() as ready:
                ready.register(read_fd, selectors.EVENT_READ)
                if not ready.select(5):
                    raise RuntimeError("Xvfb did not publish its display within five seconds")
                display = os.read(read_fd, 32).strip()
            if not display.isdigit():
                raise RuntimeError("Xvfb failed to publish a display number")
            environment = dict(os.environ, DISPLAY=":" + display.decode("ascii"))
            subprocess.run([str(binary), case, str(server.pid)], env=environment,
                           check=True, timeout=10)
        finally:
            os.close(read_fd)
            # A failed stall assertion must never leave our child SIGSTOPed.
            if server.poll() is None:
                server.send_signal(signal.SIGCONT)
                server.terminate()
            try:
                server.wait(timeout=5)
            except subprocess.TimeoutExpired:
                server.kill()
                server.wait(timeout=5)


def main() -> None:
    with tempfile.TemporaryDirectory(prefix="fr-keyboard-map-") as temporary:
        directory = Path(temporary)
        binary = directory / "keyboard-map"
        compiler = shlex.split(os.environ.get("CC", "cc"))
        subprocess.run(compiler + ["-std=c11", "-O2", "-Wall", "-Wextra", "-Werror",
                                   str(HERE / "keyboard_map.c"), "-lxcb", "-lX11",
                                   "-l:libXtst.so.6", "-o", str(binary)],
                       check=True, timeout=30)
        for case in CASES:
            run_case(binary, case, directory)
        print(f"{len(CASES)} native keyboard-map cases passed", flush=True)


if __name__ == "__main__":
    main()
