#!/usr/bin/env bash
# FrankenRemote End-to-End Test Suite Runner
# Usage:
#   scripts/e2e/run.sh [--scenario <name>] [--seed <num>] [--artifacts-dir <dir>]
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"

cd "${REPO_ROOT}"

SCENARIO="${1:-phase1_canonical}"
SEED="${2:-42}"
ARTIFACTS_DIR="${3:-artifacts}"

echo "========================================================="
echo " FrankenRemote E2E Session Harness"
echo " Scenario:      ${SCENARIO}"
echo " Seed:          ${SEED}"
echo " Artifacts dir: ${ARTIFACTS_DIR}"
echo "========================================================="

# Ensure artifacts directory exists
mkdir -p "${ARTIFACTS_DIR}"

# Run the test suite via cargo
RCH_CARGO_WRAPPER_BYPASS=1 cargo run -p fr-e2e --bin fr_e2e -- \
    --scenario "${SCENARIO}" \
    --seed "${SEED}" \
    --artifacts-dir "${ARTIFACTS_DIR}"
