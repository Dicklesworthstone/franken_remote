//! Real supervised pipes; opaque test payloads do not qualify a native codec.
#![cfg(target_os = "linux")]
#[path = "support/executable.rs"]
mod executable;
#[path = "support/worker.rs"]
#[allow(dead_code)]
mod worker_support;
use asupersync::cx::Cx;
use fr_core::limits::ProtocolLimits;
use fr_media::{
    access_unit::FrameId,
    worker::{self, Configuration, Kind, Role},
};
use frd::worker::{Deadline, Error, Launch, State, Worker};
use std::time::Duration;
use worker_support::{configuration, image, runtime};

fn run(test: impl AsyncFnOnce(Cx)) {
    runtime().block_on(async {
        test(Cx::current().unwrap()).await;
    });
}
async fn worker(cx: &Cx, header_only: bool) -> Worker {
    let file = image(
        "fr-response-cap",
        include_str!("support/response_capacity_fixture.py")
            .replace("@HEADER_ONLY@", if header_only { "True" } else { "False" }),
    );
    Worker::start(
        cx,
        Launch::new(&file, ":0", None, Role::Capture, 77).unwrap(),
        Configuration {
            max_access_unit_bytes: 8192,
            ..configuration()
        },
        Deadline::after(cx, Duration::from_secs(2)).unwrap(),
    )
    .await
    .unwrap()
}
async fn stop(w: &mut Worker, cx: &Cx) {
    let deadline = Deadline::after(cx, Duration::from_secs(1)).unwrap();
    if w.state() == State::Running {
        w.request(cx, Kind::Stop, vec![], deadline).await.unwrap();
    }
    w.reap(cx, deadline).await.unwrap();
}
fn capture() -> Vec<u8> {
    worker::capture_payload(FrameId::FIRST, 1, true)
}

#[test]
fn oversized_header_is_refused_before_waiting_for_absent_payload() {
    run(async |cx| {
        let mut w = worker(&cx, true).await;
        let result = w
            .request_with_response_capacity(
                &cx,
                Kind::Capture,
                capture(),
                Some(64),
                Deadline::after(&cx, Duration::from_secs(1)).unwrap(),
            )
            .await;
        assert!(matches!(
            result,
            Err(Error::Protocol(worker::Error::ResourceLimit))
        ));
        assert_eq!(w.state(), State::Poisoned);
        stop(&mut w, &cx).await;
    });
}

#[test]
fn exact_capacity_reply_keeps_the_original_sequence_and_worker_usable() {
    run(async |cx| {
        let mut w = worker(&cx, false).await;
        let id = w.id();
        for _ in 0..3 {
            let record = w
                .request_with_response_capacity(
                    &cx,
                    Kind::Capture,
                    capture(),
                    Some(56),
                    Deadline::after(&cx, Duration::from_secs(1)).unwrap(),
                )
                .await
                .unwrap();
            let bytes = record.into_body();
            assert_eq!(bytes.len(), 56);
            assert_eq!(bytes.capacity(), 56);
            let unit = worker::parse_unit(bytes, &ProtocolLimits::ABSOLUTE).unwrap();
            assert_eq!(unit.bytes().len(), 16);
            assert_eq!(w.id(), id);
            assert_eq!(w.state(), State::Running);
        }
        stop(&mut w, &cx).await;
    });
}

#[test]
fn zero_capacity_refuses_before_any_capture_command_and_preserves_the_worker() {
    run(async |cx| {
        let mut w = worker(&cx, false).await;
        assert!(matches!(
            w.request_with_response_capacity(
                &cx,
                Kind::Capture,
                capture(),
                Some(0),
                Deadline::after(&cx, Duration::from_secs(1)).unwrap()
            )
            .await,
            Err(Error::Protocol(worker::Error::ResourceLimit))
        ));
        assert_eq!(w.state(), State::Running);
        let reply = w
            .request(
                &cx,
                Kind::Capture,
                capture(),
                Deadline::after(&cx, Duration::from_secs(1)).unwrap(),
            )
            .await
            .unwrap();
        // The child counts commands, so this proves the rejected call sent none.
        assert_eq!(reply.body()[40], 1);
        stop(&mut w, &cx).await;
    });
}

#[test]
fn a_missing_small_payload_still_expires_and_poisoned_ipc_cannot_resume() {
    run(async |cx| {
        let mut w = worker(&cx, true).await;
        assert!(matches!(
            w.request_with_response_capacity(
                &cx,
                Kind::Capture,
                capture(),
                Some(1040),
                Deadline::after(&cx, Duration::from_millis(100)).unwrap()
            )
            .await,
            Err(Error::Deadline)
        ));
        assert_eq!(w.state(), State::Poisoned);
        assert!(matches!(
            w.request(
                &cx,
                Kind::Capture,
                capture(),
                Deadline::after(&cx, Duration::from_secs(1)).unwrap()
            )
            .await,
            Err(Error::Unavailable)
        ));
        stop(&mut w, &cx).await;
    });
}
