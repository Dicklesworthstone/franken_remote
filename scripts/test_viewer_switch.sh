#!/usr/bin/env bash
# Actual native window/shortcut boundary; independent of the async Rust graph.
set -euo pipefail
cd "$(dirname "$0")/.."
work="${FR_VIEWER_SWITCH_TEST_DIR:-$(mktemp -d /tmp/fr-viewer-switch.XXXXXX)}"
mkdir -p "$work"
# shellcheck disable=SC2046
"${CC:-cc}" -std=c11 -O2 -Wall -Wextra -Werror \
  crates/fr-native/src/viewer_window.c crates/fr-native/tests/viewer_switch.c \
  $(pkg-config --cflags --libs x11 xcb) -l:libXtst.so.6 -o "$work/viewer-switch-test"
python3 - "$work" <<'PY'
import os
from pathlib import Path
import selectors
import subprocess
import sys

work = Path(sys.argv[1]).resolve()
with (work / "xvfb.log").open("w") as errors:
    server = subprocess.Popen(
        ["Xvfb", "-displayfd", "1", "-screen", "0", "640x480x24", "-nolisten", "tcp", "-noreset"],
        stdout=subprocess.PIPE, stderr=errors,
    )
    try:
        with selectors.DefaultSelector() as selector:
            selector.register(server.stdout, selectors.EVENT_READ)
            if not selector.select(5):
                raise RuntimeError("Xvfb did not become ready")
            number = int(server.stdout.readline().strip())
        result = subprocess.run(
            [str(work / "viewer-switch-test")],
            env=dict(os.environ, DISPLAY=f":{number}"),
            capture_output=True, text=True, timeout=30,
        )
        output = result.stdout + result.stderr + f"process_exit={result.returncode}\n"
        (work / "viewer-switch.log").write_text(output)
        print(output, end="")
        result.check_returncode()
    finally:
        if server.poll() is None:
            server.terminate()
        server.wait(timeout=5)
        server.stdout.close()
print(f"Retained native evidence: {work}")
PY
