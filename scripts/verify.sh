#!/usr/bin/env bash
# Repository-owned verification lanes. CI and local builders call the same
# commands; Rust tests are not evidence of native media or wire qualification.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"
export RCH_CARGO_WRAPPER_BYPASS=1

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

rust_lane() {
  if [ ! -f Cargo.toml ]; then
    echo "Rust lane: BLOCKED (workspace manifest missing)" >&2
    return 2
  fi
  cargo fmt --all --check
  cargo check --workspace --all-targets --all-features --locked
  cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
  cargo test --workspace --all-features --locked
  cargo test --workspace --all-features --locked --examples
  echo "Rust lane: passed (format/check/clippy/tests, including test-only media contracts)"
}

case "$lane" in
  docs)
    docs_lane
    ;;
  fast)
    rust_lane
    ;;
  full)
    rust_lane
    docs_lane
    if ! command -v ubs >/dev/null 2>&1; then
      echo "full lane: BLOCKED (ubs is not installed; Rust/docs gates ran above)" >&2
      exit 2
    fi
    # Scan the source tree, not an empty git diff after a clean checkout.
    ubs .
    echo "full source lane: passed; native/wire/hardware qualification is separate"
    ;;
  release)
    echo "release lane: BLOCKED (native artifacts and qualification matrix are not implemented)" >&2
    exit 2
    ;;
  *)
    echo "usage: scripts/verify.sh <docs|fast|full|release>" >&2
    exit 2
    ;;
esac
