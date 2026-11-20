#!/usr/bin/env python3
"""Planted negative tests and verification test runner for count.py and audit.py."""

from __future__ import annotations

import json
import os
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent


def run_cmd(args: list[str]) -> tuple[int, str, str]:
    proc = subprocess.run(
        args,
        capture_output=True,
        text=True,
        cwd=REPO_ROOT,
        timeout=60,
    )
    return proc.returncode, proc.stdout, proc.stderr


def test_baseline() -> None:
    print("[1/11] Testing baseline count on repo...")
    rc, stdout, stderr = run_cmd([sys.executable, "scripts/count.py", "--json"])
    assert rc == 0, f"count.py failed on baseline repo: {stderr}"
    print("  ✓ Baseline count passed within budget.")

    print("[2/11] Testing baseline audit on repo...")
    rc, stdout, stderr = run_cmd([sys.executable, "scripts/audit.py", "--json"])
    assert rc == 0, f"audit.py failed on baseline repo: {stderr}"
    print("  ✓ Baseline audit passed with zero violations.")


def test_planted_over_budget_rust() -> None:
    # The owner lifted the 500k hard stop on 2026-11-08. A tree past the old
    # stop must still be counted exactly and must no longer be refused.
    print("[3/11] Testing a Rust fixture past the lifted 500,000 stop...")
    with tempfile.TemporaryDirectory() as tmpdir:
        tmp_path = Path(tmpdir)
        crates_dir = tmp_path / "crates" / "fake-crate" / "src"
        crates_dir.mkdir(parents=True)
        with open(crates_dir / "lib.rs", "w", encoding="utf-8") as fp:
            fp.write("#![forbid(unsafe_code)]\n")
            fp.write("// padding\n" * 500_005)

        rc, stdout, stderr = run_cmd([
            sys.executable, "scripts/count.py", "--fixture", str(tmp_path), "--json"
        ])
        assert rc == 0, f"Expected no Rust hard stop, but got rc={rc}: {stderr}"
        assert "OVER_BUDGET_REFUSAL" not in stderr + stdout
        stats = json.loads(stdout)
        assert stats["rust_handwritten"]["total"] == 500_006, stats["rust_handwritten"]
        assert stats["rust_handwritten"]["hard_stop"] is None
        print("  ✓ 500,006 Rust lines counted exactly and not refused.")


def test_planted_over_budget_glue() -> None:
    # The 20k glue allowance was lifted on 2026-11-13 (owner-delegated). A
    # tree past it must still be counted exactly, per extension, and must no
    # longer be refused.
    print("[4/11] Testing a glue fixture past the lifted 20,000 allowance...")
    with tempfile.TemporaryDirectory() as tmpdir:
        tmp_path = Path(tmpdir)
        web_dir = tmp_path / "web"
        web_dir.mkdir(parents=True)
        with open(web_dir / "app.js", "w", encoding="utf-8") as fp:
            fp.write("// glue line\n" * 20_005)
        with open(web_dir / "peer.py", "w", encoding="utf-8") as fp:
            fp.write("# glue line\n" * 7)

        rc, stdout, stderr = run_cmd([
            sys.executable, "scripts/count.py", "--fixture", str(tmp_path), "--json"
        ])
        assert rc == 0, f"Expected no glue allowance, but got rc={rc}: {stderr}"
        assert "OVER_BUDGET_REFUSAL" not in stderr + stdout
        stats = json.loads(stdout)
        glue = stats["non_rust_glue"]
        assert glue["total"] == 20_012, glue
        assert glue["by_extension"] == {".js": 20_005, ".py": 7}, glue
        assert glue["allowance"] is None, glue
        print("  ✓ 20,012 glue lines counted exactly per extension and not refused.")


def test_planted_forbidden_unsafe() -> None:
    print("[5/11] Testing planted unsafe code in safe crate...")
    with tempfile.TemporaryDirectory() as tmpdir:
        tmp_path = Path(tmpdir)
        crate_dir = tmp_path / "crates" / "safe-crate"
        src_dir = crate_dir / "src"
        src_dir.mkdir(parents=True)
        (crate_dir / "Cargo.toml").write_text('[package]\nname = "safe-crate"\nversion = "0.1.0"\n')
        (src_dir / "lib.rs").write_text("#![forbid(unsafe_code)]\npub fn bad() { unsafe { } }\n")

        rc, stdout, stderr = run_cmd([
            sys.executable, "scripts/audit.py", "--fixture", str(tmp_path)
        ])
        assert rc == 1, f"Expected audit.py to refuse unsafe code, got rc={rc}"
        assert "Forbidden unsafe usage" in stderr or "Forbidden unsafe usage" in stdout
        print("  ✓ Planted unsafe code in safe crate correctly caught and refused.")


def test_planted_missing_forbid_unsafe() -> None:
    print("[6/11] Testing planted missing #![forbid(unsafe_code)]...")
    with tempfile.TemporaryDirectory() as tmpdir:
        tmp_path = Path(tmpdir)
        crate_dir = tmp_path / "crates" / "safe-crate"
        src_dir = crate_dir / "src"
        src_dir.mkdir(parents=True)
        (crate_dir / "Cargo.toml").write_text('[package]\nname = "safe-crate"\nversion = "0.1.0"\n')
        (src_dir / "lib.rs").write_text("pub fn clean() {}\n")

        rc, stdout, stderr = run_cmd([
            sys.executable, "scripts/audit.py", "--fixture", str(tmp_path)
        ])
        assert rc == 1, f"Expected audit.py to refuse missing forbid, got rc={rc}"
        assert "Missing #![forbid(unsafe_code)]" in stderr or "Missing #![forbid(unsafe_code)]" in stdout
        print("  ✓ Missing #![forbid(unsafe_code)] correctly caught and refused.")


def test_planted_browser_desktop_ffi() -> None:
    print("[7/11] Testing planted desktop FFI dependency in browser build...")
    with tempfile.TemporaryDirectory() as tmpdir:
        tmp_path = Path(tmpdir)
        crate_dir = tmp_path / "crates" / "fr-web"
        src_dir = crate_dir / "src"
        src_dir.mkdir(parents=True)
        (crate_dir / "Cargo.toml").write_text(
            '[package]\nname = "fr-web"\nversion = "0.1.0"\n[dependencies]\nfr-native = { path = "../fr-native" }\n'
        )
        (src_dir / "lib.rs").write_text("#![forbid(unsafe_code)]\n")

        rc, stdout, stderr = run_cmd([
            sys.executable, "scripts/audit.py", "--fixture", str(tmp_path)
        ])
        assert rc == 1, f"Expected audit.py to refuse desktop FFI in browser, got rc={rc}"
        assert "Browser build cannot inherit desktop FFI" in stderr or "Browser build cannot inherit desktop FFI" in stdout
        print("  ✓ Desktop FFI in browser build correctly caught and refused.")


def test_planted_forbidden_runtime() -> None:
    print("[8/11] Testing planted forbidden runtime (tokio)...")
    with tempfile.TemporaryDirectory() as tmpdir:
        tmp_path = Path(tmpdir)
        crate_dir = tmp_path / "crates" / "some-crate"
        src_dir = crate_dir / "src"
        src_dir.mkdir(parents=True)
        (crate_dir / "Cargo.toml").write_text(
            '[package]\nname = "some-crate"\nversion = "0.1.0"\n[dependencies]\ntokio = "1.0"\n'
        )
        (src_dir / "lib.rs").write_text("#![forbid(unsafe_code)]\n")

        rc, stdout, stderr = run_cmd([
            sys.executable, "scripts/audit.py", "--fixture", str(tmp_path)
        ])
        assert rc == 1, f"Expected audit.py to refuse tokio runtime, got rc={rc}"
        assert "Forbidden runtime dependency 'tokio'" in stderr or "Forbidden runtime dependency 'tokio'" in stdout
        print("  ✓ Forbidden runtime dependency (tokio) correctly caught and refused.")


def test_planted_ios_target_desktop_ffi() -> None:
    print("[9/11] Testing planted cfg(target_os = \"ios\") desktop FFI contamination...")
    with tempfile.TemporaryDirectory() as tmpdir:
        tmp_path = Path(tmpdir)
        crate_dir = tmp_path / "crates" / "fr-mobile-ffi"
        src_dir = crate_dir / "src"
        src_dir.mkdir(parents=True)
        (crate_dir / "Cargo.toml").write_text(
            '[package]\nname = "fr-mobile-ffi"\nversion = "0.1.0"\n'
            '[target.\'cfg(target_os = "ios")\'.dependencies]\n'
            'x11 = "2.21"\n'
        )
        (src_dir / "lib.rs").write_text("// mobile FFI boundary\n")

        rc, stdout, stderr = run_cmd([
            sys.executable, "scripts/audit.py", "--fixture", str(tmp_path), "--json"
        ])
        assert rc == 1, f"Expected audit.py to refuse x11 on iOS target, got rc={rc}"
        report = json.loads(stdout)
        assert report["targets"]["targets"]["aarch64-apple-ios"]["compliant"] is False
        assert report["targets"]["targets"]["aarch64-linux-android"]["compliant"] is True
        print("  ✓ cfg-resolved iOS desktop FFI leak caught while leaving Android target clean.")


def test_planted_wasm_target_desktop_ffi() -> None:
    print("[10/11] Testing planted cfg(target_arch = \"wasm32\") desktop FFI contamination...")
    with tempfile.TemporaryDirectory() as tmpdir:
        tmp_path = Path(tmpdir)
        crate_dir = tmp_path / "crates" / "fr-client"
        src_dir = crate_dir / "src"
        src_dir.mkdir(parents=True)
        (crate_dir / "Cargo.toml").write_text(
            '[package]\nname = "fr-client"\nversion = "0.1.0"\n'
            '[target.\'cfg(target_arch = "wasm32")\'.dependencies]\n'
            'windows-sys = "0.59"\n'
        )
        (src_dir / "lib.rs").write_text("#![forbid(unsafe_code)]\n")

        rc, stdout, stderr = run_cmd([
            sys.executable, "scripts/audit.py", "--fixture", str(tmp_path), "--json"
        ])
        assert rc == 1, f"Expected audit.py to refuse windows-sys on wasm32, got rc={rc}"
        report = json.loads(stdout)
        assert report["targets"]["targets"]["wasm32-unknown-unknown"]["compliant"] is False
        print("  ✓ cfg-resolved wasm32 desktop FFI leak correctly caught and refused.")


def test_planted_daemon_target_gui_dep() -> None:
    print("[11/11] Testing planted cfg(target_os = \"linux\") GUI dep in frd...")
    with tempfile.TemporaryDirectory() as tmpdir:
        tmp_path = Path(tmpdir)
        crate_dir = tmp_path / "crates" / "frd"
        src_dir = crate_dir / "src"
        src_dir.mkdir(parents=True)
        (crate_dir / "Cargo.toml").write_text(
            '[package]\nname = "frd"\nversion = "0.1.0"\n'
            '[target.\'cfg(target_os = "linux")\'.dependencies]\n'
            'winit = "0.30"\n'
        )
        (src_dir / "lib.rs").write_text("#![forbid(unsafe_code)]\n")

        rc, stdout, stderr = run_cmd([
            sys.executable, "scripts/audit.py", "--fixture", str(tmp_path), "--json"
        ])
        assert rc == 1, f"Expected audit.py to refuse winit in frd, got rc={rc}"
        report = json.loads(stdout)
        assert report["targets"]["targets"]["x86_64-unknown-linux-gnu"]["compliant"] is False
        assert report["targets"]["targets"]["aarch64-apple-darwin"]["compliant"] is True
        print("  ✓ cfg-resolved Linux GUI dep in frd correctly caught and refused.")


def main() -> int:
    print("================================================================================")
    print("           RUNNING VERIFY LANES & PLANTED FIXTURE TEST SUITE")
    print("================================================================================")
    test_baseline()
    test_planted_over_budget_rust()
    test_planted_over_budget_glue()
    test_planted_forbidden_unsafe()
    test_planted_missing_forbid_unsafe()
    test_planted_browser_desktop_ffi()
    test_planted_forbidden_runtime()
    test_planted_ios_target_desktop_ffi()
    test_planted_wasm_target_desktop_ffi()
    test_planted_daemon_target_gui_dep()
    print("================================================================================")
    print("ALL 11 VERIFICATION & PLANTED FIXTURE TESTS PASSED CLEANLY!")
    print("================================================================================")
    return 0


if __name__ == "__main__":
    sys.exit(main())
