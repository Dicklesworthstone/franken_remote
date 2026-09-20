#![forbid(unsafe_code)]
//! Integration and unit tests for the media worker lifecycle state machine (plan §§5.4, 10.2, 11.1).
//!
//! Acceptance criteria:
//! - Unit tests for worker lifecycle state machine:
//!   1. Spawn on demand
//!   2. Generation-fenced teardown
//!   3. Kill-on-stall with authority fencing FIRST
//!   4. Restart with bounded rate / exponential backoff
//!   5. Repeated GPU failure ends the media profile rather than spawning unlimited workers
//!   6. Permission loss / display removal / display-color changes as typed capability events
//!   7. Protected-content reported as such, never as a stalled network.

use asupersync::types::Time;
use frd::worker::lifecycle::{
    LifecycleError, ProfileDisableReason, TeardownStage, WorkerCapabilityEvent,
    WorkerLifecycleConfig, WorkerLifecycleStateMachine, WorkerState,
};
use std::time::Duration;

fn make_config() -> WorkerLifecycleConfig {
    WorkerLifecycleConfig {
        max_restarts: 3,
        max_gpu_failures: 3,
        initial_backoff: Duration::from_millis(100),
        max_backoff: Duration::from_millis(1000),
        restart_window: Duration::from_secs(10),
        stall_timeout: Duration::from_millis(1500),
    }
}

fn time_ms(ms: u64) -> Time {
    Time::from_nanos(ms * 1_000_000)
}

#[test]
fn spawn_on_demand_lifecycle_happy_path() {
    let mut sm = WorkerLifecycleStateMachine::new(make_config(), 100);
    assert_eq!(sm.state(), WorkerState::Idle);
    assert_eq!(sm.generation(), 100);
    assert!(!sm.is_authority_fenced());
    assert!(!sm.is_input_revoked());

    // 1. Spawn on demand
    let t0 = time_ms(1000);
    let spawn_gen = sm.request_spawn(t0).expect("request spawn");
    assert_eq!(spawn_gen, 101);
    assert_eq!(sm.state(), WorkerState::Spawning);

    // Cannot request spawn while already spawning
    assert!(matches!(
        sm.request_spawn(t0),
        Err(LifecycleError::InvalidTransition { .. })
    ));

    // 2. Spawn succeeds -> Configuring
    sm.on_spawn_success().expect("spawn success");
    assert_eq!(sm.state(), WorkerState::Configuring);

    // 3. Handshake/configuration complete -> Running
    let t1 = time_ms(1050);
    sm.on_configured(t1).expect("configured");
    assert_eq!(sm.state(), WorkerState::Running);
    assert_eq!(sm.consecutive_gpu_failures(), 0);

    // 4. Progress updates
    let t2 = time_ms(1100);
    sm.on_progress(t2).expect("progress");
    assert!(!sm.check_stall(time_ms(1200)));
}

#[test]
fn kill_on_stall_fences_authority_first() {
    let mut sm = WorkerLifecycleStateMachine::new(make_config(), 100);
    let t0 = time_ms(1000);
    sm.request_spawn(t0).unwrap();
    sm.on_spawn_success().unwrap();
    sm.on_configured(t0).unwrap();
    assert_eq!(sm.state(), WorkerState::Running);

    // Advance time past stall timeout (1500ms)
    let t_stalled = time_ms(3000);
    assert!(sm.check_stall(t_stalled), "should detect stall");
    assert_eq!(sm.state(), WorkerState::Stalled);

    // CRITICAL: Cannot terminate directly while Stalled without fencing authority first!
    let term_err = sm.terminate_after_fencing(t_stalled, false).unwrap_err();
    assert_eq!(term_err, LifecycleError::AuthorityNotFenced);

    // Authority must be fenced FIRST
    sm.fence_authority_first_on_stall().unwrap();
    assert_eq!(sm.state(), WorkerState::FencingAuthority);
    assert!(sm.is_authority_fenced(), "authority must be fenced");
    assert!(sm.is_input_revoked(), "input must be revoked");
    assert_eq!(
        sm.generation(),
        102,
        "generation must advance to fence late packets"
    );

    // Check that teardown log records authority revocation before worker termination
    let log = sm.teardown_log();
    assert_eq!(log[0], TeardownStage::RevokeInputAuthority);
    assert_eq!(log[1], TeardownStage::InvalidateGenerations);

    // Now terminate child process
    let new_state = sm.terminate_after_fencing(t_stalled, false).unwrap();
    assert_eq!(new_state, WorkerState::Backoff);

    // Complete teardown sequence verified in order
    let final_log = sm.teardown_log();
    assert_eq!(
        final_log,
        &[
            TeardownStage::RevokeInputAuthority,
            TeardownStage::InvalidateGenerations,
            TeardownStage::StopCaptureAdmission,
            TeardownStage::CancelCooperativeTasks,
            TeardownStage::DrainBoundedSends,
            TeardownStage::TerminateWorker,
            TeardownStage::ReapWorker,
            TeardownStage::Completed,
        ]
    );
}

#[test]
fn generation_fenced_teardown_strict_stage_order() {
    let mut sm = WorkerLifecycleStateMachine::new(make_config(), 200);
    sm.request_spawn(time_ms(1000)).unwrap();
    sm.on_spawn_success().unwrap();
    sm.on_configured(time_ms(1050)).unwrap();

    let stages = sm.execute_generation_fenced_teardown();
    assert_eq!(
        stages,
        &[
            TeardownStage::RevokeInputAuthority,
            TeardownStage::InvalidateGenerations,
            TeardownStage::StopCaptureAdmission,
            TeardownStage::CancelCooperativeTasks,
            TeardownStage::DrainBoundedSends,
            TeardownStage::TerminateWorker,
            TeardownStage::ReapWorker,
            TeardownStage::Completed,
        ]
    );
    assert_eq!(sm.state(), WorkerState::Idle);
    assert!(sm.is_input_revoked());
    assert!(sm.is_authority_fenced());
    assert_eq!(sm.generation(), 202);
}

#[test]
fn bounded_rate_and_exponential_backoff() {
    let mut sm = WorkerLifecycleStateMachine::new(make_config(), 300);
    let t0 = time_ms(1000);

    // First attempt fails at spawn
    sm.request_spawn(t0).unwrap();
    let state = sm.on_spawn_failure(t0, false);
    assert_eq!(state, WorkerState::Backoff);
    assert_eq!(sm.compute_backoff(), Duration::from_millis(200)); // attempt 1 -> 200ms

    // Immediate restart before backoff expiry (100ms) fails
    let err = sm.request_spawn(time_ms(1050)).unwrap_err();
    assert!(matches!(err, LifecycleError::BackoffNotExpired { .. }));

    // Restart after backoff expiry succeeds
    let t1 = time_ms(1200);
    sm.request_spawn(t1).unwrap();
    assert_eq!(sm.state(), WorkerState::Spawning);
}

#[test]
fn repeated_gpu_failure_ends_media_profile() {
    let mut sm = WorkerLifecycleStateMachine::new(make_config(), 400);

    // Failures 1 and 2
    for i in 1_u32..=2_u32 {
        let t = time_ms(1000 * u64::from(i));
        sm.request_spawn(t).unwrap();
        let state = sm.on_spawn_failure(t, true);
        assert_eq!(state, WorkerState::Backoff);
        assert_eq!(sm.consecutive_gpu_failures(), i);
    }

    // Failure 3 (reaches max_gpu_failures = 3)
    let t3 = time_ms(5000);
    sm.request_spawn(t3).unwrap();
    let state = sm.on_spawn_failure(t3, true);
    assert_eq!(
        state,
        WorkerState::ProfileDisabled(ProfileDisableReason::RepeatedGpuFailure)
    );
    assert!(sm.state().is_disabled());

    // Subsequent spawn requests are rejected with ProfileDisabled
    let err = sm.request_spawn(time_ms(10000)).unwrap_err();
    assert_eq!(
        err,
        LifecycleError::ProfileDisabled(ProfileDisableReason::RepeatedGpuFailure)
    );
}

#[test]
fn max_restarts_exceeded_disables_profile() {
    let mut config = make_config();
    config.max_restarts = 2; // Allow only 2 restarts in window
    config.restart_window = Duration::from_secs(10);
    let mut sm = WorkerLifecycleStateMachine::new(config, 500);

    // Attempt 1: failure
    sm.request_spawn(time_ms(1000)).unwrap();
    sm.on_spawn_failure(time_ms(1000), false);

    // Attempt 2: failure
    sm.request_spawn(time_ms(2000)).unwrap();
    sm.on_spawn_failure(time_ms(2000), false);

    // Attempt 3: exceeds max_restarts (2) within 10s window
    let err = sm.request_spawn(time_ms(3000)).unwrap_err();
    assert_eq!(
        err,
        LifecycleError::ProfileDisabled(ProfileDisableReason::MaxRestartsExceeded)
    );
}

#[test]
fn permission_loss_fences_authority_and_disables_profile() {
    let mut sm = WorkerLifecycleStateMachine::new(make_config(), 600);
    sm.request_spawn(time_ms(1000)).unwrap();
    sm.on_spawn_success().unwrap();
    sm.on_configured(time_ms(1050)).unwrap();
    assert_eq!(sm.state(), WorkerState::Running);

    // Handle typed PermissionLoss capability event
    let new_state = sm
        .handle_capability_event(&WorkerCapabilityEvent::PermissionLoss, time_ms(2000))
        .unwrap();

    assert_eq!(
        new_state,
        WorkerState::ProfileDisabled(ProfileDisableReason::PermissionDenied)
    );
    assert!(
        sm.is_input_revoked(),
        "input must be revoked on permission loss"
    );
    assert!(
        sm.is_authority_fenced(),
        "authority must be fenced on permission loss"
    );
    assert_eq!(sm.generation(), 602, "generation must advance");
}

#[test]
fn protected_content_reported_as_typed_event_never_stalls_network() {
    let mut sm = WorkerLifecycleStateMachine::new(make_config(), 700);
    sm.request_spawn(time_ms(1000)).unwrap();
    sm.on_spawn_success().unwrap();
    sm.on_configured(time_ms(1050)).unwrap();
    assert_eq!(sm.state(), WorkerState::Running);

    // Protected content detected
    let state = sm
        .handle_capability_event(
            &WorkerCapabilityEvent::ProtectedContentDetected,
            time_ms(1200),
        )
        .unwrap();

    assert_eq!(state, WorkerState::Running, "worker remains running");
    assert!(sm.is_protected_content_active());

    // Progress updates continue; stall is NOT triggered
    sm.on_progress(time_ms(1300)).unwrap();
    assert!(!sm.check_stall(time_ms(1400)));
}

#[test]
fn display_events_trigger_generation_fencing() {
    let mut sm = WorkerLifecycleStateMachine::new(make_config(), 800);
    sm.request_spawn(time_ms(1000)).unwrap();
    sm.on_spawn_success().unwrap();
    sm.on_configured(time_ms(1050)).unwrap();
    let gen_before = sm.generation();

    // Geometry change
    sm.handle_capability_event(
        &WorkerCapabilityEvent::GeometryChanged {
            new_width: 2560,
            new_height: 1440,
        },
        time_ms(1200),
    )
    .unwrap();
    assert_eq!(sm.generation(), gen_before + 1);

    // Color space change
    sm.handle_capability_event(
        &WorkerCapabilityEvent::DisplayColorChanged {
            new_color_space: "DisplayP3".into(),
        },
        time_ms(1300),
    )
    .unwrap();
    assert_eq!(sm.generation(), gen_before + 2);

    // Display removed
    sm.handle_capability_event(
        &WorkerCapabilityEvent::DisplayRemoved { display_id: 1 },
        time_ms(1400),
    )
    .unwrap();
    assert_eq!(sm.generation(), gen_before + 3);
}
