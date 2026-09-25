#!/usr/bin/env bash
# Real native transfer checks and scoped Xvfb capture timings; no codec simulation.
set -euo pipefail
cd "$(dirname "$0")/.."
work="${FR_X11_TEST_DIR:-$(mktemp -d /tmp/fr-x11-image.XXXXXX)}"
mkdir -p "$work"
# shellcheck disable=SC2046
"${CC:-cc}" -std=c11 -O2 -Wall -Wextra -Werror \
  crates/fr-native/src/x11_image.c crates/fr-native/tests/x11_image.c \
  $(pkg-config --cflags --libs x11 xcb) -l:libX11-xcb.so.1 -o "$work/image-test"
python3 - "$work" <<'PY'
import os
from pathlib import Path
import selectors
import subprocess
import sys
work = Path(sys.argv[1]).resolve()
for mode in ("shared", "socket"):
    args = ["Xvfb", "-displayfd", "1", "-screen", "0", "1920x1080x24", "-nolisten", "tcp"]
    if mode == "socket":
        args += ["-extension", "MIT-SHM"]
    with (work / f"xvfb-{mode}.log").open("w") as errors:
        server = subprocess.Popen(args, stdout=subprocess.PIPE, stderr=errors)
        try:
            with selectors.DefaultSelector() as selector:
                selector.register(server.stdout, selectors.EVENT_READ)
                if not selector.select(5):
                    raise RuntimeError("Xvfb did not become ready")
                number = int(server.stdout.readline().strip())
            result = subprocess.run([str(work / "image-test"), mode],
                env=dict(os.environ, DISPLAY=f":{number}"), text=True,
                capture_output=True, timeout=60)
            output = result.stdout + result.stderr + f"process_exit={result.returncode}\n"
            (work / f"{mode}.log").write_text(output)
            print(output, end="")
            result.check_returncode()
        finally:
            if server.poll() is None:
                server.terminate()
            server.wait(timeout=5)
            server.stdout.close()
print(f"Retained native evidence: {work}")
PY
