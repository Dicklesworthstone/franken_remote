#!/usr/bin/env python3
"""Planted negative tests and verification test runner for count.py and audit.py."""

from __future__ import annotations

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
    print("[1/8] Testing baseline count on repo...")
    rc, stdout, stderr = run_cmd([sys.executable, "scripts/count.py", "--json"])
    assert rc == 0, f"count.py failed on baseline repo: {stderr}"
    print("  ✓ Baseline count passed within budget.")

    print("[2/8] Testing baseline audit on repo...")
    rc, stdout, stderr = run_cmd([sys.executable, "scripts/audit.py", "--json"])
    assert rc == 0, f"audit.py failed on baseline repo: {stderr}"
    print("  ✓ Baseline audit passed with zero violations.")


def test_planted_over_budget_rust() -> None:
    print("[3/8] Testing planted over-budget Rust fixture (>500,000 lines)...")
    with tempfile.TemporaryDirectory() as tmpdir:
        tmp_path = Path(tmpdir)
        crates_dir = tmp_path / "crates" / "fake-crate" / "src"
        crates_dir.mkdir(parents=True)
        # Write 500,005 lines of dummy Rust code (just over the hard stop)
        with open(crates_dir / "lib.rs", "w", encoding="utf-8") as fp:
            fp.write("#![forbid(unsafe_code)]\n")
            fp.write("// padding\n" * 500_005)

        rc, stdout, stderr = run_cmd([
            sys.executable, "scripts/count.py", "--fixture", str(tmp_path)
        ])
        assert rc == 1, f"Expected count.py to refuse over-budget Rust, but got rc={rc}"
        assert "OVER_BUDGET_REFUSAL" in stderr or "OVER_BUDGET_REFUSAL" in stdout
        print("  ✓ Planted over-budget Rust correctly refused with exit 1.")


def test_planted_over_budget_glue() -> None:
    print("[4/8] Testing planted over-budget glue fixture (>20,000 lines)...")
    with tempfile.TemporaryDirectory() as tmpdir:
        tmp_path = Path(tmpdir)
        web_dir = tmp_path / "web"
        web_dir.mkdir(parents=True)
        # Write 20,005 lines of dummy JS glue
        with open(web_dir / "app.js", "w", encoding="utf-8") as fp:
            fp.write("// glue line\n" * 20_005)

        rc, stdout, stderr = run_cmd([
            sys.executable, "scripts/count.py", "--fixture", str(tmp_path)
        ])
        assert rc == 1, f"Expected count.py to refuse over-budget glue, but got rc={rc}"
        assert "OVER_BUDGET_REFUSAL" in stderr or "OVER_BUDGET_REFUSAL" in stdout
        print("  ✓ Planted over-budget glue correctly refused with exit 1.")


def test_planted_forbidden_unsafe() -> None:
    print("[5/8] Testing planted unsafe code in safe crate...")
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
    print("[6/8] Testing planted missing #![forbid(unsafe_code)]...")
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
    print("[7/8] Testing planted desktop FFI dependency in browser build...")
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
    print("[8/8] Testing planted forbidden runtime (tokio)...")
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
    print("================================================================================")
    print("ALL 8 VERIFICATION & PLANTED FIXTURE TESTS PASSED CLEANLY!")
    print("================================================================================")
    return 0


if __name__ == "__main__":
    sys.exit(main())
