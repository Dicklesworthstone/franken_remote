#!/usr/bin/env python3
"""Exercise the exact chooser pause boundary on isolated, real X11 servers.

These native query/event cases do not claim remote input authority, portal
integration or file delivery. Each case gets a fresh server and capture owner.
"""
import os
from pathlib import Path
import shlex
import subprocess
import tempfile

from keyboard_map import run_case

HERE = Path(__file__).resolve().parent
CASES = (
    "focus", "visibility", "stale-focus-reply", "held-input", "text-pause",
    "pending-refusal", "shortcuts", "unmap", "geometry", "synthetic", "stalled-probe",
)


def main() -> None:
    with tempfile.TemporaryDirectory(prefix="fr-viewer-choice-") as temporary:
        directory = Path(temporary)
        binary = directory / "viewer-choice"
        compiler = shlex.split(os.environ.get("CC", "cc"))
        subprocess.run(compiler + ["-std=c11", "-O2", "-Wall", "-Wextra", "-Werror", "-pthread",
                                   "-DFR_NATIVE_XLIB=1", str(HERE / "choice.c"),
                                   str(HERE / "../../src/x11_connection.c"), "-lxcb", "-lX11",
                                   "-l:libXtst.so.6", "-o", str(binary)], check=True, timeout=30)
        for case in CASES:
            run_case(binary, case, directory)
        print(f"{len(CASES)} native chooser pause cases passed", flush=True)


if __name__ == "__main__":
    main()
