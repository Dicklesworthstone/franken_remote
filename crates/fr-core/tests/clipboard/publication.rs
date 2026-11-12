//! Native clipboard publication must account for time spent obtaining policy.
use super::*;
use std::{
    cell::Cell,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
        mpsc,
    },
    thread,
    time::Duration,
};

fn shared_owner() -> (InputSession, Arc<Mutex<SessionAuthority>>) {
    let credentials = InputCredentials {
        session: RemoteSessionId::from_raw(1),
        lease: InputLeaseId::from_raw(2),
        ticket: InputTicketId::from_raw(3),
        view: InputView {
            geometry: DisplayGeometryGeneration::INITIAL,
            viewport: ViewportMappingGeneration::INITIAL,
            configuration: CodecConfigurationGeneration::INITIAL,
            recovery: RecoveryGeneration::INITIAL,
        },
    };
    let mut authority =
        SessionAuthority::new(credentials.session, AuthorityPolicy::plan_defaults());
    authority.mark_capabilities_checked().unwrap();
    authority.authorize_observation(at(0)).unwrap();
    authority.require_view_evidence(at(0)).unwrap();
    authority
        .mark_view_ready_until(at(1_000_000), at(0))
        .unwrap();
    authority.grant_lease(credentials.lease, at(0)).unwrap();
    authority
        .issue_input_ticket(credentials.lease, credentials.ticket, at(0))
        .unwrap();
    let shared = Arc::new(Mutex::new(authority));
    let input = InputSession::from_shared_authority(
        shared.clone(),
        credentials,
        InputBounds::new(DesktopPoint { x: 0, y: 0 }, 320, 240).unwrap(),
        Capabilities::default(),
        at(0),
    )
    .unwrap();
    (input, shared)
}

struct WaitSink<'a> {
    inner: &'a mut Sink,
    wait_next_sample: &'a Cell<bool>,
    deadline: Option<HostInstant>,
}

impl ClipboardSink for WaitSink<'_> {
    fn prepare(&mut self, text: &str, stamp: Stamp) -> Result<(), PlatformError> {
        self.inner.prepare(text, stamp)?;
        self.wait_next_sample.set(true);
        Ok(())
    }

    fn publish(&mut self, text: &str, stamp: Stamp) -> Publication {
        self.inner.publish(text, stamp)
    }

    fn publish_until(&mut self, text: &str, stamp: Stamp, until: HostInstant) -> Publication {
        self.deadline = Some(until);
        self.inner.publish(text, stamp)
    }

    fn cancel_prepared(&mut self) {
        self.inner.cancel_prepared();
    }
}

/// Channel handoffs retain a pre-lock clock sample while another thread owns
/// the actual shared authority. Its policy time stays unchanged, so serialized
/// policy time cannot stand in for the required fresh unlocked clock sample.
fn through_policy_turn(
    session: &mut ClipboardSession,
    shared: &Arc<Mutex<SessionAuthority>>,
    begin: Begin,
    sink: &mut Sink,
    elapsed_until: u64,
) -> (Result<Receipt, Error>, Option<HostInstant>) {
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
        wait_next_sample: &wait_next_sample,
        deadline: None,
    };
    let mut passed_policy_turn = false;
    let result = session.commit(begin.stamp, begin.total_bytes, &mut boundary, || {
        if wait_next_sample.replace(false) {
            start_tx.send(()).unwrap();
            locked_rx.recv_timeout(timeout).unwrap();
            assert!(shared.try_lock().is_err(), "the policy turn owns the lock");
            let sampled = now.load(Ordering::Acquire);
            sampled_tx.send(()).unwrap();
            passed_policy_turn = true;
            at(sampled)
        } else {
            if passed_policy_turn {
                assert!(shared.try_lock().is_ok(), "clock ran under policy lock");
            }
            at(now.load(Ordering::Acquire))
        }
    });
    holder.join().unwrap();
    (result, boundary.deadline)
}

#[test]
fn policy_wait_cannot_publish_at_the_exact_original_view_deadline() {
    let (input, shared) = shared_owner();
    let mut session = receiver(&input);
    let item = receive(&mut session, 1, b"must not replace the local clipboard");
    let mut sink = Sink::default();
    let (result, deadline) = through_policy_turn(&mut session, &shared, item, &mut sink, 1_000_000);
    assert_eq!(result, Err(Error::Expired));
    assert_eq!(deadline, None);
    assert_eq!(sink.effects, [] as [String; 0]);
    assert_eq!(sink.cleanup, 1);
    assert_eq!(session.buffered_bytes(), 0);
    assert_eq!(session.reserved_bytes(), 0);
    // A consumed old commit is never retried, even when its caller later
    // supplies an earlier clock. Regression closes the original owner.
    assert_eq!(
        session.commit(item.stamp, item.total_bytes, &mut sink, || at(0)),
        Err(Error::Clock)
    );
    assert!(input.monitor().is_revoked());
    assert_eq!(sink.effects, [] as [String; 0]);
}

#[test]
fn policy_wait_inside_original_deadline_publishes_once_with_the_same_cap() {
    let (input, shared) = shared_owner();
    let mut session = receiver(&input);
    let item = receive(&mut session, 1, b"current");
    let mut sink = Sink::default();
    let (result, deadline) = through_policy_turn(&mut session, &shared, item, &mut sink, 999_999);
    let receipt = result.unwrap();
    assert_eq!(receipt.publication, Publication::SubmittedToOs);
    assert_eq!(deadline, Some(at(1_000_000)));
    assert_eq!(sink.effects, ["current"]);
    assert_eq!(sink.cleanup, 1);
    assert_eq!(
        session.commit(item.stamp, item.total_bytes, &mut sink, || at(999_999)),
        Ok(receipt)
    );
    assert_eq!(sink.effects, ["current"]);
}

#[test]
fn policy_wait_does_not_extend_the_earlier_item_deadline() {
    let (input, shared) = shared_owner();
    let mut session = receiver(&input);
    let item = begin(&session, 1, 4, 1);
    session.begin_before(item, at(0), at(500_000)).unwrap();
    session.chunk(item.stamp, 0, 0, b"late", at(0)).unwrap();
    let mut sink = Sink::default();
    let (result, deadline) = through_policy_turn(&mut session, &shared, item, &mut sink, 500_000);
    assert_eq!(result, Err(Error::Expired));
    assert_eq!(deadline, None);
    assert_eq!(sink.effects, [] as [String; 0]);
    assert_eq!(sink.cleanup, 1);
    assert!(
        !input.monitor().is_revoked(),
        "item expiry is not lease revocation"
    );
    assert_eq!(
        session.commit(item.stamp, item.total_bytes, &mut sink, || at(500_000)),
        Err(Error::UnknownTransfer)
    );
}

#[test]
fn final_unlocked_sample_rejects_clock_regression_before_publication() {
    let (input, shared) = shared_owner();
    let mut session = receiver(&input);
    let item = receive(&mut session, 1, b"private");
    let mut sink = Sink::default();
    let mut samples = [100, 100, 99].into_iter();
    assert_eq!(
        session.commit(item.stamp, item.total_bytes, &mut sink, || {
            assert!(shared.try_lock().is_ok());
            at(samples.next().unwrap())
        }),
        Err(Error::Clock)
    );
    assert!(input.monitor().is_revoked());
    assert!(session.is_closed());
    assert_eq!(sink.effects, [] as [String; 0]);
    assert_eq!(sink.cleanup, 1);
}

#[test]
fn revocation_during_final_unlocked_sample_prevents_publication() {
    let (input, shared) = shared_owner();
    let mut session = receiver(&input);
    let item = receive(&mut session, 1, b"private");
    let revoke = input.revoke_handle();
    let mut sink = Sink::default();
    let mut samples = 0;
    assert_eq!(
        session.commit(item.stamp, item.total_bytes, &mut sink, || {
            assert!(shared.try_lock().is_ok());
            samples += 1;
            if samples == 3 {
                revoke.revoke();
            }
            at(0)
        }),
        Err(Error::Authority(
            fr_core::input_submission::Refusal::Revoked
        ))
    );
    assert!(session.is_closed());
    assert_eq!(sink.effects, [] as [String; 0]);
    assert_eq!(sink.cleanup, 1);
}

#[test]
fn clipboard_switch_cycle_during_final_sample_consumes_without_publication() {
    let (input, shared) = shared_owner();
    let mut session = receiver(&input);
    let item = receive(&mut session, 1, b"private");
    let switch = session.local_switch();
    let mut sink = Sink::default();
    let mut samples = 0;
    assert_eq!(
        session.commit(item.stamp, item.total_bytes, &mut sink, || {
            assert!(shared.try_lock().is_ok());
            samples += 1;
            if samples == 3 {
                switch.set_enabled(false);
                switch.set_enabled(true);
            }
            at(0)
        }),
        Err(Error::Disabled)
    );
    assert!(!input.monitor().is_revoked());
    assert_eq!(sink.effects, [] as [String; 0]);
    assert_eq!(sink.cleanup, 1);
    assert_eq!(
        session.commit(item.stamp, item.total_bytes, &mut sink, || at(0)),
        Err(Error::UnknownTransfer)
    );
}

#[test]
fn newer_view_evidence_cannot_replace_the_cap_already_authorized() {
    let (input, shared) = shared_owner();
    let mut session = receiver(&input);
    let item = receive(&mut session, 1, b"original");
    let mut sink = Sink::default();
    let mut samples = 0;
    assert_eq!(
        session.commit(item.stamp, item.total_bytes, &mut sink, || {
            samples += 1;
            if samples == 3 {
                shared
                    .lock()
                    .unwrap()
                    .mark_view_ready_until(at(2_000_000), at(999_999))
                    .unwrap();
                at(1_000_000)
            } else {
                at(0)
            }
        }),
        Err(Error::Expired)
    );
    assert_eq!(sink.effects, [] as [String; 0]);
    assert_eq!(sink.cleanup, 1);
    assert!(!input.monitor().is_revoked());
    assert_eq!(
        session.commit(item.stamp, item.total_bytes, &mut sink, || at(1_000_000)),
        Err(Error::UnknownTransfer)
    );
}

#[test]
fn native_effect_receipt_survives_revocation_during_the_publication_call() {
    struct RevokeAfterEffect {
        revoke: fr_core::input_submission::RevokeHandle,
        result: Publication,
        effects: usize,
        cleanup: usize,
    }
    impl ClipboardSink for RevokeAfterEffect {
        fn prepare(&mut self, _: &str, _: Stamp) -> Result<(), PlatformError> {
            Ok(())
        }
        fn publish(&mut self, _: &str, _: Stamp) -> Publication {
            self.effects += 1;
            self.revoke.revoke();
            self.result
        }
        fn cancel_prepared(&mut self) {
            self.cleanup += 1;
        }
    }
    for publication in [Publication::SubmittedToOs, Publication::UnknownEffect] {
        let input = owner();
        let mut session = receiver(&input);
        let item = receive(&mut session, 1, b"committed");
        let mut sink = RevokeAfterEffect {
            revoke: input.revoke_handle(),
            result: publication,
            effects: 0,
            cleanup: 0,
        };
        let receipt = session
            .commit(item.stamp, item.total_bytes, &mut sink, || at(0))
            .unwrap();
        assert_eq!(receipt.publication, publication);
        assert_eq!(receipt.stamp, item.stamp);
        assert_eq!((sink.effects, sink.cleanup), (1, 1));
        assert!(input.monitor().is_revoked());
        assert!(
            session
                .commit(item.stamp, item.total_bytes, &mut sink, || at(0))
                .is_err()
        );
        assert_eq!((sink.effects, sink.cleanup), (1, 1));
    }
}

#[test]
fn final_view_loss_pauses_the_channel_and_never_replays_the_consumed_item() {
    let (input, shared) = shared_owner();
    let mut session = receiver(&input);
    let item = receive(&mut session, 1, b"obsolete");
    let mut sink = Sink::default();
    let mut samples = 0;
    assert_eq!(
        session.commit(item.stamp, item.total_bytes, &mut sink, || {
            samples += 1;
            if samples == 3 {
                shared.lock().unwrap().mark_view_stale();
            }
            at(0)
        }),
        Err(Error::Suspended)
    );
    assert!(!session.is_closed());
    assert!(!input.monitor().is_revoked());
    assert_eq!(sink.effects, [] as [String; 0]);
    assert_eq!(sink.cleanup, 1);
    input.monitor().confirm_suspension_release().unwrap();
    shared
        .lock()
        .unwrap()
        .mark_view_ready_until(at(2_000_000), at(1))
        .unwrap();
    assert_eq!(
        session.commit(item.stamp, item.total_bytes, &mut sink, || at(2)),
        Err(Error::UnknownTransfer)
    );
    let fresh = begin(&session, 2, 5, 1);
    session.begin(fresh, at(2)).unwrap();
    session.chunk(fresh.stamp, 0, 0, b"fresh", at(2)).unwrap();
    let receipt = session
        .commit(fresh.stamp, fresh.total_bytes, &mut sink, || at(2))
        .unwrap();
    assert_eq!(receipt.publication, Publication::SubmittedToOs);
    assert_eq!(sink.effects, ["fresh"]);
    assert_eq!(sink.cleanup, 2);
}

#[test]
fn final_policy_contention_refuses_clipboard_without_waiting_or_revoking_input() {
    let (input, shared) = shared_owner();
    let mut session = receiver(&input);
    let item = receive(&mut session, 1, b"private");
    let mut sink = Sink::default();
    let mut samples = 0;
    let mut held_policy = None;
    assert_eq!(
        session.commit(item.stamp, item.total_bytes, &mut sink, || {
            samples += 1;
            if samples == 3 {
                held_policy = Some(shared.lock().unwrap());
            }
            at(0)
        }),
        Err(Error::Authority(
            fr_core::input_submission::Refusal::AuthorityUnavailable
        ))
    );
    assert!(
        held_policy.is_some(),
        "the final check must not wait for this lock"
    );
    drop(held_policy);
    assert!(session.is_closed());
    assert!(!input.monitor().is_revoked());
    assert_eq!(input.monitor().deadline(at(0)), Ok(at(1_000_000)));
    assert_eq!(sink.effects, [] as [String; 0]);
    assert_eq!(sink.cleanup, 1);
}
