//! The real input owner must account for time spent obtaining the policy lock.
use super::*;
use fr_core::authority::AuthorityError;
use std::{
    cell::Cell,
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc,
    },
    thread,
    time::Duration,
};

struct WaitSink<'a> {
    inner: &'a mut Sink,
    wait_before: Operation,
    wait_next_sample: &'a Cell<bool>,
    deadline: Option<HostInstant>,
}

impl InputSink for WaitSink<'_> {
    fn prepare(&mut self, op: Operation) -> Result<(), PlatformError> {
        self.inner.prepare(op)?;
        self.wait_next_sample.set(op == self.wait_before);
        Ok(())
    }

    fn submit(&mut self, op: Operation) -> Submission {
        self.inner.submit(op)
    }

    fn submit_until(&mut self, op: Operation, until: HostInstant) -> Submission {
        self.deadline = Some(until);
        self.inner.submit_until(op, until)
    }

    fn cancel_prepared(&mut self) {
        self.inner.cancel_prepared();
    }

    fn repeat_requires_pair(&self) -> bool {
        self.inner.repeat_requires_pair()
    }
}

fn key(transition: KeyTransition) -> Operation {
    Operation::Key {
        key: PhysicalKey::new(4).unwrap(),
        transition,
    }
}

/// Retain an old clock snapshot while another thread owns the actual authority
/// mutex. The policy turn advances time without modifying authority: taking
/// max(snapshot, last policy time) therefore cannot account for the delay.
/// Channel handoffs make this deterministic without sleeps or elapsed-time tests.
fn through_policy_turn(
    owner: &mut InputSession,
    shared: &Arc<Mutex<SessionAuthority>>,
    request: InputRequest<'_>,
    sink: &mut Sink,
    wait_before: Operation,
    elapsed_until: u64,
) -> (Receipt, Option<HostInstant>) {
    let now = Arc::new(AtomicU64::new(0));
    let (start_tx, start_rx) = mpsc::sync_channel(1);
    let (locked_tx, locked_rx) = mpsc::sync_channel(1);
    let (sampled_tx, sampled_rx) = mpsc::sync_channel(1);
    let policy = shared.clone();
    let policy_now = now.clone();
    let timeout = Duration::from_secs(5);
    let holder = thread::spawn(move || {
        start_rx.recv_timeout(timeout).unwrap();
        let guard = policy.lock().unwrap();
        locked_tx.send(()).unwrap();
        sampled_rx.recv_timeout(timeout).unwrap();
        policy_now.store(elapsed_until, Ordering::Release);
        drop(guard);
    });
    let wait_next_sample = Cell::new(false);
    let mut boundary = WaitSink {
        inner: sink,
        wait_before,
        wait_next_sample: &wait_next_sample,
        deadline: None,
    };
    let mut passed_policy_turn = false;
    let result = owner.dispatch(request, &mut boundary, || {
        if wait_next_sample.replace(false) {
            start_tx.send(()).unwrap();
            locked_rx.recv_timeout(timeout).unwrap();
            assert!(shared.try_lock().is_err(), "policy turn must own the lock");
            let sampled = now.load(Ordering::Acquire);
            sampled_tx.send(()).unwrap();
            passed_policy_turn = true;
            HostInstant::from_micros(sampled)
        } else {
            // The fresh sample must run after policy authorization and outside
            // its locks; a caller clock is never an operation under that mutex.
            if passed_policy_turn {
                assert!(shared.try_lock().is_ok(), "clock called under policy lock");
            }
            HostInstant::from_micros(now.load(Ordering::Acquire))
        }
    });
    holder.join().unwrap();
    (receipt(result.unwrap()), boundary.deadline)
}

#[test]
fn expiry_during_policy_lock_turn_cancels_preparation_without_press_or_replay() {
    let (mut owner, credentials, shared) = setup_shared();
    let mut sink = Sink::default();
    let action = request(credentials, 0, KeyTransition::Press);
    let (result, deadline) = through_policy_turn(
        &mut owner,
        &shared,
        action,
        &mut sink,
        key(KeyTransition::Press),
        1_000_000,
    );
    assert_eq!(result.outcome, InputOutcome::ExpiredBeforeSubmission);
    assert_eq!(result.refusal, Some(Refusal::ExpiredAtBoundary));
    assert_eq!(result.submitted_operations, 0);
    assert_eq!(deadline, None);
    assert_eq!(sink.effects, [] as [Operation; 0]);
    assert_eq!(sink.cancelled, 1);
    assert!(sink.prepared.is_none());
    assert_eq!(owner.held_count(), 0);
    assert!(owner.monitor().is_revoked());
    let duplicate = receipt(
        owner
            .dispatch(action, &mut sink, || {
                panic!("a consumed action must not retry")
            })
            .unwrap(),
    );
    assert_eq!(duplicate, result);
    assert_eq!(sink.cancelled, 1);
    assert_eq!(sink.effects, [] as [Operation; 0]);
}

#[test]
fn policy_lock_delay_within_the_original_deadline_still_submits() {
    let (mut owner, credentials, shared) = setup_shared();
    let mut sink = Sink::default();
    let (result, deadline) = through_policy_turn(
        &mut owner,
        &shared,
        request(credentials, 0, KeyTransition::Press),
        &mut sink,
        key(KeyTransition::Press),
        999_999,
    );
    assert_eq!(result.outcome, InputOutcome::SubmittedToOs);
    assert_eq!(result.submitted_operations, 1);
    assert_eq!(deadline, Some(HostInstant::from_micros(1_000_000)));
    assert_eq!(sink.effects, [key(KeyTransition::Press)]);
    assert_eq!(owner.held_count(), 1);
    assert_eq!(owner.cleanup(&mut sink).remaining, 0);
}

#[test]
fn policy_wait_after_repeat_release_retains_partial_receipt_without_repress() {
    let (mut owner, credentials, shared) = setup_shared();
    let mut sink = Sink::default();
    let _ = owner
        .dispatch(
            request(credentials, 0, KeyTransition::Press),
            &mut sink,
            || HostInstant::ORIGIN,
        )
        .unwrap();
    let repeat = request(credentials, 1, KeyTransition::Repeat);
    let (result, _) = through_policy_turn(
        &mut owner,
        &shared,
        repeat,
        &mut sink,
        key(KeyTransition::Press),
        1_000_000,
    );
    assert_eq!(result.outcome, InputOutcome::PartiallySubmittedToOs);
    assert_eq!(result.refusal, Some(Refusal::ExpiredAtBoundary));
    assert_eq!(result.submitted_operations, 1);
    assert_eq!(owner.held_count(), 0);
    assert_eq!(
        sink.effects,
        [key(KeyTransition::Press), key(KeyTransition::Release)]
    );
    assert_eq!(sink.cancelled, 3);
    let duplicate = receipt(
        owner
            .dispatch(repeat, &mut sink, || {
                panic!("a partial action must not retry")
            })
            .unwrap(),
    );
    assert_eq!(duplicate, result);
    assert_eq!(sink.effects.len(), 2);
}

#[test]
fn policy_wait_expiry_preserves_an_existing_press_for_release_only_cleanup() {
    let (mut owner, credentials, shared) = setup_shared();
    let mut sink = Sink::default();
    let _ = owner
        .dispatch(
            request(credentials, 0, KeyTransition::Press),
            &mut sink,
            || HostInstant::ORIGIN,
        )
        .unwrap();
    let (result, _) = through_policy_turn(
        &mut owner,
        &shared,
        request(credentials, 1, KeyTransition::Release),
        &mut sink,
        key(KeyTransition::Release),
        1_000_000,
    );
    assert_eq!(result.outcome, InputOutcome::ExpiredBeforeSubmission);
    assert_eq!(result.submitted_operations, 0);
    assert_eq!(owner.held_count(), 1);
    assert_eq!(sink.effects, [key(KeyTransition::Press)]);
    let cleanup = owner.cleanup(&mut sink);
    assert_eq!(cleanup.submitted_releases, 1);
    assert_eq!(cleanup.remaining, 0);
    assert_eq!(
        sink.effects,
        [key(KeyTransition::Press), key(KeyTransition::Release)]
    );
}

#[test]
fn final_unlocked_sample_rejects_clock_regression_without_tracking_a_press() {
    let (mut owner, credentials, shared) = setup_shared();
    let mut sink = Sink::default();
    // Admission, policy snapshot, then the fresh sample after all policy waits.
    let mut samples = [100, 100, 99].into_iter();
    let result = receipt(
        owner
            .dispatch(
                request(credentials, 0, KeyTransition::Press),
                &mut sink,
                || {
                    assert!(shared.try_lock().is_ok());
                    HostInstant::from_micros(samples.next().unwrap())
                },
            )
            .unwrap(),
    );
    assert_eq!(
        result.refusal,
        Some(Refusal::Authority(AuthorityError::ClockRegression))
    );
    assert_eq!(result.submitted_operations, 0);
    assert!(owner.monitor().is_revoked());
    assert_eq!(owner.held_count(), 0);
    assert_eq!(sink.effects, [] as [Operation; 0]);
    assert_eq!(sink.cancelled, 1);
}

#[test]
fn revocation_during_the_final_unlocked_sample_prevents_submission() {
    let (mut owner, credentials, shared) = setup_shared();
    let revoke = owner.revoke_handle();
    let mut sink = Sink::default();
    let mut samples = 0;
    let result = receipt(
        owner
            .dispatch(
                request(credentials, 0, KeyTransition::Press),
                &mut sink,
                || {
                    assert!(shared.try_lock().is_ok());
                    samples += 1;
                    if samples == 3 {
                        revoke.revoke();
                    }
                    HostInstant::ORIGIN
                },
            )
            .unwrap(),
    );
    assert_eq!(result.outcome, InputOutcome::CancelledBeforeSubmission);
    assert_eq!(result.refusal, Some(Refusal::Revoked));
    assert_eq!(result.submitted_operations, 0);
    assert_eq!(owner.held_count(), 0);
    assert_eq!(sink.effects, [] as [Operation; 0]);
    assert_eq!(sink.cancelled, 1);
}
