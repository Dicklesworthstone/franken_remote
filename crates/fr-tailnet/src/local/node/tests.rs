//! Real credential-checked Unix/HTTP lookups with explicit authority-clock schedules.
//! These fixtures do not qualify an installed Tailscale daemon or kernel ingress.
use super::*;
use crate::local::tests::{Server, fixtures, response};
use asupersync::{
    runtime::{Runtime, RuntimeBuilder},
    time::{TimerDriverHandle, VirtualClock},
    types::{CancelKind, Time},
};
use serde_json::{Value, json};
use std::sync::{Mutex, atomic::Ordering};

fn status() -> Value {
    let (mut status, _) = fixtures();
    status["Self"]["DNSName"] = json!("host.fixture.ts.net.");
    status
}

fn runtime(clock: Arc<VirtualClock>) -> Runtime {
    RuntimeBuilder::new()
        .worker_threads(2)
        .enable_platform_reactor(true)
        .with_timer_driver(TimerDriverHandle::with_virtual_clock(clock))
        .build()
        .unwrap()
}

fn clock() -> Arc<VirtualClock> {
    Arc::new(VirtualClock::starting_at(Time::from_millis(100)))
}

#[test]
fn direct_ingress_slow_lookup_keeps_its_start_time_and_the_original_deadline() {
    let clock = clock();
    let response_clock = clock.clone();
    let server = Server::with_reply_hook(vec![response(&status(), false)], move |index| {
        if index >= 2 {
            response_clock.advance(600_000_000);
        }
    });
    runtime(clock.clone()).block_on(async {
        let cx = Cx::current().unwrap();
        let old = server.client.node_identity(&cx).await.unwrap();
        assert_eq!(old.expires_us(), 3_100_000);
        clock.advance(400_000_000);
        let new = server
            .client
            .revalidate_direct_ingress_node(&cx, &old)
            .await
            .unwrap();
        assert_eq!(server.calls.load(Ordering::SeqCst), 4);
        assert_eq!(now(&cx).unwrap(), 1_700_000);
        assert_eq!(new.issued_us, 500_000);
        assert_eq!(new.expires_us(), 3_500_000);
        assert_eq!(old.expires_us(), 3_100_000, "the old proof never changes");
        assert!(old.same_node(&new));
        assert!(server.client.check_node(&cx, &old).is_ok());
    });
}

#[test]
fn direct_ingress_accepts_a_real_reply_delayed_more_than_one_second() {
    let server = Server::with_reply_hook(vec![response(&status(), false)], |index| {
        if index == 2 {
            std::thread::sleep(Duration::from_millis(1_200));
        }
    });
    RuntimeBuilder::new()
        .worker_threads(2)
        .enable_platform_reactor(true)
        .build()
        .unwrap()
        .block_on(async {
            let cx = Cx::current().unwrap();
            let old = server.client.node_identity(&cx).await.unwrap();
            let before = now(&cx).unwrap();
            let new = server
                .client
                .revalidate_direct_ingress_node(&cx, &old)
                .await
                .unwrap();
            let completed = now(&cx).unwrap();
            assert!(completed - before >= 1_000_000);
            assert!(new.issued_us >= before);
            assert!(completed - new.issued_us >= 1_000_000);
            assert_eq!(new.expires_us(), new.issued_us + VALIDITY_US);
            assert!(completed < old.expires_us());
            assert!(old.same_node(&new));
            assert_eq!(server.calls.load(Ordering::SeqCst), 4);
        });
}

#[test]
fn cold_and_ordinary_revalidation_keep_the_one_second_lookup_limit() {
    for revalidate in [false, true] {
        let clock = clock();
        let response_clock = clock.clone();
        let server = Server::with_reply_hook(vec![response(&status(), false)], move |index| {
            if !revalidate || index >= 2 {
                response_clock.advance(600_000_000);
            }
        });
        runtime(clock).block_on(async {
            let cx = Cx::current().unwrap();
            let result = if revalidate {
                let old = server.client.node_identity(&cx).await.unwrap();
                server.client.revalidate_node(&cx, &old).await
            } else {
                server.client.node_identity(&cx).await
            };
            assert!(matches!(result, Err(Error::Timeout)), "{result:?}");
            assert!(!server.client.busy.load(Ordering::Acquire));
            assert_eq!(
                server.calls.load(Ordering::SeqCst),
                if revalidate { 4 } else { 2 },
                "one lookup, without retries"
            );
        });
    }
}

#[test]
fn a_direct_lookup_cannot_replace_a_snapshot_at_or_after_its_expiry() {
    for completed_at in [3_099_999, 3_100_000, 3_100_001] {
        let clock = clock();
        let response_clock = clock.clone();
        let server = Server::with_reply_hook(vec![response(&status(), false)], move |index| {
            if index == 2 {
                response_clock.advance_to(Time::from_nanos(completed_at * 1_000));
            }
        });
        runtime(clock.clone()).block_on(async {
            let cx = Cx::current().unwrap();
            let old = server.client.node_identity(&cx).await.unwrap();
            clock.advance(400_000_000);
            let result = server
                .client
                .revalidate_direct_ingress_node(&cx, &old)
                .await;
            if completed_at < old.expires_us() {
                let new = result.unwrap();
                assert_eq!(new.issued_us, 500_000);
                assert_eq!(new.expires_us(), 3_500_000);
            } else {
                assert!(matches!(result, Err(Error::Expired)), "{result:?}");
                assert_eq!(server.client.check_node(&cx, &old), Err(Error::Expired));
            }
            assert_eq!(old.expires_us(), 3_100_000);
            assert!(!server.client.busy.load(Ordering::Acquire));
        });
    }
}

#[test]
fn direct_lookup_does_not_retry_identity_key_or_metadata_refusals() {
    let original = status();
    for (field, value, expected) in [
        (
            "PublicKey",
            json!(format!("nodekey:{}", "3".repeat(64))),
            Error::IdentityChanged,
        ),
        ("Expired", json!(true), Error::KeyExpired),
        (
            "KeyExpiry",
            json!("1970-01-01T00:00:01Z"),
            Error::KeyExpired,
        ),
        ("DNSName", json!(""), Error::MalformedMetadata),
    ] {
        let mut changed = original.clone();
        changed["Self"][field] = value;
        let server = Server::with_reply_hook(
            vec![
                response(&original, false),
                response(&original, false),
                response(&changed, false),
            ],
            |_| {},
        );
        runtime(clock()).block_on(async {
            let cx = Cx::current().unwrap();
            let old = server.client.node_identity(&cx).await.unwrap();
            let result = server
                .client
                .revalidate_direct_ingress_node(&cx, &old)
                .await;
            assert!(
                matches!(result, Err(error) if error == expected),
                "{field}: {result:?}"
            );
            assert_eq!(server.calls.load(Ordering::SeqCst), 4);
            assert!(!server.client.busy.load(Ordering::Acquire));
        });
    }
}

#[test]
fn direct_lookup_rejects_foreign_provenance_and_old_key_or_wall_clock_failure() {
    let server = Server::with_reply_hook(vec![response(&status(), false)], |_| {});
    runtime(clock()).block_on(async {
        let cx = Cx::current().unwrap();
        let mut old = server.client.node_identity(&cx).await.unwrap();
        let calls = server.calls.load(Ordering::SeqCst);
        let foreign = LocalApi::new(&*server.client.path).unwrap();
        assert!(matches!(
            foreign.revalidate_direct_ingress_node(&cx, &old).await,
            Err(Error::IdentityMismatch)
        ));
        old.key_expires_us = Some(1);
        assert!(matches!(
            server
                .client
                .revalidate_direct_ingress_node(&cx, &old)
                .await,
            Err(Error::KeyExpired)
        ));
        old.key_expires_us = None;
        old.wall_issued_us = u64::MAX;
        assert!(matches!(
            server
                .client
                .revalidate_direct_ingress_node(&cx, &old)
                .await,
            Err(Error::Clock)
        ));
        assert_eq!(server.calls.load(Ordering::SeqCst), calls);
        assert!(!server.client.busy.load(Ordering::Acquire));
    });
}

#[test]
fn cancellation_during_a_direct_lookup_never_publishes_a_replacement() {
    let cancel = Arc::new(Mutex::new(None::<Cx>));
    let response_cancel = cancel.clone();
    let server = Server::with_reply_hook(vec![response(&status(), false)], move |index| {
        if index == 2 {
            response_cancel
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .cancel_fast(CancelKind::User);
        }
    });
    runtime(clock()).block_on(async {
        let cx = Cx::current().unwrap();
        let old = server.client.node_identity(&cx).await.unwrap();
        *cancel.lock().unwrap() = Some(cx.clone());
        let result = server
            .client
            .revalidate_direct_ingress_node(&cx, &old)
            .await;
        assert!(matches!(result, Err(Error::Cancelled)), "{result:?}");
        assert_eq!(old.expires_us(), 3_100_000);
        assert!(!server.client.busy.load(Ordering::Acquire));
    });
}
