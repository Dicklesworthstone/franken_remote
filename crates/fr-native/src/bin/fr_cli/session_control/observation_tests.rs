//! Actual abstract sockets prove snapshot reads do not become STOP requests.
use super::super::{Owner, hex, probe};
use super::*;
use asupersync::{runtime::RuntimeBuilder, types::Budget};
use std::{
    future::{Future, poll_fn},
    pin::pin,
};

async fn beside<T>(owner: &mut Owner, cx: &Cx, work: impl Future<Output = T>) -> T {
    let mut waiting = pin!(owner.wait_for_stop(cx));
    let mut work = pin!(work);
    poll_fn(|task| {
        assert!(
            waiting.as_mut().poll(task).is_pending(),
            "observation requested STOP"
        );
        work.as_mut().poll(task)
    })
    .await
}
fn target(cx: &Cx) -> String {
    let mut bytes = [0; 16];
    cx.random_bytes(&mut bytes);
    format!("observation-{}", hex(&bytes))
}
fn runtime() -> asupersync::runtime::Runtime {
    RuntimeBuilder::current_thread()
        .enable_platform_reactor(true)
        .build()
        .unwrap()
}

#[test]
fn successive_reads_reuse_the_original_owner_and_see_lifecycle_changes() {
    let runtime = runtime();
    let cx = runtime.request_cx_with_budget(Budget::INFINITE);
    runtime.block_on(async {
        let target = target(&cx);
        let publisher = Publisher::default();
        let mut owner = Owner::bind(&cx, &target)
            .await
            .unwrap()
            .with_observations(publisher.clone());
        let id = owner.info().instance;
        let first = beside(&mut owner, &cx, read(&cx, &target, &id))
            .await
            .unwrap();
        assert_eq!(first.snapshot.phase, Phase::Starting);
        publisher.update(|state| state.begin(1)).unwrap();
        publisher.update(Observation::authenticated).unwrap();
        publisher.update(Observation::approval).unwrap();
        let second = beside(&mut owner, &cx, read(&cx, &target, &id))
            .await
            .unwrap();
        assert_eq!(second.info, first.info);
        assert_eq!(second.snapshot.phase, Phase::AwaitingApproval);
        publisher.retire(Phase::Cleaning);
        let third = beside(&mut owner, &cx, read(&cx, &target, &id))
            .await
            .unwrap();
        assert!(third.snapshot.frame.is_none());
        assert_eq!(third.snapshot.phase, Phase::Cleaning);
        assert!(owner.requester.is_none());
        assert!(!cx.is_cancel_requested());
        assert_eq!(
            beside(&mut owner, &cx, probe(&cx, &target))
                .await
                .unwrap()
                .instance,
            id
        );
    });
}

#[test]
fn absent_publication_is_typed_absence_not_a_fabricated_empty_frame() {
    let runtime = runtime();
    let cx = runtime.request_cx_with_budget(Budget::INFINITE);
    runtime.block_on(async {
        let target = target(&cx);
        let mut owner = Owner::bind(&cx, &target).await.unwrap();
        let id = owner.info().instance;
        assert_eq!(
            beside(&mut owner, &cx, read(&cx, &target, &id)).await,
            Err(ReadError::Unavailable)
        );
        assert!(owner.requester.is_none());
    });
}

#[test]
fn a_saved_pin_cannot_observe_a_replacement_and_a_bad_pin_does_no_io() {
    let runtime = runtime();
    let cx = runtime.request_cx_with_budget(Budget::INFINITE);
    runtime.block_on(async {
        let target = target(&cx);
        let old = Owner::bind(&cx, &target).await.unwrap();
        let old_id = old.info().instance;
        drop(old);
        let mut next = Owner::bind(&cx, &target)
            .await
            .unwrap()
            .with_observations(Publisher::default());
        assert_ne!(old_id, next.info().instance);
        assert_eq!(
            beside(&mut next, &cx, read(&cx, &target, &old_id)).await,
            Err(ReadError::Local(Error::WrongOwner))
        );
        assert_eq!(
            read(&cx, &target, "not-a-pin").await,
            Err(ReadError::Local(Error::InvalidInstance))
        );
        assert!(next.requester.is_none());
    });
}

#[test]
fn response_identity_length_and_version_are_checked_before_payload_allocation() {
    let id = [7; 16];
    let mut header = [0; 26];
    header[..8].copy_from_slice(&RESPONSE);
    header[8..24].copy_from_slice(&id);
    for size in [0_u16, 1, 2048] {
        header[24..].copy_from_slice(&size.to_be_bytes());
        assert_eq!(reply_length(&header, id), Ok(usize::from(size)));
    }
    for size in [2049_u16, u16::MAX] {
        header[24..].copy_from_slice(&size.to_be_bytes());
        assert_eq!(reply_length(&header, id), Err(Error::Protocol));
    }
    header[24..].copy_from_slice(&1_u16.to_be_bytes());
    assert_eq!(reply_length(&header, [8; 16]), Err(Error::Protocol));
    header[0] ^= 1;
    assert_eq!(reply_length(&header, id), Err(Error::Protocol));
}

#[test]
fn cancellation_and_busy_snapshot_storage_never_stop_or_invent_data() {
    let runtime = runtime();
    let cx = runtime.request_cx_with_budget(Budget::INFINITE);
    runtime.block_on(async {
        let target = target(&cx);
        let mut owner = Owner::bind(&cx, &target).await.unwrap();
        let cancelled = runtime.request_cx_with_budget(Budget::INFINITE);
        cancelled.cancel_fast(asupersync::types::CancelKind::User);
        assert_eq!(
            read(&cancelled, &target, &owner.info().instance).await,
            Err(ReadError::Local(Error::Cancelled))
        );
        assert!(owner.requester.is_none());
        let publisher = Publisher::default();
        let held = publisher.0.lock().unwrap();
        assert!(
            publisher.read().is_none(),
            "read must not block on a held slot"
        );
        drop(held);
        assert_eq!(
            beside(&mut owner, &cx, probe(&cx, &target))
                .await
                .unwrap()
                .target,
            target
        );
    });
}
