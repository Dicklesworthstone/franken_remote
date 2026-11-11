//! Final publication follows the original authority through lock waits and
//! keep-both collisions, while retaining every already committed effect.
use super::*;
use fr_files::receive::ConflictPolicy;
use std::{
    sync::{Arc, Mutex, mpsc},
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
    authority.mark_view_ready(at(0)).unwrap();
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

#[test]
fn publication_resamples_after_the_actual_authority_mutex_without_renewing_its_bound() {
    for elapsed in [2_999_999, 3_000_000] {
        let scratch = Scratch::new();
        let (input, shared) = shared_owner();
        let mut session = scratch.session(&input);
        staged(&mut session);
        let now = Arc::new(AtomicU64::new(0));
        let (start_tx, start_rx) = mpsc::sync_channel(1);
        let (locked_tx, locked_rx) = mpsc::sync_channel(1);
        let (sampled_tx, sampled_rx) = mpsc::sync_channel(1);
        let holder = shared.clone();
        let holder_now = now.clone();
        let timeout = Duration::from_secs(5);
        let turn = thread::spawn(move || {
            start_rx.recv_timeout(timeout).unwrap();
            let guard = holder.lock().unwrap();
            locked_tx.send(()).unwrap();
            sampled_rx.recv_timeout(timeout).unwrap();
            holder_now.store(elapsed, Ordering::Release);
            drop(guard);
        });
        let mut samples = 0;
        let result = session.complete(session.binding(), 1, || {
            samples += 1;
            if samples == 2 {
                // Verification has finished; block this authorization turn on
                // the actual owner's mutex with an already captured timestamp.
                start_tx.send(()).unwrap();
                locked_rx.recv_timeout(timeout).unwrap();
                assert!(shared.try_lock().is_err());
                let old = now.load(Ordering::Acquire);
                sampled_tx.send(()).unwrap();
                at(old)
            } else {
                assert!(shared.try_lock().is_ok(), "no clock under the policy lock");
                at(now.load(Ordering::Acquire))
            }
        });
        turn.join().unwrap();
        if elapsed == 3_000_000 {
            assert_eq!(result, Err(Error::Authority(Refusal::ExpiredAtBoundary)));
            assert!(session.is_closed());
            scratch.empty();
        } else {
            assert_eq!(result.unwrap().publication, Publication::Durable);
            assert_eq!(fs::read(scratch.0.join("item")).unwrap(), b"abc");
        }
        assert!(session.progress().is_none());
    }
}

#[test]
fn final_publication_sample_fences_permission_revoke_input_revoke_and_regression() {
    for mode in 0..3 {
        let scratch = Scratch::new();
        let input = owner();
        let permission = Permission::new(true);
        let mut session = HostReceiver::new(
            &input,
            scratch.root(),
            permission.clone(),
            Policy::conservative(),
            at(0),
        )
        .unwrap();
        staged(&mut session);
        let mut samples = 0;
        let result = session.complete(session.binding(), 1, || {
            samples += 1;
            if samples == 3 {
                match mode {
                    0 => permission.revoke(),
                    1 => input.revoke_handle().revoke(),
                    _ => return at(99),
                }
            }
            at(100)
        });
        assert_eq!(
            result,
            Err(match mode {
                0 => Error::Permission,
                1 => Error::Authority(Refusal::Revoked),
                _ => Error::Clock,
            })
        );
        assert!(session.is_closed());
        assert!(session.progress().is_none());
        scratch.empty();
    }
}

#[test]
fn keep_both_rechecks_authority_and_immutable_transfer_lifetime_between_candidates() {
    for expire in [false, true] {
        let scratch = Scratch::new();
        let input = owner();
        let permission = Permission::new(true);
        let mut session = HostReceiver::new(
            &input,
            scratch
                .root()
                .with_conflict_policy(ConflictPolicy::KeepBoth),
            permission.clone(),
            Policy {
                transfer_lifetime: HostDuration::from_micros(1_000),
                ..Policy::conservative()
            },
            at(0),
        )
        .unwrap();
        staged(&mut session);
        fs::write(scratch.0.join("item"), b"keep original").unwrap();
        let mut samples = 0;
        let result = session.complete(session.binding(), 1, || {
            samples += 1;
            if samples >= 4 {
                if expire {
                    return at(1_000);
                }
                permission.revoke();
            }
            at(0)
        });
        assert_eq!(
            result,
            Err(if expire {
                Error::Expired
            } else {
                Error::Permission
            })
        );
        assert!(session.is_closed());
        assert!(session.progress().is_none());
        assert_eq!(fs::read_dir(&scratch.0).unwrap().count(), 1);
        assert_eq!(fs::read(scratch.0.join("item")).unwrap(), b"keep original");
        // Ending the file operation never revokes otherwise valid input.
        assert!(input.monitor().deadline(at(1_001)).is_ok());
    }
}

#[test]
fn valid_keep_both_publishes_once_and_never_rechecks_a_committed_effect() {
    let scratch = Scratch::new();
    let input = owner();
    let mut session = HostReceiver::new(
        &input,
        scratch
            .root()
            .with_conflict_policy(ConflictPolicy::KeepBoth),
        Permission::new(true),
        Policy::conservative(),
        at(0),
    )
    .unwrap();
    staged(&mut session);
    fs::write(scratch.0.join("item"), b"original").unwrap();
    let mut samples = 0;
    let receipt = session
        .complete(session.binding(), 1, || {
            samples += 1;
            assert!(samples <= 5, "no authority check after publication");
            at(samples * 10)
        })
        .unwrap();
    assert_eq!(samples, 5);
    assert_eq!(receipt.bytes, 3);
    assert_eq!(receipt.publication, Publication::Durable);
    let contents: Vec<_> = fs::read_dir(&scratch.0)
        .unwrap()
        .map(|entry| fs::read(entry.unwrap().path()).unwrap())
        .collect();
    assert_eq!(contents.len(), 2);
    assert!(contents.iter().any(|bytes| bytes == b"abc"));
    assert!(contents.iter().any(|bytes| bytes == b"original"));
    assert_eq!(
        session.complete(session.binding(), 1, || panic!("never replay")),
        Err(Error::NoTransfer)
    );
}
