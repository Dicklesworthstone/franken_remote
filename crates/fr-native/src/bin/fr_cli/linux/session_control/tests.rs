//! Actual kernel abstract sockets and credential reads on the pinned runtime.
//! These qualify the local stop channel, not a remote desktop or held-key release.
use super::*;
use asupersync::{
    runtime::{Runtime, RuntimeBuilder},
    types::Budget,
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
    format!("local-stop-test-{}", hex(&unique))
}
async fn beside<T>(owner: &mut Owner, cx: &Cx, work: impl Future<Output = T>) -> T {
    let mut waiting = pin!(owner.wait_for_stop(cx));
    let mut work = pin!(work);
    poll_fn(|task| {
        assert!(
            waiting.as_mut().poll(task).is_pending(),
            "read-only/invalid caller stopped the owner"
        );
        work.as_mut().poll(task)
    })
    .await
}
async fn turn(owner: &mut Owner, cx: &Cx) {
    let mut waiting = pin!(owner.wait_for_stop(cx));
    poll_fn(|task| {
        assert!(waiting.as_mut().poll(task).is_pending());
        Poll::Ready(())
    })
    .await;
}

#[test]
fn kernel_credentials_and_bounded_exact_targets_are_required() {
    let (stream, _peer) = UnixStream::pair().unwrap();
    let uid = own_uid().unwrap();
    assert_eq!(check_peer(&stream, uid), Ok(()));
    assert_eq!(
        check_peer(&stream, uid.wrapping_add(1)),
        Err(Error::UntrustedPeer)
    );
    for invalid in [
        String::new(),
        "x".repeat(MAX_TARGET + 1),
        "private\0target".into(),
        "host\nother".into(),
    ] {
        assert_eq!(check_target(&invalid), Err(Error::InvalidTarget));
    }
    assert_eq!(check_target(&"x".repeat(MAX_TARGET)), Ok(()));
    assert!(name(u32::MAX, &"x".repeat(MAX_TARGET)).len() < 108);
    assert_ne!(name(uid, "n-host"), name(uid, "n-hos"));
    assert_ne!(name(uid, "n-host"), name(uid.wrapping_add(1), "n-host"));
}

#[test]
fn probe_is_read_only_and_duplicate_owners_cannot_steal_the_target() {
    let runtime = runtime();
    let cx = runtime.request_cx_with_budget(Budget::INFINITE);
    runtime.block_on(async {
        let target = target(&cx);
        assert_eq!(probe(&cx, &target).await, Err(Error::NoSession));
        let mut owner = Owner::bind(&cx, &target).await.unwrap();
        assert!(matches!(
            Owner::bind(&cx, &target).await,
            Err(Error::AlreadyRunning)
        ));
        let info = beside(&mut owner, &cx, probe(&cx, &target)).await.unwrap();
        assert_eq!(info.target, target);
        assert_eq!(info.instance, hex(&owner.instance));
        assert!(owner.requester.is_none());
        assert!(!owner.completed);
        assert!(!cx.is_cancel_requested());
        drop(owner);
        let successor = Owner::bind(&cx, &target).await.unwrap();
        assert_ne!(hex(&successor.instance), info.instance);
    });
}

#[test]
fn stop_waits_for_the_original_owners_explicit_local_cleanup_result() {
    let runtime = runtime();
    let cx = runtime.request_cx_with_budget(Budget::INFINITE);
    runtime.block_on(async {
        for completion in [Completion::Confirmed, Completion::Unconfirmed] {
            let target = target(&cx);
            let mut owner = Owner::bind(&cx, &target).await.unwrap();
            let instance = hex(&owner.instance);
            let mut client = Box::pin(stop(&cx, &target));
            {
                let mut waiting = pin!(owner.wait_for_stop(&cx));
                poll_fn(|task| {
                    assert!(
                        client.as_mut().poll(task).is_pending(),
                        "socket acceptance is not cleanup"
                    );
                    waiting.as_mut().poll(task)
                })
                .await
                .unwrap();
            }
            assert!(owner.requester.is_some());
            // Accepted stop requests do not free or replace the original owner
            // while it is draining input/media/file/clipboard resources.
            assert!(matches!(
                Owner::bind(&cx, &target).await,
                Err(Error::AlreadyRunning)
            ));
            poll_fn(|task| {
                assert!(client.as_mut().poll(task).is_pending());
                Poll::Ready(())
            })
            .await;
            owner.complete(&cx, completion).await.unwrap();
            let stopped = client.await.unwrap();
            assert_eq!(stopped.info, Info { target, instance });
            assert_eq!(stopped.completion, completion);
            assert!(
                !cx.is_cancel_requested(),
                "only the enclosing session owner requests cancellation"
            );
        }
    });
}

#[test]
fn losing_the_owner_after_stop_is_unknown_and_never_fabricated_cleanup() {
    let runtime = runtime();
    let cx = runtime.request_cx_with_budget(Budget::INFINITE);
    runtime.block_on(async {
        let target = target(&cx);
        let mut owner = Owner::bind(&cx, &target).await.unwrap();
        let mut client = Box::pin(stop(&cx, &target));
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
        assert_eq!(client.await, Err(Error::OutcomeUnknown));
        // The old request was tied to the old stream, not a pathname/PID lookup.
        let successor = Owner::bind(&cx, &target).await.unwrap();
        assert!(successor.requester.is_none());
    });
}

#[test]
fn wrong_target_or_instance_cannot_retarget_a_stop_to_another_owner() {
    let runtime = runtime();
    let cx = runtime.request_cx_with_budget(Budget::INFINITE);
    runtime.block_on(async {
        let target = target(&cx);
        let mut owner = Owner::bind(&cx, &target).await.unwrap();
        // Explicit faulty same-user endpoint fixture: a routing name is never
        // enough evidence when its identity response names a different target.
        owner.target = "different-exact-target".into();
        assert_eq!(
            beside(&mut owner, &cx, stop(&cx, &target)).await,
            Err(Error::WrongOwner)
        );
        assert!(owner.requester.is_none());
        owner.target.clone_from(&target);
        let (mut stream, _, mut instance) = beside(&mut owner, &cx, identified(&cx, &target))
            .await
            .unwrap();
        instance[0] ^= 1;
        let mut stale = [0; 24];
        stale[..8].copy_from_slice(&STOP);
        stale[8..].copy_from_slice(&instance);
        stream.write_all(&stale).await.unwrap();
        turn(&mut owner, &cx).await;
        assert!(owner.requester.is_none());
        assert!(!owner.completed);
    });
}

#[test]
fn pending_local_callers_are_bounded_and_bad_framing_cannot_stop() {
    let runtime = runtime();
    let cx = runtime.request_cx_with_budget(Budget::INFINITE);
    runtime.block_on(async {
        let target = target(&cx);
        let mut owner = Owner::bind(&cx, &target).await.unwrap();
        let address = name(own_uid().unwrap(), &target);
        let mut clients = Vec::new();
        for _ in 0..=MAX_PEERS {
            clients.push(UnixStream::connect_abstract(&address).await.unwrap());
        }
        turn(&mut owner, &cx).await;
        assert_eq!(owner.peers.len(), MAX_PEERS);
        turn(&mut owner, &cx).await;
        assert_eq!(owner.peers.len(), MAX_PEERS);
        let mut extra = clients.pop().unwrap();
        let mut byte = [0];
        assert_eq!(
            extra.read(&mut byte).await.unwrap(),
            0,
            "capacity is a refusal, never another queue"
        );
        for stream in &mut clients {
            stream.write_all(b"INVALID!").await.unwrap();
        }
        turn(&mut owner, &cx).await;
        assert!(owner.peers.is_empty());
        assert!(owner.requester.is_none());
        let info = beside(&mut owner, &cx, probe(&cx, &target)).await.unwrap();
        assert_eq!(
            info.target, target,
            "bad callers do not disable the real owner"
        );
    });
}

#[test]
fn an_incomplete_handshake_expires_on_its_original_deadline() {
    let runtime = runtime();
    let cx = runtime.request_cx_with_budget(Budget::INFINITE);
    runtime.block_on(async {
        let target = target(&cx);
        let mut owner = Owner::bind(&cx, &target).await.unwrap();
        let _stalled = UnixStream::connect_abstract(&name(own_uid().unwrap(), &target))
            .await
            .unwrap();
        let clock = cx.timer_driver().unwrap();
        let began = clock.now().as_nanos();
        {
            let mut waiting = pin!(owner.wait_for_stop(&cx));
            poll_fn(|task| {
                assert!(waiting.as_mut().poll(task).is_pending());
                if clock.now().as_nanos() - began >= u64::try_from(HANDSHAKE.as_nanos()).unwrap() {
                    Poll::Ready(())
                } else {
                    Poll::Pending
                }
            })
            .await;
        }
        assert!(owner.peers.is_empty());
        assert!(owner.requester.is_none());
        assert!(!cx.is_cancel_requested());
    });
}

#[test]
fn exhausted_rejected_accepts_wake_the_next_original_caller() {
    let runtime = runtime();
    let cx = runtime.request_cx_with_budget(Budget::INFINITE);
    runtime.block_on(async {
        let target = target(&cx);
        let mut owner = Owner::bind(&cx, &target).await.unwrap();
        let address = name(own_uid().unwrap(), &target);
        let mut invalid = Vec::new();
        for _ in 0..MAX_PEERS {
            let mut stream = UnixStream::connect_abstract(&address).await.unwrap();
            stream.write_all(b"INVALID!").await.unwrap();
            invalid.push(stream);
        }
        let mut valid = Box::pin(probe(&cx, &target));
        // Queue this fifth connection before the owner's first bounded accept
        // turn. Its four predecessors all fail immediately and lose their wakes.
        poll_fn(|task| {
            assert!(valid.as_mut().poll(task).is_pending());
            Poll::Ready(())
        })
        .await;
        let info = bounded(
            &cx,
            Deadline::new(&cx, HANDSHAKE).unwrap(),
            beside(&mut owner, &cx, valid),
        )
        .await
        .unwrap();
        assert_eq!(info.target, target);
        assert!(owner.requester.is_none());
    });
}

#[test]
fn ready_work_cannot_hide_cancellation_or_renew_its_original_deadline() {
    let runtime = runtime();
    let cx = runtime.request_cx_with_budget(Budget::INFINITE);
    runtime.block_on(async {
        let deadline = Deadline::new(&cx, HANDSHAKE).unwrap();
        let result = bounded(&cx, deadline, async {
            cx.cancel_fast(asupersync::types::CancelKind::User);
            Ok(())
        })
        .await;
        assert_eq!(result, Err(Error::Cancelled));
    });
    let cx = runtime.request_cx_with_budget(Budget::INFINITE);
    runtime.block_on(async {
        let deadline = Deadline::new(&cx, Duration::from_millis(10)).unwrap();
        let result = bounded(&cx, deadline, async {
            // Explicit synchronous-overrun fixture, never a production wait.
            std::thread::sleep(Duration::from_millis(20));
            Ok(())
        })
        .await;
        assert_eq!(result, Err(Error::Timeout));
    });
}
