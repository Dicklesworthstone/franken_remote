//! Real supervised child IPC and production source/authority/egress owners.
//! Opaque encoded payloads and simulated receiver completions do NOT qualify HEVC.
#![cfg(target_os = "linux")]
use asupersync::{
    cx::Cx,
    runtime::{Runtime, RuntimeBuilder},
    types::Budget,
};
use fr_core::{
    authority::{AuthorityPolicy, SessionAuthority},
    ids::*,
    input::{DesktopPoint, InputBounds, InputCredentials, InputView},
    input_submission::{Capabilities, InputSession},
    limits::ProtocolLimits,
    time::HostDuration,
};
use fr_media::{
    delivery::*,
    worker::{Backend, Configuration, Kind, Role},
};
use fr_wire::{
    Channel, MediaLimits,
    decoder::Binding,
    input::{InputDelivery, InputDirection},
    negotiation::ControlBinding,
    recovery_request::{self, Reason, Request},
};
use frd::{
    media::{
        CaptureSource, Error, ObservationControl, SharedCaptureUpdate, Subscription, host_now,
    },
    media_egress::{Admission, Egress, Lane, Progress},
    worker::{Deadline, Launch},
};
use std::{os::unix::fs::PermissionsExt, time::Duration};

fn runtime() -> Runtime {
    RuntimeBuilder::new()
        .worker_threads(1)
        .blocking_threads(1, 2)
        .enable_platform_reactor(true)
        .build()
        .unwrap()
}

macro_rules! run_shared {
    ($rt:ident, $cx:ident, $body:expr) => {{
        let $rt = runtime();
        $rt.block_on(async {
            let $cx = Cx::current().unwrap();
            $body
        });
    }};
}

fn gate(runtime: &Runtime, id: u128) -> (ObservationControl, InputSession) {
    let cx = runtime.request_cx_with_budget(Budget::INFINITE);
    let now = host_now(&cx).unwrap();
    let mut a = SessionAuthority::new(
        RemoteSessionId::from_raw(id),
        AuthorityPolicy {
            authorization_lifetime: HostDuration::from_micros(5_000_000),
            ticket_lifetime: HostDuration::from_micros(4_000_000),
        },
    );
    a.mark_capabilities_checked().unwrap();
    a.authorize_observation(now).unwrap();
    a.mark_view_ready(now).unwrap();
    a.grant_lease(InputLeaseId::from_raw(1), now).unwrap();
    a.issue_input_ticket(InputLeaseId::from_raw(1), InputTicketId::from_raw(1), now)
        .unwrap();
    let c = ObservationControl::new(cx, a).unwrap();
    let input = c
        .input_session(
            InputCredentials {
                session: RemoteSessionId::from_raw(id),
                lease: InputLeaseId::from_raw(1),
                ticket: InputTicketId::from_raw(1),
                view: InputView {
                    geometry: DisplayGeometryGeneration::INITIAL,
                    viewport: ViewportMappingGeneration::INITIAL,
                    configuration: epoch().configuration,
                    recovery: epoch().recovery,
                },
            },
            InputBounds::new(DesktopPoint { x: 0, y: 0 }, 320, 240).unwrap(),
            Capabilities::default(),
        )
        .unwrap();
    (c, input)
}
fn epoch() -> MediaEpoch {
    MediaEpoch {
        configuration: CodecConfigurationGeneration::INITIAL,
        recovery: RecoveryGeneration::INITIAL,
    }
}
fn limits(bytes: usize) -> MediaLimits {
    MediaLimits::new(ProtocolLimits::ABSOLUTE, bytes, 16384, 64).unwrap()
}
fn bindings(n: u32) -> MediaBindings {
    MediaBindings::new(n, n + 1, n + 2, n + 3).unwrap()
}
fn pool() -> SharedFramePool {
    SharedFramePool::new(ProtocolLimits::ABSOLUTE, 1024 * 1024, 32).unwrap()
}
fn egress(c: &ObservationControl, l: MediaLimits, b: MediaBindings, policy: SendPolicy) -> Egress {
    Egress::new(Subscription::new(c.clone(), l, b, epoch(), policy).unwrap())
}
fn receiver(cx: &Cx, l: MediaLimits, b: MediaBindings) -> ReceivePipeline {
    let mut r = ReceivePipeline::new(
        ReceiveConfig {
            limits: l,
            bindings: b,
            epoch: epoch(),
            policy: ReceivePolicy::default(),
        },
        MediaBudget::new(l.protocol()).unwrap(),
    )
    .unwrap();
    r.decoder_configured(host_now(cx).unwrap().as_micros())
        .unwrap();
    r
}
async fn source(c: &ObservationControl, changing: bool) -> CaptureSource {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let file = std::env::temp_dir().join(format!(
        "fr-shared-{}-{}.py",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::write(
        &file,
        include_str!("support/shared_capture_fixture.py")
            .replace("@CHANGING@", if changing { "True" } else { "False" }),
    )
    .unwrap();
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o700)).unwrap();
    CaptureSource::start(
        c,
        Launch::new(&file, ":0", None, Role::Capture, 77).unwrap(),
        Configuration {
            width: 320,
            height: 240,
            fps: 30,
            backend: Backend::SoftwareExplicit,
            bitrate: 2_000_000,
            max_access_unit_bytes: 1024 * 1024,
            generation: epoch().configuration,
        },
    )
    .await
    .unwrap()
}
async fn capture(
    c: &ObservationControl,
    s: &mut CaptureSource,
    p: &SharedFramePool,
    force: bool,
) -> SharedCaptureUpdate {
    s.capture_if_changed(c, force)
        .await
        .unwrap()
        .share(p)
        .unwrap()
}
async fn stop(s: &mut CaptureSource, cx: &Cx) {
    let d = Deadline::after(cx, Duration::from_secs(1)).unwrap();
    let w = s.worker_mut();
    w.request(cx, Kind::Stop, vec![], d).await.unwrap();
    w.reap(cx, d).await.unwrap();
}
fn pump(cx: &Cx, e: &mut Egress, r: &mut ReceivePipeline) -> Vec<Channel> {
    let mut channels = Vec::new();
    loop {
        let p = e
            .transmit(Lane::Original, |offer, bytes, guard| {
                guard().unwrap();
                r.receive(offer.channel(), bytes, host_now(cx).unwrap().as_micros())
                    .unwrap();
                channels.push(offer.channel());
                Ok::<_, ()>(Admission::Accepted)
            })
            .unwrap();
        if p == Progress::Idle {
            break;
        }
    }
    channels
}
fn decoded(cx: &Cx, r: &mut ReceivePipeline) -> DecodedFrame {
    let now = host_now(cx).unwrap().as_micros();
    let p = r.take_decodable(now).unwrap().unwrap();
    r.complete_decode(&p, now).unwrap()
}
fn input_live(cx: &Cx, input: &InputSession) -> bool {
    input
        .monitor()
        .authorize_ticket(InputTicketId::from_raw(1), host_now(cx).unwrap())
        .is_ok()
}

#[test]
fn one_native_allocation_feeds_independent_egresses_and_receiver_bindings() {
    run_shared!(rt, cx, {
        let (owner, _) = gate(&rt, 1);
        let (ac, _) = gate(&rt, 2);
        let (bc, _) = gate(&rt, 3);
        let mut s = source(&owner, true).await;
        let pid = s.worker_id();
        let pool = pool();
        let raw = s.capture_if_changed(&owner, true).await.unwrap();
        let pointer = raw.encoded().unwrap().bytes().as_ptr();
        let shared = raw.share(&pool).unwrap();
        assert_eq!(shared.encoded().unwrap().bytes().as_ptr(), pointer);
        let alias = shared.clone();
        assert!(
            shared
                .encoded()
                .unwrap()
                .shares_storage_with(alias.encoded().unwrap())
        );
        let mut a = egress(&ac, limits(1150), bindings(1), SendPolicy::default());
        let mut b = egress(&bc, limits(700), bindings(11), SendPolicy::default());
        let physical = pool.usage();
        assert_eq!(
            shared.distribute(&mut [&mut a, &mut b]).unwrap().results(),
            &[Ok(()), Ok(())]
        );
        assert_eq!(pool.usage(), physical);
        assert!(a.cache_usage().bytes > physical.bytes);
        assert_eq!(a.cache_usage(), b.cache_usage());
        let mut ar = receiver(&cx, limits(1150), bindings(1));
        let mut br = receiver(&cx, limits(700), bindings(11));
        let ap = pump(&cx, &mut a, &mut ar);
        let bp = pump(&cx, &mut b, &mut br);
        assert!(bp.len() > ap.len());
        assert_eq!(decoded(&cx, &mut ar).descriptor().frame, 0);
        assert_eq!(decoded(&cx, &mut br).descriptor().frame, 0);
        drop(shared);
        drop(alias);
        a.close();
        assert_eq!(pool.usage(), physical);
        b.close();
        assert_eq!(pool.usage(), BudgetUsage::default());
        assert_eq!(s.worker_id(), pid);
        stop(&mut s, &cx).await;
    });
}
#[test]
fn unchanged_native_results_update_both_subscribers_without_another_encoded_allocation() {
    run_shared!(rt, cx, {
        let (owner, _) = gate(&rt, 1);
        let (ac, _) = gate(&rt, 2);
        let (bc, _) = gate(&rt, 3);
        let mut s = source(&owner, false).await;
        let pool = pool();
        let mut a = egress(&ac, limits(1150), bindings(1), SendPolicy::default());
        let mut b = egress(&bc, limits(1150), bindings(11), SendPolicy::default());
        let mut ar = receiver(&cx, limits(1150), bindings(1));
        let mut br = receiver(&cx, limits(1150), bindings(11));
        let initial = capture(&owner, &mut s, &pool, true).await;
        initial.distribute(&mut [&mut a, &mut b]).unwrap();
        pump(&cx, &mut a, &mut ar);
        pump(&cx, &mut b, &mut br);
        decoded(&cx, &mut ar);
        decoded(&cx, &mut br);
        drop(initial);
        let used = pool.usage();
        let idle = capture(&owner, &mut s, &pool, false).await;
        assert!(idle.is_unchanged());
        assert!(idle.encoded().is_none());
        assert_eq!(pool.usage(), used);
        assert_eq!(
            idle.distribute(&mut [&mut a, &mut b]).unwrap().results(),
            &[Ok(()), Ok(())]
        );
        assert_eq!(pump(&cx, &mut a, &mut ar), [Channel::MediaConfig]);
        assert_eq!(pump(&cx, &mut b, &mut br), [Channel::MediaConfig]);
        assert!(
            ar.take_decodable(host_now(&cx).unwrap().as_micros())
                .unwrap()
                .is_none()
        );
        assert_eq!(pool.usage(), used);
        let mut fresh = egress(&ac, limits(1150), bindings(21), SendPolicy::default());
        assert_eq!(
            fresh.enqueue_shared_capture(&idle),
            Err(Error::InvalidFrame)
        );
        a.close();
        b.close();
        assert_eq!(pool.usage(), BudgetUsage::default());
        stop(&mut s, &cx).await;
    });
}
#[test]
fn revoked_viewer_neither_blocks_fanout_nor_keeps_a_prepared_packet_authorized() {
    run_shared!(rt, cx, {
        let (owner, _) = gate(&rt, 1);
        let (ac, ai) = gate(&rt, 2);
        let (bc, bi) = gate(&rt, 3);
        let mut s = source(&owner, true).await;
        let pid = s.worker_id();
        let pool = pool();
        let mut a = egress(&ac, limits(1150), bindings(1), SendPolicy::default());
        let mut b = egress(&bc, limits(1150), bindings(11), SendPolicy::default());
        let mut br = receiver(&cx, limits(1150), bindings(11));
        let first = capture(&owner, &mut s, &pool, true).await;
        first.distribute(&mut [&mut a, &mut b]).unwrap();
        pump(&cx, &mut b, &mut br);
        decoded(&cx, &mut br);
        drop(first);
        assert!(matches!(
            a.transmit(Lane::Original, |_, _, guard| {
                guard().unwrap();
                Ok::<_, ()>(Admission::Backpressure)
            })
            .unwrap(),
            Progress::Pending(_)
        ));
        ac.revoke();
        let next = capture(&owner, &mut s, &pool, false).await;
        let report = next.distribute(&mut [&mut a, &mut b]).unwrap();
        assert!(report.results()[0].is_err());
        assert_eq!(report.results()[1], Ok(()));
        let mut submitted = false;
        assert!(
            a.transmit(Lane::Original, |_, _, _| {
                submitted = true;
                Ok::<_, ()>(Admission::Accepted)
            })
            .is_err()
        );
        assert!(!submitted);
        assert!(!input_live(&cx, &ai));
        assert!(input_live(&cx, &bi));
        assert!(owner.check().is_ok());
        pump(&cx, &mut b, &mut br);
        assert_eq!(decoded(&cx, &mut br).descriptor().frame, 1);
        assert_eq!(s.worker_id(), pid);
        drop(next);
        b.close();
        assert_eq!(pool.usage(), BudgetUsage::default());
        stop(&mut s, &cx).await;
    });
}
#[test]
fn a_missing_reference_fences_only_that_viewers_cache_and_native_input_tickets() {
    run_shared!(rt, cx, {
        let (owner, _) = gate(&rt, 1);
        let (ac, ai) = gate(&rt, 2);
        let (bc, bi) = gate(&rt, 3);
        let mut s = source(&owner, true).await;
        let pool = pool();
        let mut a = egress(&ac, limits(1150), bindings(1), SendPolicy::default());
        let mut b = egress(&bc, limits(1150), bindings(11), SendPolicy::default());
        let mut ar = receiver(&cx, limits(1150), bindings(1));
        let mut br = receiver(&cx, limits(1150), bindings(11));
        let f = capture(&owner, &mut s, &pool, true).await;
        f.distribute(&mut [&mut a, &mut b]).unwrap();
        drop(f);
        pump(&cx, &mut a, &mut ar);
        pump(&cx, &mut b, &mut br);
        decoded(&cx, &mut ar);
        decoded(&cx, &mut br);
        let f = capture(&owner, &mut s, &pool, false).await;
        b.enqueue_shared_capture(&f).unwrap();
        drop(f);
        pump(&cx, &mut b, &mut br);
        decoded(&cx, &mut br);
        let f = capture(&owner, &mut s, &pool, false).await;
        let report = f.distribute(&mut [&mut a, &mut b]).unwrap();
        assert_eq!(
            report.results(),
            &[Err(Error::Send(SendError::InvalidDependency)), Ok(())]
        );
        assert_eq!(a.cache_usage(), BudgetUsage::default());
        assert!(!ac.view_ready().unwrap());
        assert!(!input_live(&cx, &ai));
        assert!(input_live(&cx, &bi));
        pump(&cx, &mut b, &mut br);
        assert_eq!(decoded(&cx, &mut br).descriptor().frame, 2);
        owner.check().unwrap();
        drop(f);
        b.close();
        assert_eq!(pool.usage(), BudgetUsage::default());
        stop(&mut s, &cx).await;
    });
}
#[test]
fn equal_numbers_from_a_foreign_native_source_do_not_change_the_accepted_view() {
    run_shared!(rt, cx, {
        let (owner, _) = gate(&rt, 1);
        let (ac, ai) = gate(&rt, 2);
        let mut s = source(&owner, true).await;
        let mut foreign = source(&owner, true).await;
        let pool = pool();
        let mut a = egress(&ac, limits(1150), bindings(1), SendPolicy::default());
        let mut ar = receiver(&cx, limits(1150), bindings(1));
        let f = capture(&owner, &mut s, &pool, true).await;
        a.enqueue_shared_capture(&f).unwrap();
        drop(f);
        pump(&cx, &mut a, &mut ar);
        decoded(&cx, &mut ar);
        let used = a.cache_usage();
        let wrong = capture(&owner, &mut foreign, &pool, true).await;
        assert_eq!(wrong.frame().as_raw(), 0);
        assert_eq!(a.enqueue_shared_capture(&wrong), Err(Error::InvalidFrame));
        assert_eq!(a.cache_usage(), used);
        assert!(ac.view_ready().unwrap());
        assert!(input_live(&cx, &ai));
        let f = capture(&owner, &mut s, &pool, false).await;
        a.enqueue_shared_capture(&f).unwrap();
        pump(&cx, &mut a, &mut ar);
        assert_eq!(decoded(&cx, &mut ar).descriptor().frame, 1);
        stop(&mut s, &cx).await;
        stop(&mut foreign, &cx).await;
    });
}
#[test]
fn a_small_viewer_budget_refuses_without_pinning_history_or_disrupting_other_viewers() {
    run_shared!(rt, cx, {
        let (owner, _) = gate(&rt, 1);
        let (ac, ai) = gate(&rt, 2);
        let (bc, _) = gate(&rt, 3);
        let mut s = source(&owner, true).await;
        let pool = pool();
        let f = capture(&owner, &mut s, &pool, true).await;
        let mut a = egress(
            &ac,
            limits(1150),
            bindings(1),
            SendPolicy {
                max_cached_bytes: 256,
                repair_bytes_per_window: 128,
                ..SendPolicy::default()
            },
        );
        let mut b = egress(&bc, limits(1150), bindings(11), SendPolicy::default());
        let mut br = receiver(&cx, limits(1150), bindings(11));
        assert_eq!(
            f.distribute(&mut [&mut a, &mut b]).unwrap().results(),
            &[Err(Error::Send(SendError::CacheFull)), Ok(())]
        );
        assert_eq!(a.cache_usage(), BudgetUsage::default());
        assert!(!input_live(&cx, &ai));
        assert_eq!(pool.usage().pictures, 1);
        pump(&cx, &mut b, &mut br);
        assert_eq!(decoded(&cx, &mut br).descriptor().frame, 0);
        drop(f);
        b.close();
        assert_eq!(pool.usage(), BudgetUsage::default());
        owner.check().unwrap();
        stop(&mut s, &cx).await;
    });
}
#[test]
fn one_native_idr_recovers_one_egress_without_resetting_the_healthy_viewer() {
    run_shared!(rt, cx, {
        let (owner, _) = gate(&rt, 1);
        let (ac, ai) = gate(&rt, 2);
        let (bc, bi) = gate(&rt, 3);
        let mut s = source(&owner, true).await;
        let pid = s.worker_id();
        let pool = pool();
        let mut a = egress(&ac, limits(1150), bindings(1), SendPolicy::default());
        let mut b = egress(&bc, limits(1150), bindings(11), SendPolicy::default());
        let mut ar = receiver(&cx, limits(1150), bindings(1));
        let mut br = receiver(&cx, limits(1150), bindings(11));
        let f = capture(&owner, &mut s, &pool, true).await;
        f.distribute(&mut [&mut a, &mut b]).unwrap();
        drop(f);
        pump(&cx, &mut a, &mut ar);
        pump(&cx, &mut b, &mut br);
        decoded(&cx, &mut ar);
        decoded(&cx, &mut br);
        let binding = Binding {
            parent: ControlBinding {
                id: 10,
                host_boot: HostBootId::from_raw(1),
                os_session: OsSessionId::from_raw(2),
                remote_session: RemoteSessionId::from_raw(2),
            },
            display: 4,
            geometry: DisplayGeometryGeneration::INITIAL,
            configuration: epoch().configuration,
            recovery: epoch().recovery,
            viewport: ViewportMappingGeneration::INITIAL,
        };
        let mut bytes = [0; recovery_request::REQUEST_BYTES];
        recovery_request::encode(
            Request {
                reason: Reason::ReferenceExpired,
                last_useful_frame: Some(0),
            },
            binding,
            &ProtocolLimits::ABSOLUTE,
            &mut bytes,
            InputDirection::ViewerToHost,
            InputDelivery::Reliable,
        )
        .unwrap();
        assert!(a.request_recovery(&mut s, &bytes, binding).unwrap());
        let f = capture(&owner, &mut s, &pool, false).await;
        assert!(matches!(
            f.encoded().unwrap().kind(),
            fr_media::access_unit::FrameKind::Idr { .. }
        ));
        let next = MediaEpoch {
            recovery: epoch().recovery.next().unwrap(),
            ..epoch()
        };
        // Model separately admitted replacement channels; this is NOT an input grant.
        a.recover(next, bindings(21)).unwrap();
        ar.replace(next, bindings(21), host_now(&cx).unwrap().as_micros())
            .unwrap();
        ar.decoder_configured(host_now(&cx).unwrap().as_micros())
            .unwrap();
        assert_eq!(
            f.distribute(&mut [&mut a, &mut b]).unwrap().results(),
            &[Ok(()), Ok(())]
        );
        drop(f);
        assert!(pump(&cx, &mut a, &mut ar).contains(&Channel::Recovery));
        assert!(pump(&cx, &mut b, &mut br).contains(&Channel::Video));
        assert_eq!(decoded(&cx, &mut ar).epoch(), next);
        assert_eq!(decoded(&cx, &mut br).epoch(), epoch());
        let f = capture(&owner, &mut s, &pool, false).await;
        f.distribute(&mut [&mut a, &mut b]).unwrap();
        pump(&cx, &mut a, &mut ar);
        pump(&cx, &mut b, &mut br);
        assert_eq!(decoded(&cx, &mut ar).descriptor().frame, 2);
        assert_eq!(decoded(&cx, &mut br).descriptor().frame, 2);
        assert!(!input_live(&cx, &ai));
        assert!(input_live(&cx, &bi));
        assert_eq!(s.worker_id(), pid);
        stop(&mut s, &cx).await;
    });
}
#[test]
fn fanout_turn_limit_is_checked_before_mutating_any_recipient() {
    run_shared!(rt, cx, {
        let (owner, _) = gate(&rt, 1);
        let mut s = source(&owner, true).await;
        let pool = pool();
        let f = capture(&owner, &mut s, &pool, true).await;
        let mut all = (2..11)
            .map(|id| {
                let (c, _) = gate(&rt, id);
                egress(&c, limits(1150), bindings(1), SendPolicy::default())
            })
            .collect::<Vec<_>>();
        assert!(matches!(
            f.distribute(&mut all.iter_mut().collect::<Vec<_>>()),
            Err(Error::InvalidFrame)
        ));
        assert!(
            all.iter()
                .all(|e| e.cache_usage() == BudgetUsage::default())
        );
        assert!(f.distribute(&mut []).is_err());
        assert!(
            f.distribute(&mut all[..8].iter_mut().collect::<Vec<_>>())
                .unwrap()
                .results()
                .iter()
                .all(Result::is_ok)
        );
        assert_eq!(pool.usage().pictures, 1);
        stop(&mut s, &cx).await;
    });
}

#[path = "shared_capture/reservation.rs"]
mod reservation;

#[path = "shared_capture/production.rs"]
mod production;
