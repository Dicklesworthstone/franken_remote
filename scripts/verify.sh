#!/usr/bin/env bash
# Repository-owned verification lanes for FrankenRemote.
#
# This script is the authoritative entrypoint; workflow YAML and dsr/act call
# it and contain no correctness logic of their own (AGENTS.md section 10).
# Pre-implementation, only the docs lane exists. Code lanes appear together
# with the first workspace crate; until then they refuse honestly instead of
# pretending absent gates passed.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

lane="${1:-}"

docs_lane() {
  local failures=0
  local files
  files=$(find . -maxdepth 2 -name '*.md' -not -path './target/*' -not -path './.git/*' | sort)

  for f in $files; do
    # Extract inline markdown link targets: [text](target)
    while IFS= read -r target; do
      # Skip absolute URLs, mail links, and pure in-page anchors.
      case "$target" in
        http://*|https://*|mailto:*|\#*) continue ;;
      esac
      # Strip any trailing anchor.
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

  # Files the constitution requires to exist at the repository root.
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

case "$lane" in
  docs)
    docs_lane
    ;;
  fast|full|release)
    echo "verify.sh: lane '$lane' is dormant: no workspace crates exist yet (spec-first phase)." >&2
    echo "verify.sh: refusing rather than reporting a pass for gates that did not run." >&2
    exit 2
    ;;
  *)
    echo "usage: scripts/verify.sh <docs|fast|full|release>" >&2
    exit 2
    ;;
esac
