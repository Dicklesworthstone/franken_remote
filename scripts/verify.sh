#!/usr/bin/env bash
# Repository-owned verification lanes for FrankenRemote.
# CI and local builders call the same commands; Rust tests are not evidence of
# native media or wire qualification.
#
# Commands:
#   scripts/verify.sh fast                  - Quick Rust check: fmt, check, clippy, tests, examples
#   scripts/verify.sh fmt|check|clippy|test|examples - One fast-lane step (CI runs each independently)
#   scripts/verify.sh docs                  - Verify docs links and mandatory core design files
#   scripts/verify.sh count [--fixture DIR] - Run the fixed line counter & size discipline audit
#   scripts/verify.sh audit [--fixture DIR] - Feature-resolved per-target dependency & unsafe audit
#   scripts/verify.sh crate <name> [action] - Per-crate verification (fast|check|clippy|test|fmt)
#   scripts/verify.sh test-fixtures         - Run planted over-budget and violation fixture suite
#   scripts/verify.sh full                  - Full suite: fast + docs + count + audit + ubs
#   scripts/verify.sh release               - Release gate (honestly refuses until implemented)

set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root" || exit 1
export RCH_CARGO_WRAPPER_BYPASS=1
export RUST_MIN_STACK="${RUST_MIN_STACK:-16777216}"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-${repo_root}/target}"

lane="${1:-}"

docs_lane() {
  local failures=0
  local files
  files=$(find . -maxdepth 2 -name '*.md' -not -path './target/*' -not -path './.git/*' | sort)

  for f in $files; do
    while IFS= read -r target; do
      case "$target" in
        http://*|https://*|mailto:*|\#*) continue ;;
      esac
      local path="${target%%#*}"
      [ -z "$path" ] && continue
      local dir
      dir="$(dirname "$f")"
      if [ ! -e "$dir/$path" ] && [ ! -e "$path" ]; then
        echo "BROKEN LINK: $f -> $target"
        failures=$((failures + 1))
      fi
    done < <(grep -oE '\]\(([^)]+)\)' "$f" | sed -E 's/^\]\((<)?//; s/(>)?\)$//')
  done

  for required in \
    COMPREHENSIVE_PLAN_FOR_THE_DESIGN_OF_FRANKENREMOTE.md \
    AGENTS.md README.md PROTOCOL.md SECURITY.md LICENSE rust-toolchain.toml; do
    if [ ! -e "$required" ]; then
      echo "MISSING REQUIRED FILE: $required"
      failures=$((failures + 1))
    fi
  done

  if [ "$failures" -ne 0 ]; then
    echo "docs lane: FAILED ($failures problem(s))"
    exit 1
  fi
  echo "docs lane: passed"
}

rust_step() {
  case "$1" in
    fmt) cargo fmt --all --check ;;
    check) cargo check --workspace --all-targets --all-features --locked ;;
    clippy) cargo clippy --workspace --all-targets --all-features --locked -- -D warnings ;;
    # --no-fail-fast: one failing test binary must not hide the rest.
    test) cargo test --workspace --all-features --locked --no-fail-fast ;;
    examples) cargo test --workspace --all-features --locked --examples --no-fail-fast ;;
    *) echo "unknown rust step: $1" >&2; return 2 ;;
  esac
}

rust_lane() {
  if [ ! -f Cargo.toml ]; then
    echo "Rust lane: BLOCKED (workspace manifest missing)" >&2
    return 2
  fi
  rust_step fmt
  rust_step check
  rust_step clippy
  rust_step test
  rust_step examples
  echo "Rust lane: passed (format/check/clippy/tests, including test-only media contracts)"
}

crate_lane() {
  local crate_name="${1:-}"
  local action="${2:-fast}"

  if [ -z "$crate_name" ]; then
    echo "error: crate name required for crate lane (e.g. scripts/verify.sh crate fr-core)" >&2
    exit 2
  fi

  if [ ! -d "crates/$crate_name" ]; then
    echo "error: crate 'crates/$crate_name' does not exist" >&2
    exit 2
  fi

  echo "=== Running crate verification: $crate_name ($action) ==="
  case "$action" in
    fmt)
      cargo fmt --check --manifest-path "crates/$crate_name/Cargo.toml"
      ;;
    check)
      cargo check -p "$crate_name" --all-targets --all-features --locked
      ;;
    clippy)
      cargo clippy -p "$crate_name" --all-targets --all-features --locked -- -D warnings
      ;;
    test)
      cargo test -p "$crate_name" --all-features --locked
      ;;
    fast)
      cargo check -p "$crate_name" --all-targets --all-features --locked
      cargo clippy -p "$crate_name" --all-targets --all-features --locked -- -D warnings
      cargo test -p "$crate_name" --all-features --locked
      ;;
    *)
      echo "unknown crate action: $action (valid: fast|check|clippy|test|fmt)" >&2
      exit 2
      ;;
  esac
  echo "=== Crate $crate_name ($action): passed ==="
}

count_lane() {
  shift || true
  python3 scripts/count.py "$@"
}

audit_lane() {
  shift || true
  python3 scripts/audit.py "$@"
}

test_fixtures_lane() {
  python3 scripts/test_verify_lanes.py
}

case "$lane" in
  docs)
    docs_lane
    ;;
  fast)
    rust_lane
    ;;
  fmt|check|clippy|test|examples)
    rust_step "$lane"
    ;;
  crate)
    shift
    crate_lane "$@"
    ;;
  count)
    count_lane "$@"
    ;;
  audit)
    audit_lane "$@"
    ;;
  test-fixtures)
    test_fixtures_lane
    ;;
  full)
    # Every lane runs; one failure never hides another. The verdict names them.
    failed=""
    for step in fmt check clippy test examples; do
      rust_step "$step" || failed="$failed $step"
    done
    ( docs_lane ) || failed="$failed docs"
    ( count_lane ) || failed="$failed count"
    ( audit_lane ) || failed="$failed audit"
    ( test_fixtures_lane ) || failed="$failed fixtures"
    if command -v ubs >/dev/null 2>&1; then
      # Scan the source tree, not an empty git diff after a clean checkout.
      ubs . || failed="$failed ubs"
    else
      echo "full lane: ubs BLOCKED (not installed)" >&2
      failed="$failed ubs(blocked)"
    fi
    if [ -n "$failed" ]; then
      echo "full lane: FAILED lanes:$failed" >&2
      exit 1
    fi
    echo "full source lane: passed; native/wire/hardware qualification is separate"
    ;;
  release)
    echo "release lane: BLOCKED (native artifacts and qualification matrix are not implemented)" >&2
    exit 2
    ;;
  *)
    echo "usage: scripts/verify.sh <docs|fast|fmt|check|clippy|test|examples|full|count|audit|crate|test-fixtures|release>" >&2
    exit 2
    ;;
esac
