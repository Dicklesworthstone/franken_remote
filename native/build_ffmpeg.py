#!/usr/bin/env python3
"""Build the decode-only experiment from a supplied pinned archive; never install globally."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import shutil
import subprocess
import tarfile


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("archive", type=Path)
    parser.add_argument("output", type=Path, help="new directory; existing paths refuse")
    args = parser.parse_args()
    policy = json.loads(Path(__file__).with_name("linux-ffmpeg.json").read_text())
    if platform.system() != "Linux" or platform.machine() != "x86_64":
        parser.error("this experiment requires native x86_64 Linux")
    digest = hashlib.file_digest(args.archive.open("rb"), "sha256").hexdigest()
    if digest != policy["source"]["sha256"]:
        parser.error("source archive SHA256 mismatch")
    output = args.output.absolute()
    output.mkdir(parents=True, exist_ok=False)
    evidence = output / "evidence"
    evidence.mkdir()
    source = output / "source"
    source.mkdir()
    with tarfile.open(args.archive) as archive:
        members = archive.getmembers()
        if len(members) > 20000 or sum(m.size for m in members) > 300_000_000:
            raise ValueError("source archive exceeds extraction bounds")
        archive.extractall(source, members=members, filter="data")
    source = source / ("ffmpeg-" + policy["source"]["version"])
    sdk = output / "sdk"
    # Keep build paths out of FFmpeg's embedded configuration; stage /sdk below output.
    command = ["./configure"] + policy["configure_flags"]
    install = ["make", "install", "DESTDIR=" + str(output)]
    environment = os.environ.copy()
    environment.update(SOURCE_DATE_EPOCH=str(policy["source_date_epoch"]), LC_ALL="C")
    record = {"policy": policy, "configure": command, "platform": platform.platform(),
              "install": install, "source_sha256": digest, "status": "started"}
    manifest = evidence / "manifest.json"
    manifest.write_text(json.dumps(record, indent=2) + "\n")
    try:
        for index, cmd in enumerate((["cc", "--version"], command, ["make", "-j1"], install)):
            print("COMMAND", cmd, flush=True)
            with (evidence / f"command-{index}.log").open("wb") as log:
                result = subprocess.run(cmd, cwd=source, env=environment, stdout=log, stderr=subprocess.STDOUT)
            print((evidence / f"command-{index}.log").read_text(errors="replace"), flush=True)
            result.check_returncode()
        for name in ("config.h", "config_components.h", "ffbuild/config.mak", "COPYING.LGPLv2.1", "LICENSE.md"):
            shutil.copyfile(source / name, evidence / Path(name).name)
        components = (source / "config_components.h").read_text().splitlines()
        enabled = sorted(line.split()[1] for line in components if line.startswith("#define CONFIG_") and line.endswith(" 1"))
        if enabled != sorted(policy["registered_components"]):
            raise ValueError(f"unexpected registered components: {enabled}")
        libraries = sorted(p for p in (sdk / "lib").glob("*.so.*") if not p.is_symlink())
        if sorted(p.name.split(".")[0].removeprefix("lib") for p in libraries) != sorted(policy["libraries"]):
            raise ValueError("unexpected installed library set")
        record["libraries"] = {}
        for lib in libraries:
            with lib.open("rb") as stream:
                record["libraries"][lib.name] = hashlib.file_digest(stream, "sha256").hexdigest()
            inspection = subprocess.run(["readelf", "-d", str(lib)], check=True, capture_output=True, text=True)
            (evidence / (lib.name + ".dynamic.txt")).write_text(inspection.stdout)
        record.update(status="built", enabled_components=enabled)
    except Exception as error:
        record.update(status="failed", error=str(error))
        raise
    finally:
        manifest.write_text(json.dumps(record, indent=2) + "\n")
    print("SDK", sdk, flush=True)


if __name__ == "__main__":
    main()
