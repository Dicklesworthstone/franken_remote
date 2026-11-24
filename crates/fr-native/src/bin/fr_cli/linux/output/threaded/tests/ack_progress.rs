//! The real ACK bridge keeps one owner's progress callback active while the
//! session is pending. This exercises no native audio hardware or codec.
use super::super::forward_ack as maintain_ack;
use super::*;
use crate::audio_output::native::{DeviceError, PlayoutError};

#[test]
fn pending_ack_maintains_same_owner_progress_without_admitting_audio() {
    let origin = Instant::now();
    let (progress, observed) = mpsc::sync_channel(1);
    let (result, completion) = mpsc::sync_channel(1);
    let (release, released) = mpsc::sync_channel(1);
    let (mut output, shared, _) = fixture(move |shared, _, replies| {
        let owner = thread::current().id();
        let original_until = shared.until.load(Ordering::Acquire);
        let mut polls = 0;
        let accepted = maintain_ack(
            shared,
            origin,
            origin + SETUP,
            &replies,
            &ack_record(),
            &mut || {
                assert_eq!(thread::current().id(), owner);
                polls += 1;
                if polls == 3 {
                    assert_eq!(shared.ack.load(Ordering::Acquire), ACK_PENDING);
                    assert_eq!(shared.until.load(Ordering::Acquire), original_until);
                    assert_eq!(shared.submitted.load(Ordering::Acquire), 0);
                    progress.send(()).unwrap();
                }
                Ok(())
            },
        )
        .is_ok();
        result.send((accepted, polls)).unwrap();
        let _ = released.recv_timeout(Duration::from_secs(2));
        true
    });
    observed.recv_timeout(Duration::from_secs(1)).unwrap();
    assert_eq!(shared.ack.load(Ordering::Acquire), ACK_PENDING);
    assert!(!output.report.borrow().acknowledged);
    assert!(completion.try_recv().is_err());
    output
        .service(&mut || true, &mut |bytes| {
            assert_eq!(bytes, &ack_record());
            Ok(())
        })
        .unwrap();
    let (accepted, polls) = completion.recv_timeout(Duration::from_secs(1)).unwrap();
    assert!(accepted && polls >= 3);
    // Native progress and even worker-side acceptance cannot bypass the
    // existing session's admission receipt for these exact bytes.
    assert!(!output.report.borrow().acknowledged);
    output.configuration_admitted(73, offer()).unwrap();
    assert!(output.report.borrow().acknowledged);
    release.send(()).unwrap();
    retired(&shared);
}

#[test]
fn stop_or_permission_expiry_during_progress_refuses_a_racing_ack() {
    for stop in [true, false] {
        let origin = Instant::now();
        let shared = Shared::new(0);
        shared.authorize_at(0).unwrap();
        let original_until = shared.until.load(Ordering::Acquire);
        let (replies, records) = mpsc::sync_channel(1);
        let mut polls = 0;
        assert!(
            maintain_ack(
                &shared,
                origin,
                origin + SETUP,
                &replies,
                &ack_record(),
                &mut || {
                    polls += 1;
                    if stop {
                        shared.stop();
                    } else {
                        // Force the exact permission boundary while native
                        // maintenance is in progress, without a timing race.
                        shared.until.store(now(origin).0, Ordering::Release);
                    }
                    shared.ack.store(ACK_ACCEPTED, Ordering::Release);
                    Ok(())
                },
            )
            .is_err()
        );
        assert_eq!(polls, 1);
        assert_eq!(records.try_recv().unwrap(), ack_record());
        assert!(shared.is_stopped());
        assert!(shared.until.load(Ordering::Acquire) <= original_until);
        assert_eq!(shared.submitted.load(Ordering::Acquire), 0);
    }
}

#[test]
fn native_progress_expiry_keeps_its_reason_and_original_deadline() {
    let origin = Instant::now();
    let shared = Shared::new(0);
    shared.authorize_at(0).unwrap();
    let original_until = shared.until.load(Ordering::Acquire);
    let (replies, records) = mpsc::sync_channel(1);
    assert!(
        maintain_ack(
            &shared,
            origin,
            origin + SETUP,
            &replies,
            &ack_record(),
            &mut || Err(PlayoutError::Device(DeviceError::Expired)),
        )
        .is_err()
    );
    assert_eq!(records.try_recv().unwrap(), ack_record());
    assert_eq!(shared.fault.load(Ordering::Acquire), Fault::Expired as u8);
    assert_eq!(shared.until.load(Ordering::Acquire), original_until);
    assert_eq!(shared.ack.load(Ordering::Acquire), ACK_PENDING);
    assert_eq!(shared.submitted.load(Ordering::Acquire), 0);
    assert!(shared.is_stopped());
}
