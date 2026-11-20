//! Deterministic simulation suite (`fr-rc2-deterministic-simulation-5pww`).
//!
//! Proves all 7 host-authority and multi-peer demux invariants across 200
//! deterministic seeds in Asupersync virtual lab time (zero wall-clock timing),
//! proves bit-exact reproducibility, and verifies that 3 planted scheduling/demux
//! bugs are reliably caught with exact failing seeds printed.

use fr_lab::simulation::{PlantedBug, SimConfig, SimInvariant, run_deterministic_simulation};

#[test]
fn production_host_and_multipeer_demux_pass_200_deterministic_seeds() {
    let mut total_grants = 0u64;
    let mut total_inputs = 0u64;
    let mut total_demotions = 0u64;
    let mut total_spoofs_refused = 0u64;

    for seed in 1_u64..=200 {
        let cfg = SimConfig::standard(seed);
        let summary = run_deterministic_simulation(cfg)
            .unwrap_or_else(|v| panic!("unexpected simulation failure: {v}"));
        assert_eq!(summary.seed, seed);
        assert_eq!(summary.steps_executed, cfg.steps);
        total_grants += u64::from(summary.control_grants);
        total_inputs += u64::from(summary.inputs_submitted);
        total_demotions += u64::from(summary.control_demotions);
        total_spoofs_refused += u64::from(summary.spoofs_refused);
    }

    // Confirm the simulator actually exercised meaningful traffic across the 200 seeds
    // (never a vacuous green loop!).
    assert!(
        total_grants >= 200,
        "expected >= 200 control grants, got {total_grants}"
    );
    assert!(
        total_inputs >= 50,
        "expected >= 50 submitted inputs, got {total_inputs}"
    );
    assert!(
        total_demotions >= 50,
        "expected >= 50 control->view demotions, got {total_demotions}"
    );
    assert!(
        total_spoofs_refused >= 50,
        "expected >= 50 cross-peer spoof refusals, got {total_spoofs_refused}"
    );
}

#[test]
fn identical_seed_reproduces_bit_exact_summary_and_trace_digest() {
    let first = run_deterministic_simulation(SimConfig::standard(0xDEAD_BEEF_1337)).unwrap();
    let second = run_deterministic_simulation(SimConfig::standard(0xDEAD_BEEF_1337)).unwrap();
    assert_eq!(first, second);
    assert_ne!(first.trace_digest, 0);

    let different = run_deterministic_simulation(SimConfig::standard(0xDEAD_BEEF_1338)).unwrap();
    assert_ne!(first.trace_digest, different.trace_digest);
}

#[test]
fn simulator_catches_planted_scheduling_and_demux_bugs_with_reproducible_seed() {
    // 1. SkipCleanupBeforeHandoff must be caught under CleanReleaseBeforeHandoff.
    let mut caught_handoff = None;
    for seed in 1_u64..=100 {
        let mut cfg = SimConfig::standard(seed);
        cfg.planted_bug = PlantedBug::SkipCleanupBeforeHandoff;
        if let Err(v) = run_deterministic_simulation(cfg) {
            assert_eq!(v.seed, seed);
            assert_eq!(v.invariant, SimInvariant::CleanReleaseBeforeHandoff);
            assert!(v.to_string().contains(&format!("seed={seed}")));
            // Re-running the exact same seed reproduces the identical violation.
            let replay = run_deterministic_simulation(cfg).unwrap_err();
            assert_eq!(v, replay);
            caught_handoff = Some(seed);
            break;
        }
    }
    assert!(
        caught_handoff.is_some(),
        "simulator failed to catch SkipCleanupBeforeHandoff within 100 seeds"
    );

    // 2. CrossPeerDemuxLeak must be caught under DemuxIsolation.
    let mut caught_demux = None;
    for seed in 1_u64..=100 {
        let mut cfg = SimConfig::standard(seed);
        cfg.planted_bug = PlantedBug::CrossPeerDemuxLeak;
        if let Err(v) = run_deterministic_simulation(cfg) {
            assert_eq!(v.seed, seed);
            assert_eq!(v.invariant, SimInvariant::DemuxIsolation);
            let replay = run_deterministic_simulation(cfg).unwrap_err();
            assert_eq!(v, replay);
            caught_demux = Some(seed);
            break;
        }
    }
    assert!(
        caught_demux.is_some(),
        "simulator failed to catch CrossPeerDemuxLeak within 100 seeds"
    );

    // 3. SkipSuspendFence must be caught under SubmissionTimeAuthorityFence.
    let mut caught_suspend = None;
    for seed in 1_u64..=100 {
        let mut cfg = SimConfig::standard(seed);
        cfg.planted_bug = PlantedBug::SkipSuspendFence;
        if let Err(v) = run_deterministic_simulation(cfg) {
            assert_eq!(v.seed, seed);
            assert_eq!(v.invariant, SimInvariant::SubmissionTimeAuthorityFence);
            let replay = run_deterministic_simulation(cfg).unwrap_err();
            assert_eq!(v, replay);
            caught_suspend = Some(seed);
            break;
        }
    }
    assert!(
        caught_suspend.is_some(),
        "simulator failed to catch SkipSuspendFence within 100 seeds"
    );
}
