# `fr-e2e` — FrankenRemote End-to-End Session Harness

`fr-e2e` is the runnable spine for all integration and fault-injection scenarios across FrankenRemote. It launches and scripts real client and host sessions, captures structured event logs (JSONL), evaluates behavioral assertions, and preserves complete reproduction artifact bundles.

---

## 1. Architecture & Scope

Per **Plan Section 23 (Phase 1 Gate)** and **Section 24.2 (Native Fault Tests)**:
- **Real Sessions, No Mocks**: Executes production state machines (`fr_client::ClientSession` and `frd::broker::SessionRegistry`).
- **Data-Driven Scenarios**: Scenarios script sessions as data (`ScenarioStep`), decoupling scenario definitions from execution mechanics.
- **Machine-Readable Structured Logging**: Chronological events are emitted to `events.jsonl` covering authority transitions, queue depth bounds, input stage dispositions, and recovery events.
- **Standard Fault Injectors**: Kill/stall workers, drop transport, expire tickets, resize displays, and planted violations.
- **Assertion Invariants**:
  - `assert_no_input_executed_after_revoke`: Verifies that no OS input submission occurs after authority is revoked.
  - `assert_queue_high_water`: Verifies that all queues remain bounded by count and bytes.
  - `assert_no_lease_resurrection_on_reconnect`: Verifies that reconnect preserves viewing but never resurrects an input lease without a fresh grant.
  - `assert_orderly_teardown`: Verifies the sequence: revoke -> release remote keys -> fence generations -> close transport.

---

## 2. Artifact Bundle & Single-Command Reproduction

Every execution produces a run folder under `artifacts/run_<scenario>_<timestamp>_<seed>/`:
```
artifacts/run_phase1_canonical_1726770000_42/
├── manifest.json      # Commit hash, configuration, seed, platform metadata
├── events.jsonl       # Structured event log in JSONL
├── summary.json       # Detailed pass/fail summary and assertion results
├── host_stderr.log    # Captured host process stderr
├── client_stderr.log  # Captured client process stderr
└── reproduce.sh       # Executable reproduction script
```

### Reproducing a Failed Run
To reproduce any run exactly as it executed, execute the generated reproduction script:
```bash
./artifacts/run_phase1_canonical_1726770000_42/reproduce.sh
```
Or directly via the runner:
```bash
scripts/e2e/run.sh phase1_canonical 42
```

---

## 3. Scenario Registration Convention

Every feature bead from Phase 1 onward must land at least one scenario exercising its behavior.

To register a new scenario:
1. Define a function in `crates/fr-e2e/src/scenario.rs` returning a `Scenario`:
   ```rust
   pub fn my_feature_scenario(seed: u64) -> Scenario {
       ScenarioBuilder::new("my_feature")
           .seed(seed)
           .connect(5000)
           .authorize("prompt_always", 5000)
           // add steps specific to feature
           .teardown()
           .build()
   }
   ```
2. Register the name in `crates/fr-e2e/src/bin/fr_e2e.rs` under the `--scenario` match statement.
3. Add an integration test in `crates/fr-e2e/tests/e2e_harness.rs`.

---

## 4. Planted Violation Sensitivity (Harness Failure Proof)

Acceptance requires demonstrating that the assertion layer actually catches violations. The harness includes a planted violation scenario (`phase1_planted_violation`):
```bash
cargo run -p fr-e2e --bin fr_e2e -- --scenario phase1_planted_violation
```
This scenario deliberately delays the revoke fence and submits an input action during the delayed window. The assertion layer detects the late submission and exits with a failure code, recording the offending event in `summary.json`.

---

## 5. CI Lane Wiring

Add to CI verification workflow (`scripts/verify.sh` or GitHub Actions):
```bash
# 1. Canonical Phase 1 integration run (must pass)
scripts/e2e/run.sh phase1_canonical 42

# 2. Planted violation sensitivity test (must catch violation and exit non-zero)
if scripts/e2e/run.sh phase1_planted_violation 42; then
    echo "ERROR: Planted violation was not caught by the assertion layer!"
    exit 1
else
    echo "SUCCESS: Planted violation was correctly detected and failed."
fi
```
