#!/usr/bin/env bash
# FrankenRemote End-to-End Test Suite Runner
# Usage:
#   scripts/e2e/run.sh [--scenario <name>] [--seed <num>] [--artifacts-dir <dir>]
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"

cd "${REPO_ROOT}"

SCENARIO="phase1_canonical"
SEED="42"
ARTIFACTS_DIR="artifacts"

while [[ $# -gt 0 ]]; do
    case "$1" in
        --scenario)
            SCENARIO="$2"
            shift 2
            ;;
        --seed)
            SEED="$2"
            shift 2
            ;;
        --artifacts-dir)
            ARTIFACTS_DIR="$2"
            shift 2
            ;;
        -h|--help)
            echo "Usage: scripts/e2e/run.sh [--scenario <name>] [--seed <num>] [--artifacts-dir <dir>]"
            exit 0
            ;;
        *)
            if [[ -z "${POSITIONAL_SCENARIO:-}" ]]; then
                SCENARIO="$1"
                POSITIONAL_SCENARIO=1
            elif [[ -z "${POSITIONAL_SEED:-}" ]]; then
                SEED="$1"
                POSITIONAL_SEED=1
            elif [[ -z "${POSITIONAL_ARTIFACTS:-}" ]]; then
                ARTIFACTS_DIR="$1"
                POSITIONAL_ARTIFACTS=1
            fi
            shift
            ;;
    esac
done

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
