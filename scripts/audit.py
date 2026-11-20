#!/usr/bin/env python3
"""Feature-resolved per-target dependency and unsafe audit lane for FrankenRemote.

Enforces architecture and security contracts per AGENTS.md Sections 3.1, 3.2, 3.5:
1. Memory-safety boundary:
   - All protocol, policy, admission, scheduling, session code uses #![forbid(unsafe_code)].
   - Unsafe code is confined strictly to named boundary crates (BOUNDARY_CRATES).
2. One runtime:
   - Asupersync is the sole async runtime.
   - Forbidden: tokio, async-std, smol, libwebrtc, electron, chromium, web application frameworks.
3. Feature-resolved per-target dependency rules:
   - Browser build (wasm32) inherits no desktop FFI or native OS windowing.
   - Daemon (frd) inherits no GUI dependencies.
   - Separate accounting of direct, transitive, native, build-only, and test-only dependencies.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import sys
import tomllib
from pathlib import Path

# The named FFI boundary crates of AGENTS.md 3.2. Each opts out of the
# workspace's `unsafe_code = "forbid"` in its own Cargo.toml and says why there:
# fr-native (X11, FFmpeg, PulseAudio, Media Foundation, ...), fr-winsys (the
# Win32 calls under fr-tailnet and frd), fr-windows (the Windows client's
# window, message procedure and D3D11 presentation) and fr-mobile-ffi (the C
# ABI to Swift and the VideoToolbox glue). A crate is added here by name, not
# by opting itself out.
BOUNDARY_CRATES = {"fr-native", "fr-winsys", "fr-windows", "fr-mobile-ffi"}

FORBIDDEN_DEPENDENCIES = {
    "tokio",
    "async-std",
    "smol",
    "libwebrtc",
    "electron",
    "chromium",
    "actix",
    "actix-web",
    "rocket",
    "axum",
}

DESKTOP_FFI_CRATES = {
    "fr-native",
    "fr-ffi",
    "x11",
    "x11-dl",
    "winapi",
    "windows",
    "windows-sys",
    "metal",
    "objc",
    "cocoa",
    "core-graphics",
}

DAEMON_GUI_CRATES = {
    "winit",
    "gtk",
    "qt",
    "iced",
    "egui",
}


def is_rust_unsafe_keyword(line: str) -> bool:
    """Check if a line contains a Rust unsafe keyword outside comments and strings."""
    # Remove strings
    line_no_strings = re.sub(r'\"(?:\\.|[^\"])*\"', '', line)
    # Remove raw strings
    line_no_strings = re.sub(r'r(#*)\".*?\"\1', '', line_no_strings)
    # Remove comments
    line_no_comment = line_no_strings.split('//')[0]
    return bool(re.search(r'\bunsafe\b', line_no_comment))


def audit_unsafe(root_dir: Path) -> dict:
    """Audit unsafe code usage across all workspace crates."""
    crates_dir = root_dir / "crates"
    results: dict[str, dict] = {}
    violations: list[str] = []

    if not crates_dir.exists():
        return {"violations": violations, "crates": results}

    for c in sorted(os.listdir(crates_dir)):
        cp = crates_dir / c
        if not cp.is_dir() or not (cp / "Cargo.toml").exists():
            continue

        is_boundary = c in BOUNDARY_CRATES
        has_forbid = False

        for rf in ["src/lib.rs", "src/main.rs"]:
            root_file = cp / rf
            if root_file.exists():
                try:
                    content = root_file.read_text(encoding="utf-8", errors="ignore")
                    if "#![forbid(unsafe_code)]" in content:
                        has_forbid = True
                except Exception:
                    pass

        if not is_boundary and not has_forbid:
            violations.append(f"{c}: Missing #![forbid(unsafe_code)] in root module")

        unsafe_count = 0
        unsafe_locations: list[str] = []

        for root, dirs, files in os.walk(cp):
            if "target" in root:
                continue
            for f in files:
                if f.endswith(".rs"):
                    file_path = Path(root) / f
                    rel_file = file_path.relative_to(cp)
                    try:
                        with open(file_path, "r", encoding="utf-8", errors="ignore") as fp:
                            for idx, line in enumerate(fp, 1):
                                if is_rust_unsafe_keyword(line):
                                    unsafe_count += 1
                                    loc = f"{rel_file}:{idx}"
                                    unsafe_locations.append(loc)
                                    if not is_boundary:
                                        violations.append(
                                            f"{c} ({loc}): Forbidden unsafe usage: {line.strip()}"
                                        )
                    except Exception:
                        pass

        results[c] = {
            "is_boundary": is_boundary,
            "has_forbid_unsafe": has_forbid,
            "unsafe_count": unsafe_count,
            "sample_locations": unsafe_locations[:5],
        }

    return {
        "violations": violations,
        "crates": results,
    }


def audit_dependencies(root_dir: Path) -> dict:
    """Audit dependencies from Cargo manifests and lockfile."""
    violations: list[str] = []
    crates_dir = root_dir / "crates"
    cargo_lock_path = root_dir / "Cargo.lock"

    # Read lockfile for transitive analysis if available
    lock_packages: dict[str, set[str]] = {}
    if cargo_lock_path.exists():
        try:
            lock_data = tomllib.loads(cargo_lock_path.read_text(encoding="utf-8"))
            for pkg in lock_data.get("package", []):
                pkg_name = pkg.get("name", "")
                deps = set()
                for d in pkg.get("dependencies", []):
                    # Format in lockfile: "dep_name" or "dep_name 1.0.0 (registry+...)"
                    d_name = d.split()[0]
                    deps.add(d_name)
                lock_packages[pkg_name] = deps
        except Exception:
            pass

    # Check forbidden dependencies in workspace lockfile
    for forbidden in FORBIDDEN_DEPENDENCIES:
        if forbidden in lock_packages:
            violations.append(
                f"Global: Forbidden runtime dependency '{forbidden}' found in Cargo.lock"
            )

    crate_deps: dict[str, dict] = {}
    if crates_dir.exists():
        for cp in sorted(crates_dir.glob("*/Cargo.toml")):
            crate_name = cp.parent.name
            try:
                data = tomllib.loads(cp.read_text(encoding="utf-8"))
            except Exception:
                continue

            direct_deps = set(data.get("dependencies", {}).keys())
            build_deps = set(data.get("build-dependencies", {}).keys())
            dev_deps = set(data.get("dev-dependencies", {}).keys())

            # Target-specific dependencies
            target_data = data.get("target", {})
            target_deps: dict[str, list[str]] = {}
            for target_cfg, target_cfg_data in target_data.items():
                t_deps = list(target_cfg_data.get("dependencies", {}).keys())
                target_deps[target_cfg] = t_deps
                direct_deps.update(t_deps)

            # Check forbidden direct dependencies
            for forbidden in FORBIDDEN_DEPENDENCIES:
                if forbidden in direct_deps or forbidden in build_deps:
                    violations.append(
                        f"{crate_name}: Forbidden runtime dependency '{forbidden}' declared"
                    )

            # Check daemon rules
            if crate_name == "frd":
                for gui_crate in DAEMON_GUI_CRATES:
                    if gui_crate in direct_deps:
                        violations.append(
                            f"frd: Daemon cannot inherit GUI crate '{gui_crate}'"
                        )

            # Check browser crate rules (if fr-web or wasm target)
            if crate_name == "fr-web" or "wasm" in crate_name:
                for ffi_crate in DESKTOP_FFI_CRATES:
                    if ffi_crate in direct_deps:
                        violations.append(
                            f"{crate_name}: Browser build cannot inherit desktop FFI crate '{ffi_crate}'"
                        )

            # Collect transitive dependencies from lockfile
            transitive = set()
            queue = list(direct_deps)
            visited = set(direct_deps)
            while queue:
                curr = queue.pop(0)
                for next_dep in lock_packages.get(curr, set()):
                    if next_dep not in visited:
                        visited.add(next_dep)
                        transitive.add(next_dep)
                        queue.append(next_dep)

            crate_deps[crate_name] = {
                "direct": sorted(direct_deps),
                "build": sorted(build_deps),
                "dev": sorted(dev_deps),
                "transitive": sorted(transitive),
                "target_specific": target_deps,
            }

    return {
        "violations": violations,
        "crates": crate_deps,
    }


def _split_cfg_args(text: str) -> list[str]:
    """Split top-level comma-separated arguments inside a cfg(...) predicate."""
    args: list[str] = []
    depth = 0
    in_quotes = False
    start = 0
    for idx, ch in enumerate(text):
        if ch == '"' and (idx == 0 or text[idx - 1] != "\\"):
            in_quotes = not in_quotes
        elif not in_quotes:
            if ch == "(":
                depth += 1
            elif ch == ")":
                depth -= 1
            elif ch == "," and depth == 0:
                piece = text[start:idx].strip()
                if piece:
                    args.append(piece)
                start = idx + 1
    tail = text[start:].strip()
    if tail:
        args.append(tail)
    return args


def _eval_cfg_predicate(expr: str, info: dict) -> bool:
    """Evaluate a Rust cfg(...) boolean expression against target properties."""
    expr = expr.strip()
    if not expr:
        return True
    if expr.startswith("all(") and expr.endswith(")"):
        return all(_eval_cfg_predicate(arg, info) for arg in _split_cfg_args(expr[4:-1]))
    if expr.startswith("any(") and expr.endswith(")"):
        return any(_eval_cfg_predicate(arg, info) for arg in _split_cfg_args(expr[4:-1]))
    if expr.startswith("not(") and expr.endswith(")"):
        return not _eval_cfg_predicate(expr[4:-1], info)

    if "=" in expr:
        key, raw_val = expr.split("=", 1)
        key = key.strip()
        val = raw_val.strip().strip('"').strip("'")
        if key == "target_os":
            return info["target_os"] == val
        if key == "target_arch":
            return info["target_arch"] == val
        if key == "target_family":
            return info["target_family"] == val
        if key == "target_env":
            return info["target_env"] == val
        if key == "target_vendor":
            return info["target_vendor"] == val
        # Unknown key: fail-closed (assume predicate could match so forbidden deps are not hidden).
        return True

    if expr == "unix":
        return info["target_family"] == "unix"
    if expr == "windows":
        return info["target_family"] == "windows"
    # Unknown bare cfg flag (e.g. feature/custom): fail-closed.
    return True


def eval_target_cfg(target_cfg: str, target_triple: str, info: dict) -> bool:
    """Return True if a Cargo `[target.<target_cfg>]` section applies to `target_triple`."""
    spec = target_cfg.strip()
    if spec == target_triple:
        return True
    if spec.startswith("cfg(") and spec.endswith(")"):
        return _eval_cfg_predicate(spec[4:-1], info)
    # Bare target triple that does not match this target_triple.
    if "-" in spec and "(" not in spec:
        return False
    return True


def _resolve_target_crate_deps(
    manifests: dict[str, dict],
    target_triple: str,
    info: dict,
    include_optional: bool = False,
) -> dict[str, set[str]]:
    """Compute direct active dependencies per workspace crate for `target_triple`."""
    active: dict[str, set[str]] = {}
    for crate_name, data in manifests.items():
        deps: set[str] = set()
        for dep_name, dep_spec in data.get("dependencies", {}).items():
            if not include_optional and isinstance(dep_spec, dict) and dep_spec.get("optional"):
                continue
            deps.add(dep_name)
        for target_cfg, target_cfg_data in data.get("target", {}).items():
            if not eval_target_cfg(target_cfg, target_triple, info):
                continue
            for dep_name, dep_spec in target_cfg_data.get("dependencies", {}).items():
                if not include_optional and isinstance(dep_spec, dict) and dep_spec.get("optional"):
                    continue
                deps.add(dep_name)
        active[crate_name] = deps
    return active


def _workspace_closure(start: str, active_deps: dict[str, set[str]]) -> set[str]:
    """Compute transitive dependencies starting from `start` through workspace manifests."""
    closure: set[str] = set()
    queue = list(active_deps.get(start, set()))
    visited = set(queue)
    while queue:
        curr = queue.pop(0)
        closure.add(curr)
        if curr in active_deps:
            for nxt in active_deps[curr]:
                if nxt not in visited:
                    visited.add(nxt)
                    queue.append(nxt)
    return closure


def audit_targets(root_dir: Path) -> dict:
    """Audit cfg-resolved per-target dependency graphs: linux, macos, windows, wasm32, ios, android."""
    targets = {
        "x86_64-unknown-linux-gnu": {
            "os": "linux",
            "desktop": True,
            "target_os": "linux",
            "target_arch": "x86_64",
            "target_family": "unix",
            "target_env": "gnu",
            "target_vendor": "unknown",
        },
        "aarch64-apple-darwin": {
            "os": "macos",
            "desktop": True,
            "target_os": "macos",
            "target_arch": "aarch64",
            "target_family": "unix",
            "target_env": "",
            "target_vendor": "apple",
        },
        "x86_64-pc-windows-msvc": {
            "os": "windows",
            "desktop": True,
            "target_os": "windows",
            "target_arch": "x86_64",
            "target_family": "windows",
            "target_env": "msvc",
            "target_vendor": "pc",
        },
        "wasm32-unknown-unknown": {
            "os": "browser",
            "desktop": False,
            "target_os": "unknown",
            "target_arch": "wasm32",
            "target_family": "wasm",
            "target_env": "",
            "target_vendor": "unknown",
        },
        "aarch64-apple-ios": {
            "os": "ios",
            "desktop": False,
            "target_os": "ios",
            "target_arch": "aarch64",
            "target_family": "unix",
            "target_env": "",
            "target_vendor": "apple",
        },
        "aarch64-linux-android": {
            "os": "android",
            "desktop": False,
            "target_os": "android",
            "target_arch": "aarch64",
            "target_family": "unix",
            "target_env": "",
            "target_vendor": "linux",
        },
    }

    crates_dir = root_dir / "crates"
    manifests: dict[str, dict] = {}
    if crates_dir.exists():
        for cp in sorted(crates_dir.glob("*/Cargo.toml")):
            try:
                manifests[cp.parent.name] = tomllib.loads(cp.read_text(encoding="utf-8"))
            except Exception:
                continue

    # Non-desktop targets enforce zero desktop-FFI contamination across portable/mobile/web crates.
    non_desktop_checked_crates = {
        "fr-core",
        "fr-wire",
        "fr-media",
        "fr-client",
        "fr-transport",
        "fr-web",
        "fr-mobile-ffi",
    }

    target_results: dict[str, dict] = {}
    violations: list[str] = []

    for target_triple, info in targets.items():
        is_desktop = info["desktop"]
        active_deps = _resolve_target_crate_deps(manifests, target_triple, info, include_optional=False)
        target_violations: list[str] = []
        active_desktop_ffi: set[str] = set()
        daemon_gui_deps: set[str] = set()
        crates_checked: list[str] = []

        # 1. Every crate active on this target must never pull a forbidden runtime.
        for crate_name in sorted(manifests):
            closure = _workspace_closure(crate_name, active_deps)
            for forbidden in sorted(FORBIDDEN_DEPENDENCIES & closure):
                msg = f"{target_triple} ({crate_name}): Forbidden runtime dependency '{forbidden}' active on target"
                target_violations.append(msg)

        # 2. Daemon (`frd`) must never inherit GUI crates or desktop-client FFI (`fr-native`, `fr-windows`).
        if "frd" in manifests:
            crates_checked.append("frd")
            frd_closure = _workspace_closure("frd", active_deps)
            for bad_gui in sorted((DAEMON_GUI_CRATES | {"fr-native", "fr-windows"}) & frd_closure):
                daemon_gui_deps.add(bad_gui)
                target_violations.append(
                    f"{target_triple} (frd): Daemon inherits forbidden GUI/FFI dependency '{bad_gui}'"
                )

        # 3. Non-desktop targets (browser, iOS, Android) must never inherit desktop FFI crates.
        if not is_desktop:
            for crate_name in sorted(manifests):
                if crate_name not in non_desktop_checked_crates and "wasm" not in crate_name:
                    continue
                if crate_name not in crates_checked:
                    crates_checked.append(crate_name)
                closure = _workspace_closure(crate_name, active_deps)
                for ffi_crate in sorted(DESKTOP_FFI_CRATES & closure):
                    active_desktop_ffi.add(f"{crate_name}->{ffi_crate}")
                    target_violations.append(
                        f"{target_triple} ({crate_name}): Non-desktop target inherits desktop FFI crate '{ffi_crate}'"
                    )

        violations.extend(target_violations)
        target_results[target_triple] = {
            "os": info["os"],
            "desktop_ffi_allowed": is_desktop,
            "gui_allowed_in_daemon": False,
            "crates_checked": sorted(crates_checked),
            "active_desktop_ffi_deps": sorted(active_desktop_ffi),
            "daemon_gui_deps": sorted(daemon_gui_deps),
            "violation_count": len(target_violations),
            "compliant": len(target_violations) == 0,
        }

    return {
        "violations": violations,
        "targets": target_results,
    }


def format_report(unsafe_report: dict, dep_report: dict, target_report: dict) -> str:
    """Format human-readable audit report."""
    lines = [
        "================================================================================",
        "          FRANKENREMOTE TARGET DEPENDENCY & UNSAFE AUDIT",
        "================================================================================",
        "",
        "1. MEMORY-SAFETY BOUNDARY AUDIT (#![forbid(unsafe_code)]):",
        "--------------------------------------------------------------------------------",
    ]

    for crate, data in sorted(unsafe_report["crates"].items()):
        is_boundary = data["is_boundary"]
        forbid_str = "YES" if data["has_forbid_unsafe"] else "NO"
        unsafe_cnt = data["unsafe_count"]

        if is_boundary:
            status = f"BOUNDARY CRATE ({unsafe_cnt} unsafe occurrences, documented safe wrapper)"
        elif unsafe_cnt == 0 and data["has_forbid_unsafe"]:
            status = "CLEAN (forbid(unsafe_code) enforced, 0 unsafe)"
        else:
            status = f"VIOLATION ({unsafe_cnt} unsafe occurrences, forbid={forbid_str})"

        lines.append(f"  {crate:<16}: {status}")

    lines.extend([
        "",
        "2. DEPENDENCY GRAPH & CLASSIFICATION BY CRATE:",
        "--------------------------------------------------------------------------------",
    ])

    for crate, data in sorted(dep_report["crates"].items()):
        n_direct = len(data["direct"])
        n_build = len(data["build"])
        n_dev = len(data["dev"])
        n_trans = len(data["transitive"])
        lines.append(
            f"  {crate:<16}: {n_direct:>2} direct | {n_build:>2} build | "
            f"{n_dev:>2} test-only | {n_trans:>2} transitive"
        )

    lines.extend([
        "",
        "3. TARGET-SPECIFIC AUDIT MATRIX:",
        "--------------------------------------------------------------------------------",
    ])

    for target, data in sorted(target_report["targets"].items()):
        desktop_ffi = "Permitted" if data["desktop_ffi_allowed"] else "FORBIDDEN (No desktop FFI)"
        lines.append(f"  {target:<28}: OS={data['os']:<7} | FFI: {desktop_ffi}")

    lines.extend([
        "================================================================================",
    ])

    all_violations = (
        unsafe_report["violations"]
        + dep_report["violations"]
        + target_report["violations"]
    )

    if all_violations:
        lines.append(f"AUDIT VERDICT: REFUSAL - {len(all_violations)} violation(s) detected:")
        for v in all_violations:
            lines.append(f"  [X] {v}")
    else:
        lines.append("AUDIT VERDICT: PASS - All memory-safety, runtime, and target boundaries respected.")

    lines.append("================================================================================")
    return "\n".join(lines)


def main() -> int:
    parser = argparse.ArgumentParser(description="Target dependency and unsafe audit tool")
    parser.add_argument("--root", type=Path, default=Path(__file__).resolve().parent.parent,
                        help="Root directory to scan (default: repository root)")
    parser.add_argument("--fixture", type=Path, default=None,
                        help="Scan a fixture directory instead of repo root")
    parser.add_argument("--json", action="store_true",
                        help="Output JSON summary")
    parser.add_argument("--check", action="store_true", default=True,
                        help="Exit nonzero on audit violation")

    args = parser.parse_args()
    scan_root = args.fixture if args.fixture else args.root

    if not scan_root.exists():
        sys.stderr.write(f"Error: path does not exist: {scan_root}\n")
        return 2

    unsafe_report = audit_unsafe(scan_root)
    dep_report = audit_dependencies(scan_root)
    target_report = audit_targets(scan_root)

    all_violations = (
        unsafe_report["violations"]
        + dep_report["violations"]
        + target_report["violations"]
    )

    if args.json:
        out = {
            "unsafe": unsafe_report,
            "dependencies": dep_report,
            "targets": target_report,
            "violations": all_violations,
            "passed": len(all_violations) == 0,
        }
        print(json.dumps(out, indent=2))
    else:
        print(format_report(unsafe_report, dep_report, target_report))

    if args.check and all_violations:
        sys.stderr.write(f"\nAUDIT_REFUSAL: {len(all_violations)} violation(s) detected:\n")
        for v in all_violations:
            sys.stderr.write(f"  - {v}\n")
        return 1

    return 0


if __name__ == "__main__":
    sys.exit(main())
