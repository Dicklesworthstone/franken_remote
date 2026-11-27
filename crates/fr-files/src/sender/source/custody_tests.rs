//! Ordinary local selections retain their original sender's authority even
//! when no parent service turn runs. Filesystem and thread witnesses are real;
//! these tests do not manufacture a remote publication receipt.
use super::tests::{fixture, prepared, reap};
use super::*;
use crate::session::{Authority, Permission};
use asupersync::{
    runtime::RuntimeBuilder,
    time::VirtualClock,
    types::{Budget, Time},
};
use fr_core::{
    authority::{AuthorityPolicy, SessionAuthority},
    ids::{
        CodecConfigurationGeneration, DisplayGeometryGeneration, InputLeaseId, InputTicketId,
        RecoveryGeneration, RemoteSessionId, ViewportMappingGeneration,
    },
    input::{DesktopPoint, InputBounds, InputCredentials, InputView},
    input_submission::{Capabilities, InputSession},
    time::HostInstant,
};
use std::{fs, time::Instant};

fn input_owner(now: u64) -> InputSession {
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
    let now = HostInstant::from_micros(now);
    let mut authority = SessionAuthority::new(credentials.session, AuthorityPolicy::plan_defaults());
    authority.mark_capabilities_checked().unwrap();
    authority.authorize_observation(now).unwrap();
    authority.mark_view_ready(now).unwrap();
    authority.grant_lease(credentials.lease, now).unwrap();
    authority
        .issue_input_ticket(credentials.lease, credentials.ticket, now)
        .unwrap();
    InputSession::new(
        authority,
        credentials,
        InputBounds::new(DesktopPoint { x: 0, y: 0 }, 320, 240).unwrap(),
        Capabilities::default(),
        now,
    )
    .unwrap()
}

#[test]
fn sender_consent_still_bounds_hashing_under_a_separately_live_job_scope() {
    for directory in [false, true] {
        fixture(|path, cx| {
            let payload = path.join("payload");
            fs::write(&payload, vec![7_u8; 2 * MAX_CHUNK_BYTES]).unwrap();
            let clock = cx.timer_driver().unwrap();
            let now = clock.now().as_nanos() / 1000;
            let input = input_owner(now);
            let authority = Authority::from_input(&input);
            let original_permission = Permission::new(true);
            let job_permission = Permission::new(true);
            let gate = Gate {
                stop: Arc::new(AtomicBool::new(false)),
                cx: cx.clone(),
                clock,
                deadline: now + 10_000_000,
                scope: Some(SourceScope::for_sender(
                    authority.clone(),
                    job_permission.clone(),
                )),
                owner_scope: Some(SourceScope::for_sender(
                    authority,
                    original_permission.clone(),
                )),
            };
            let root = if directory { path } else { &payload };
            let mut selected = tree::Selection::new(
                File::open(root).unwrap(),
                "selected",
                directory,
                (2 * MAX_CHUNK_BYTES) as u64,
                &mut || gate.check(),
            )
            .unwrap();
            let tree::Contents::File { file, .. } = &selected.entries[0].contents else {
                panic!("the selected source must contain the real payload");
            };
            let mut witness = file.try_clone().unwrap();
            let result = prepare(&mut selected, "selected".into(), directory, &mut || {
                if witness.stream_position().unwrap() != 0 {
                    original_permission.revoke();
                }
                gate.check()
            });
            assert!(matches!(result, Err(Error::Cancelled)));
            assert_eq!(witness.stream_position().unwrap(), MAX_CHUNK_BYTES as u64);
            assert!(job_permission.is_approved());
            assert_eq!(gate.scope.as_ref().unwrap().check(now), Ok(()));
            assert!(!input.monitor().is_revoked());
            assert!(!cx.is_cancel_requested());
        });
    }
}

#[test]
fn ordinary_source_joins_at_original_lease_expiry_without_parent_service() {
    fixture(|path, _cx| {
        let payload = path.join("payload");
        fs::write(&payload, b"original source").unwrap();
        let clock = Arc::new(VirtualClock::new());
        let runtime = RuntimeBuilder::new()
            .worker_threads(1)
            .with_timer_driver(TimerDriverHandle::with_virtual_clock(clock.clone()))
            .build()
            .unwrap();
        let cx = runtime.request_cx_with_budget(Budget::INFINITE);
        let input = input_owner(0);
        let permission = Permission::new(true);
        let mut source = Source::spawn_selected(
            cx.clone(),
            SelectedSource {
                file: File::open(payload).unwrap(),
                name: "selected".into(),
                directory: false,
                expected_bytes: None,
            },
            Policy::default(),
            10_000_000,
            None,
            Some(SourceScope::for_sender(
                Authority::from_input(&input),
                permission.clone(),
            )),
        )
        .unwrap();
        assert_eq!(prepared(&mut source).total_bytes, 15);
        // The transfer still has seven seconds left. No cancellation, command
        // or owner turn is needed for the original three-second lease to end it.
        clock.advance_to(Time::from_millis(3_000));
        reap(&mut source);
        assert!(matches!(source.poll(), Err(Error::Cancelled)));
        assert!(!source.stop.load(Ordering::Acquire));
        assert!(source.thread.is_none());
        assert!(permission.is_approved());
        assert!(!cx.is_cancel_requested());
    });
}

#[test]
fn permission_revocation_discards_a_payload_read_while_its_owner_is_stalled() {
    fixture(|path, cx| {
        let payload = path.join("payload");
        fs::write(&payload, b"original source").unwrap();
        let file = File::open(payload).unwrap();
        let mut witness = file.try_clone().unwrap();
        let now = cx.timer_driver().unwrap().now().as_nanos() / 1000;
        let input = input_owner(now);
        let permission = Permission::new(true);
        let mut source = Source::spawn_selected(
            cx.clone(),
            SelectedSource {
                file,
                name: "selected".into(),
                directory: false,
                expected_bytes: None,
            },
            Policy::default(),
            now + 10_000_000,
            None,
            Some(SourceScope::for_sender(
                Authority::from_input(&input),
                permission.clone(),
            )),
        )
        .unwrap();
        assert_eq!(prepared(&mut source).total_bytes, 15);
        assert_eq!(witness.stream_position().unwrap(), 0);
        source.request(1).unwrap();
        let until = Instant::now() + Duration::from_secs(2);
        while witness.stream_position().unwrap() == 0 {
            assert!(Instant::now() < until, "the source did not read its payload");
            thread::yield_now();
        }
        assert_eq!(witness.stream_position().unwrap(), 1);
        // Leave the bounded result slot alone. Either the in-flight read or its
        // unread result must be fenced; neither can escape on a later poll.
        permission.revoke();
        reap(&mut source);
        assert!(matches!(source.poll(), Err(Error::Cancelled)));
        assert_eq!(source.request(1), Err(Error::Cancelled));
        assert_eq!(witness.stream_position().unwrap(), 1);
        assert!(!source.stop.load(Ordering::Acquire));
        assert!(!input.monitor().is_revoked());
        assert!(!cx.is_cancel_requested());
    });
}

#[cfg(target_os = "linux")]
#[allow(dead_code)]
#[path = "../../../tests/native_support/mod.rs"]
mod native_support;

#[cfg(target_os = "linux")]
#[test]
fn paused_authorized_sender_sources_stop_without_a_sender_or_network_turn() {
    use super::super::{Outcome, Sender};
    use fr_client::input::StopReason;
    use native_support::{Running, runtime};

    for directory in [false, true] {
        for revoke_input in [false, true] {
            runtime().block_on(async {
                let cx = Cx::current().unwrap();
                let mut f = Running::new(&cx).await;
                let payload = f.path.join("payload");
                fs::write(&payload, b"original source").unwrap();
                let (mut input, authority) = native_support::download::accepted(&cx);
                let permission = Permission::new(true);
                let mut sender = Sender::new_authorized(
                    cx.clone(),
                    &f.link.c,
                    &mut f.client,
                    Policy::default(),
                    authority,
                    permission.clone(),
                )
                .unwrap();
                sender.set_paused(&f.link.c, true).unwrap();
                if directory {
                    sender
                        .begin_directory(&f.link.c, File::open(&f.path).unwrap(), "selected")
                        .unwrap();
                } else {
                    sender
                        .begin(&f.link.c, File::open(payload).unwrap(), "selected")
                        .unwrap();
                }
                assert_eq!(
                    prepared(&mut sender.transfer.as_mut().unwrap().source).total_bytes,
                    15
                );
                if revoke_input {
                    input.stop(StopReason::Disconnected);
                } else {
                    permission.revoke();
                }
                // Only observe the original thread here. Sender::service,
                // cancel, stop and parent QUIC drive have not run since begin.
                let until = Instant::now() + Duration::from_secs(2);
                while !sender.cleanup_finished() {
                    assert!(Instant::now() < until, "the authorized source stayed alive");
                    thread::yield_now();
                }
                assert!(
                    !sender
                        .transfer
                        .as_ref()
                        .unwrap()
                        .source
                        .stop
                        .load(Ordering::Acquire)
                );
                assert_eq!(sender.try_finish_cleanup(), Some(Ok(())));
                assert!(sender.transfer.as_ref().unwrap().source.thread.is_none());
                assert!(matches!(
                    sender.transfer.as_mut().unwrap().source.poll(),
                    Err(Error::Cancelled)
                ));
                // A permissive caller callback cannot replace the constructor's
                // original control/permission once the paused owner returns.
                assert_eq!(
                    sender.service(&mut f.link.c, || true),
                    Err(Error::Cancelled)
                );
                assert_eq!(
                    sender.take_result().unwrap().outcome,
                    Outcome::InterruptedBeforePublication(Error::Cancelled)
                );
                assert!(!cx.is_cancel_requested());
                if !revoke_input {
                    assert!(input.stopped().is_none());
                    assert!(!f.input.monitor().is_revoked());
                }
            });
        }
    }
}

#[cfg(target_os = "linux")]
#[test]
fn a_callback_cannot_reauthorize_a_retained_offer_after_revoking_original_consent() {
    use super::super::{Outcome, Sender};
    use native_support::{Running, runtime};

    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut f = Running::new(&cx).await;
        let payload = f.path.join("payload");
        fs::write(&payload, b"original source").unwrap();
        let (_input, authority) = native_support::download::accepted(&cx);
        let permission = Permission::new(true);
        let mut sender = Sender::new_authorized(
            cx.clone(),
            &f.link.c,
            &mut f.client,
            Policy::default(),
            authority,
            permission.clone(),
        )
        .unwrap();
        sender
            .begin(&f.link.c, File::open(payload).unwrap(), "selected")
            .unwrap();
        // Retain an actual prepared manifest as an ordinary service turn can.
        // Its next prepare step consumes this event without polling the source,
        // so the autonomous worker's failure timing cannot decide this test.
        let transfer = sender.transfer.as_mut().unwrap();
        let manifest = prepared(&mut transfer.source);
        transfer.ready = Some(Event::Prepared(Box::new(manifest)));
        assert!(sender.flight.is_empty());
        assert_eq!(
            sender.service(&mut f.link.c, || {
                permission.revoke();
                true
            }),
            Err(Error::Cancelled)
        );
        assert!(sender.flight.is_empty());
        let until = Instant::now() + Duration::from_secs(2);
        while !sender.cleanup_finished() {
            assert!(Instant::now() < until, "the fenced source did not stop");
            thread::yield_now();
        }
        assert_eq!(sender.try_finish_cleanup(), Some(Ok(())));
        assert_eq!(
            sender.take_result().unwrap().outcome,
            Outcome::InterruptedBeforePublication(Error::Cancelled)
        );
        assert!(!f.input.monitor().is_revoked());
        assert!(!cx.is_cancel_requested());
    });
}
