//! Real same-user abstract sockets; synthetic reports explicitly test only the
//! local inspection grammar, not host delivery, decoding or hardware behavior.
use super::super::{IDENTIFY, Owner, UnixListener, hex, identity_record, name, own_uid};
use super::*;
use asupersync::{runtime::RuntimeBuilder, types::Budget};
use std::{
    future::{Future, poll_fn},
    pin::pin,
};

fn runtime() -> asupersync::runtime::Runtime {
    RuntimeBuilder::current_thread()
        .enable_platform_reactor(true)
        .build()
        .unwrap()
}
fn target(cx: &Cx) -> String {
    let mut bytes = [0; 16];
    cx.random_bytes(&mut bytes);
    format!("quality-{}", hex(&bytes))
}
async fn beside<T>(owner: &mut Owner, cx: &Cx, work: impl Future<Output = T>) -> T {
    let mut waiting = pin!(owner.wait_for_stop(cx));
    let mut work = pin!(work);
    poll_fn(|task| {
        assert!(
            waiting.as_mut().poll(task).is_pending(),
            "quality inspection requested STOP"
        );
        work.as_mut().poll(task)
    })
    .await
}
fn report() -> Report {
    Decision {
        sequence: 7,
        capture_interval_us: 50_000,
        mode: Mode::Active,
        reason: Reason::SendAdmission,
        applied_bitrate_bps: Some(2_000_000),
        pending_bitrate_bps: Some(1_500_000),
    }
    .into()
}

#[test]
fn real_owner_without_current_quality_returns_null_and_never_stops_or_grants_input() {
    let runtime = runtime();
    let cx = runtime.request_cx_with_budget(Budget::INFINITE);
    runtime.block_on(async {
        let target = target(&cx);
        let mut owner = Owner::bind(&cx, &target).await.unwrap();
        let original = owner.info();
        for _ in 0..3 {
            let status = beside(&mut owner, &cx, read(&cx, &target)).await.unwrap();
            assert_eq!(status.info, original);
            assert_eq!(status.quality, None);
            assert!(owner.requester.is_none());
            assert!(owner.automation.is_none());
            assert!(!cx.is_cancel_requested());
        }
    });
}

#[test]
fn same_socket_quality_reply_is_pinned_bounded_and_validated_before_exposure() {
    for case in 0..6 {
        let runtime = runtime();
        let cx = runtime.request_cx_with_budget(Budget::INFINITE);
        runtime.block_on(async {
            let target = target(&cx);
            let instance = [9; 16];
            let listener = UnixListener::bind_abstract(&name(own_uid().unwrap(), &target))
                .await
                .unwrap();
            let server = async {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut opening = [0; 8];
                stream.read_exact(&mut opening).await.unwrap();
                assert_eq!(opening, IDENTIFY);
                stream
                    .write_all(&identity_record(&target, instance))
                    .await
                    .unwrap();
                let mut request = [0; 24];
                stream.read_exact(&mut request).await.unwrap();
                assert_eq!(&request[..8], &REQUEST);
                assert_eq!(&request[8..], &instance);
                if case == 4 {
                    // A still-running older foreground binary supports ID but
                    // closes when it sees the new optional quality operation.
                    return;
                }
                let mut value = report();
                if case == 3 {
                    value.applied_bitrate_bps = None;
                }
                let bytes = serde_json::to_vec(&value).unwrap();
                let mut header = [0; 26];
                header[..8].copy_from_slice(&RESPONSE);
                header[8..24].copy_from_slice(&instance);
                if case == 1 {
                    header[8] ^= 1;
                }
                let length = if case == 2 {
                    MAX_BYTES + 1
                } else {
                    bytes.len()
                };
                header[24..].copy_from_slice(&u16::try_from(length).unwrap().to_be_bytes());
                if case == 5 {
                    stream.write_all(&header[..10]).await.unwrap();
                    return;
                }
                stream.write_all(&header).await.unwrap();
                if case != 2 {
                    stream.write_all(&bytes).await.unwrap();
                }
            };
            let client = read(&cx, &target);
            let mut server = pin!(server);
            let mut client = pin!(client);
            let mut done = false;
            let result = poll_fn(|task| {
                if !done {
                    done = server.as_mut().poll(task).is_ready();
                }
                client.as_mut().poll(task)
            })
            .await;
            if case == 0 {
                let status = result.unwrap();
                assert_eq!(status.info.instance, hex(&instance));
                assert_eq!(status.quality, Some(report()));
            } else if case == 4 {
                let status = result.unwrap();
                assert_eq!(status.info.instance, hex(&instance));
                assert_eq!(status.info.target, target);
                assert_eq!(status.quality, None);
            } else {
                assert_eq!(result, Err(Error::Protocol));
            }
            assert!(!cx.is_cancel_requested());
        });
    }
}

#[test]
fn reports_preserve_pending_bitrate_and_reject_unknown_or_out_of_range_claims() {
    let good = report();
    assert_eq!(good.validate(), Ok(()));
    assert!(
        good.text()
            .contains("pending 1500000 bps (not yet applied)")
    );
    assert!(good.text().contains("applied 2000000 bps"));
    let mut bad = good.clone();
    bad.mode = "healthy".into();
    assert_eq!(bad.validate(), Err(Error::Protocol));
    let mut bad = good.clone();
    bad.reason = "estimated_fps".into();
    assert_eq!(bad.validate(), Err(Error::Protocol));
    let mut bad = good.clone();
    bad.capture_interval_us = 0;
    assert_eq!(bad.validate(), Err(Error::Protocol));
    let mut bad = good;
    bad.pending_bitrate_bps = Some(0);
    assert_eq!(bad.validate(), Err(Error::Protocol));
}

#[test]
fn publishing_an_attempt_handle_waits_for_the_brief_read_instead_of_losing_the_update() {
    use std::{
        sync::{Barrier, mpsc},
        time::Duration,
    };
    let publisher = Publisher::default();
    let held = publisher.0.lock().unwrap();
    let began = Barrier::new(2);
    let (completed, done) = mpsc::sync_channel(1);
    std::thread::scope(|scope| {
        scope.spawn(|| {
            began.wait();
            publisher.set(None);
            completed.send(()).unwrap();
        });
        began.wait();
        assert_eq!(
            done.recv_timeout(Duration::from_millis(30)),
            Err(mpsc::RecvTimeoutError::Timeout)
        );
        drop(held);
        done.recv_timeout(Duration::from_secs(2)).unwrap();
    });
    assert!(publisher.read().is_none());
}
