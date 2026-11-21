#!/usr/bin/env python3
"""Exercise the shipping bounded composer on fresh, test-owned X servers.

The producer is real XCB/XIM/XTest; this does not qualify remote delivery or an
external input-method server. No fixture touches the user's desktop or keymap.
"""
import os
from pathlib import Path
import shlex
import subprocess
import tempfile

from keyboard_map import run_case

HERE = Path(__file__).resolve().parent
CASES = (
    "utf8-focus", "compose-before-send", "cancel-reopen", "context-readiness",
    "bounds", "editing", "lifecycle", "destroy-draw", "destroy-cleanup",
    "error-isolation", "synthetic", "escape",
    "unavailable-ime", "small-surface",
    "same-batch-key", "same-batch-pointer", "local-pointer-release", "release-only-reconcile",
    "delayed-local-key-release", "delayed-local-button-release",
    "reconciled-local-new-gesture",
)


def main() -> None:
    with tempfile.TemporaryDirectory(prefix="fr-viewer-text-") as temporary:
        directory = Path(temporary)
        binary = directory / "viewer-text"
        compiler = shlex.split(os.environ.get("CC", "cc"))
        subprocess.run(compiler + ["-std=c11", "-O2", "-Wall", "-Wextra", "-Werror", "-pthread",
                                   str(HERE / "text.c"), "-lxcb", "-lX11",
                                   "-l:libXtst.so.6", "-o", str(binary)],
                       check=True, timeout=30)
        for case in CASES:
            run_case(binary, case, directory)
        print(f"{len(CASES)} native text cases passed", flush=True)


if __name__ == "__main__":
    main()
