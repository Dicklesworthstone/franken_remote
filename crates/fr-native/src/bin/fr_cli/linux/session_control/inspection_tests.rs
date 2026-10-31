//! Real abstract sockets: inspecting or rejecting a stale invocation must not
//! stop it. A conditional stop retains the original cleanup/uncertainty rules.
use super::*;
use asupersync::{
    runtime::{Runtime, RuntimeBuilder},
    types::{Budget, CancelKind},
};

fn runtime() -> Runtime {
    RuntimeBuilder::current_thread()
        .enable_platform_reactor(true)
        .build()
        .unwrap()
}
fn target(cx: &Cx) -> String {
    let mut unique = [0; 16];
    cx.random_bytes(&mut unique);
    format!("local-inspection-test-{}", hex(&unique))
}
async fn beside<T>(owner: &mut Owner, cx: &Cx, work: impl Future<Output = T>) -> T {
    let mut waiting = pin!(owner.wait_for_stop(cx));
    let mut work = pin!(work);
    poll_fn(|task| {
        assert!(
            waiting.as_mut().poll(task).is_pending(),
            "inspection or refused instance pin requested a stop"
        );
        work.as_mut().poll(task)
    })
    .await
}

#[test]
fn repeated_inspection_preserves_the_owner_and_its_bounded_peer_slots() {
    let runtime = runtime();
    let cx = runtime.request_cx_with_budget(Budget::INFINITE);
    runtime.block_on(async {
        let target = target(&cx);
        let mut owner = Owner::bind(&cx, &target).await.unwrap();
        let expected = Info {
            target: target.clone(),
            instance: hex(&owner.instance),
        };
        for _ in 0..3 * MAX_PEERS {
            assert_eq!(
                beside(&mut owner, &cx, probe(&cx, &target)).await,
                Ok(expected.clone())
            );
            assert!(owner.peers.len() <= MAX_PEERS);
            assert!(owner.requester.is_none());
            assert!(!owner.completed);
        }
        assert!(!cx.is_cancel_requested());
    });
}

#[test]
fn an_inspected_instance_cannot_close_a_replacement_at_the_same_target() {
    let runtime = runtime();
    let cx = runtime.request_cx_with_budget(Budget::INFINITE);
    runtime.block_on(async {
        let target = target(&cx);
        let mut original = Owner::bind(&cx, &target).await.unwrap();
        let previous = beside(&mut original, &cx, probe(&cx, &target))
            .await
            .unwrap();
        drop(original);
        let mut successor = Owner::bind(&cx, &target).await.unwrap();
        assert_ne!(previous.instance, hex(&successor.instance));
        assert_eq!(
            beside(
                &mut successor,
                &cx,
                stop_if(&cx, &target, &previous.instance)
            )
            .await,
            Err(Error::WrongOwner)
        );
        let current = beside(&mut successor, &cx, probe(&cx, &target))
            .await
            .unwrap();
        assert_eq!(current.instance, hex(&successor.instance));
        assert!(successor.requester.is_none());
        assert!(!successor.completed);
        assert!(!cx.is_cancel_requested());
    });
}

#[test]
fn a_matching_pin_waits_for_the_same_owners_actual_cleanup_result() {
    let runtime = runtime();
    let cx = runtime.request_cx_with_budget(Budget::INFINITE);
    runtime.block_on(async {
        for completion in [Completion::Confirmed, Completion::Unconfirmed] {
            let target = target(&cx);
            let mut owner = Owner::bind(&cx, &target).await.unwrap();
            let info = beside(&mut owner, &cx, probe(&cx, &target)).await.unwrap();
            let mut client = pin!(stop_if(&cx, &target, &info.instance));
            {
                let mut waiting = pin!(owner.wait_for_stop(&cx));
                poll_fn(|task| {
                    assert!(
                        client.as_mut().poll(task).is_pending(),
                        "accepting STOP is not evidence of completed cleanup"
                    );
                    waiting.as_mut().poll(task)
                })
                .await
                .unwrap();
            }
            assert!(owner.requester.is_some());
            owner.complete(&cx, completion).await.unwrap();
            let stopped = client.await.unwrap();
            assert_eq!(stopped.info, info);
            assert_eq!(stopped.completion, completion);
            assert!(!cx.is_cancel_requested());
        }
    });
}

#[test]
fn malformed_pins_fail_before_lookup_even_when_no_owner_exists() {
    let runtime = runtime();
    let cx = runtime.request_cx_with_budget(Budget::INFINITE);
    runtime.block_on(async {
        let target = target(&cx);
        for invalid in [
            "",
            "0123456789abcdef0123456789abcde",
            "0123456789abcdef0123456789abcdef0",
            "0123456789ABCDEF0123456789ABCDEF",
            "0123456789abcdef0123456789abcdeg",
            "0123456789abcdef0123456789abcde\0",
        ] {
            assert_eq!(
                stop_if(&cx, &target, invalid).await,
                Err(Error::InvalidInstance)
            );
        }
        assert_eq!(
            stop_if(&cx, &target, "0123456789abcdef0123456789abcdef").await,
            Err(Error::NoSession)
        );
        assert!(!cx.is_cancel_requested());
    });
}

#[test]
fn cancelled_inspection_and_conditional_close_do_not_touch_the_live_owner() {
    let runtime = runtime();
    let cx = runtime.request_cx_with_budget(Budget::INFINITE);
    let caller = runtime.request_cx_with_budget(Budget::INFINITE);
    runtime.block_on(async {
        let target = target(&cx);
        let mut owner = Owner::bind(&cx, &target).await.unwrap();
        let instance = hex(&owner.instance);
        caller.cancel_fast(CancelKind::User);
        assert_eq!(probe(&caller, &target).await, Err(Error::Cancelled));
        assert_eq!(
            stop_if(&caller, &target, &instance).await,
            Err(Error::Cancelled)
        );
        assert_eq!(
            beside(&mut owner, &cx, probe(&cx, &target))
                .await
                .unwrap()
                .instance,
            instance
        );
        assert!(owner.requester.is_none());
        assert!(!owner.completed);
        assert!(!cx.is_cancel_requested());
    });
}

#[test]
fn losing_a_matching_owner_after_stop_remains_unknown_without_replay() {
    let runtime = runtime();
    let cx = runtime.request_cx_with_budget(Budget::INFINITE);
    runtime.block_on(async {
        let target = target(&cx);
        let mut owner = Owner::bind(&cx, &target).await.unwrap();
        let info = beside(&mut owner, &cx, probe(&cx, &target)).await.unwrap();
        let mut client = pin!(stop_if(&cx, &target, &info.instance));
        {
            let mut waiting = pin!(owner.wait_for_stop(&cx));
            poll_fn(|task| {
                assert!(client.as_mut().poll(task).is_pending());
                waiting.as_mut().poll(task)
            })
            .await
            .unwrap();
        }
        drop(owner);
        let mut successor = Owner::bind(&cx, &target).await.unwrap();
        assert_eq!(
            beside(&mut successor, &cx, client).await,
            Err(Error::OutcomeUnknown)
        );
        assert!(successor.requester.is_none());
        assert_ne!(info.instance, hex(&successor.instance));
    });
}
