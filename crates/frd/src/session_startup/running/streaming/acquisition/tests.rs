//! Actual UDP/TLS and two supervised media processes, with explicit synthetic
//! codec replies, consent/readiness, visibility and counted native input effects.
use super::*;
use crate::{
    media::{
        CaptureSource, PresentationStage, Presenter,
        streaming::{Policy, Stream},
    },
    media_egress::Lane,
    media_quic::NegotiatedMedia,
    session_startup::{
        HostControlState, StreamingViewer, ViewerControlState, now,
        running::{
            controlled::tests::attach,
            tests::{pair_initialized, run},
        },
        tests::support,
    },
    worker::{Deadline, Launch},
};
use asupersync::cx::Cx;
use fr_client::input::{Action, Policy as InputPolicy};
use fr_core::{
    ids::*,
    input::*,
    input_submission::{Capabilities, Capability, Operation as NativeOperation, Submission},
    time::HostInstant,
};
use fr_media::{
    delivery::{MediaBudget, ReceivePipeline, ReceivePolicy, SendPolicy},
    worker::Role,
};
use fr_wire::{
    attachment::{self, MediaRole},
    control::Target,
    negotiation::Capability as WireCapability,
};
use std::{
    cell::Cell,
    pin::Pin,
    sync::{Arc, Mutex},
    task::Poll,
};

struct Sink(Arc<Mutex<Vec<NativeOperation>>>);
impl InputSink for Sink {
    fn prepare(&mut self, _: NativeOperation) -> Result<(), PlatformError> {
        Ok(())
    }
    fn submit(&mut self, op: NativeOperation) -> Submission {
        self.0.lock().unwrap().push(op);
        Submission::Submitted
    }
}
#[allow(clippy::unnecessary_wraps)]
fn block(_: Route, _: &[u8]) -> Result<Disposition, ()> {
    Ok(Disposition::Blocked)
}
fn target() -> Target {
    Target {
        display_binding: 8,
        view: InputView {
            geometry: DisplayGeometryGeneration::INITIAL,
            viewport: ViewportMappingGeneration::INITIAL,
            configuration: CodecConfigurationGeneration::INITIAL,
            recovery: RecoveryGeneration::INITIAL,
        },
        bounds: InputBounds::new(DesktopPoint { x: 0, y: 0 }, 320, 240).unwrap(),
        capabilities: Capabilities::default().with(Capability::Keys),
    }
}
struct Fixture {
    host: StreamingHost,
    viewer: StreamingViewer,
    hi: NegotiatedInput,
    vi: NegotiatedInput,
    request: Request,
    seat: Seat,
}
#[allow(clippy::too_many_lines)]
async fn fixture(c: &Cx, h: &Cx, ready: bool, mode: &str) -> Fixture {
    let mut capabilities: Vec<_> = [
        fr_wire::clock::CAPABILITY,
        fr_wire::decoder::CAPABILITY,
        attachment::INPUT_CAPABILITY,
        attachment::CAPABILITY,
        attachment::DELIVERY_CAPABILITY,
        crate::input_quic::grant::CAPABILITY,
    ]
    .into_iter()
    .map(|name| WireCapability {
        name: name.into(),
        version: 1,
        required: true,
    })
    .collect();
    capabilities.sort_by(|a, b| a.name.cmp(&b.name));
    let (mut host, mut viewer) = pair_initialized(c, h, capabilities, |host| {
        if ready {
            host.authority
                .as_mut()
                .unwrap()
                .mark_view_ready(HostInstant::from_micros(now(h).unwrap()))
                .unwrap();
        }
    })
    .await;
    let (hc, vc) = attach(&mut host, &mut viewer, c, h, MediaRole::Configuration, 8).await;
    let (hr, vr) = attach(&mut host, &mut viewer, c, h, MediaRole::Recovery, 9).await;
    let (hi, vi) = attach(&mut host, &mut viewer, c, h, MediaRole::Input, 10).await;
    let (hv, vv) = attach(&mut host, &mut viewer, c, h, MediaRole::Video, 11).await;
    let parent = host.binding();
    let selection = host.selection().clone();
    let hi = NegotiatedInput::new(host.io().unwrap().0, &selection, &hc, hi).unwrap();
    let vi = NegotiatedInput::new(viewer.io().unwrap().0, &selection, &vc, vi).unwrap();
    let hm = NegotiatedMedia::new(host.io().unwrap().0, &selection, &hc, &hr, &hv).unwrap();
    let vm = NegotiatedMedia::new(viewer.io().unwrap().0, &selection, &vc, &vr, &vv).unwrap();
    host.enable_clock_sync().unwrap();
    viewer
        .enable_clock_sync(fr_media::freshness::ClockPolicy::default())
        .unwrap();
    let control = host.observation().unwrap();
    let configuration = super::super::tests::configuration();
    let mut source = CaptureSource::start(
        &control,
        Launch::new(
            &super::super::tests::fixture_path(mode),
            ":0",
            None,
            Role::Capture,
            1,
        )
        .unwrap(),
        configuration,
    )
    .await
    .unwrap();
    let first = source.capture_if_changed(&control, true).await.unwrap();
    let mut sender = hm
        .sender(host.io().unwrap().0, control.clone(), SendPolicy::default())
        .unwrap();
    sender.enqueue_capture(first).unwrap();
    let config = vm
        .receiver_config(
            viewer.io().unwrap().0,
            ReceivePolicy {
                display_budget_micros: 200_000,
                ..ReceivePolicy::default()
            },
        )
        .unwrap();
    let mut receiver =
        ReceivePipeline::new(config, MediaBudget::new(config.limits.protocol()).unwrap()).unwrap();
    let mut presenter =
        Presenter::stream_fixture(c, viewer.io().unwrap().0, &vm, &mut receiver).await;
    let mut n = 1000;
    let initial = loop {
        sender
            .transmit(h, host.io().unwrap().0, Lane::Original)
            .unwrap();
        let (host_turn, viewer_turn) = Box::pin(support::both(
            host.drive(
                Duration::from_millis(1),
                || {
                    n += 1;
                    Ok(n)
                },
                block,
            ),
            viewer.drive(Duration::from_millis(1), block),
        ))
        .await;
        host_turn.unwrap();
        viewer_turn.unwrap();
        vm.receive_ready(
            c,
            viewer.io().unwrap().0,
            || true,
            |channel, bytes| {
                receiver.receive(channel, bytes, now(c).unwrap()).unwrap();
                Ok(Disposition::Consumed)
            },
        )
        .unwrap();
        if viewer.clock_correlation().unwrap().is_some()
            && let Some(receipt) = presenter.present_next(c, &mut receiver).await.unwrap()
        {
            break receipt;
        }
    };
    let stream = Stream {
        source,
        sender,
        control,
        policy: Policy::default(),
        capacity: usize::try_from(configuration.max_access_unit_bytes).unwrap()
            + fr_media::worker::UNIT_PREFIX_BYTES,
        statistics: crate::media::streaming::Statistics::default(),
        served: false,
        pacing: None,
    };
    let host = host.into_streaming(stream).unwrap();
    let viewer = StreamingViewer::from_test_parts(viewer, vm, presenter, receiver, initial);
    Fixture {
        host,
        viewer,
        hi,
        vi,
        request: Request {
            parent,
            sequence: 1,
            target: target(),
        },
        seat: Seat::default(),
    }
}
fn key(pressed: bool) -> Action<'static> {
    Action::Key {
        key: PhysicalKey::new(4).unwrap(),
        transition: if pressed {
            KeyTransition::Press
        } else {
            KeyTransition::Release
        },
    }
}
#[derive(Clone, Copy)]
enum Case {
    Accept,
    NoConsent,
    NotReady,
    ChangeAfterGrant,
    NativeFailure,
}
#[allow(clippy::too_many_lines)]
async fn exercise(c: Cx, h: Cx, cleanup: Cx, case: Case, delay: u64, hold: u64) {
    let Fixture {
        mut host,
        mut viewer,
        hi,
        vi,
        request,
        seat,
    } = Box::pin(fixture(
        &c,
        &h,
        !matches!(case, Case::NotReady),
        "unchanged",
    ))
    .await;
    let host_pid = host.worker_id();
    let viewer_pid = viewer.worker_id();
    let stop = host.stream.control.clone();
    let view_stop = viewer.control();
    let effects = Arc::new(Mutex::new(Vec::new()));
    let (drivers, mut incoming) = asupersync::channel::mpsc::channel::<Driver>(1);
    let finished = Cell::new(false);
    let viewer_done = Cell::new(false);
    let receipts = Cell::new(0);
    let active = Cell::new(false);
    let factory_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut n = 6000;
    let mut ticket = 1000;
    let mut sampled = 0;
    let start = now(&h).unwrap();
    let mut sent = 0;
    let mut visible = None;
    let mut activated = None;
    let mut granted = false;
    let ((host_result, view_result), ()) = Box::pin(support::both(
        support::both(
            async {
                let result = host
                    .serve_accepting_control(
                        seat.clone(),
                        hi,
                        |state| {
                            sampled += 1;
                            match state {
                                HostControlState::Pending(mut pending) => {
                                    if pending.request().is_some()
                                        && pending.native_status().is_none()
                                        && now(&h).unwrap() >= start + delay
                                        && !matches!(case, Case::NoConsent)
                                    {
                                        let effects = effects.clone();
                                        let calls = factory_calls.clone();
                                        let fail = matches!(case, Case::NativeFailure);
                                        let driver = pending.approve(
                                            target(),
                                            || {
                                                Some((
                                                    InputLeaseId::from_raw(19),
                                                    InputTicketId::from_raw(23),
                                                ))
                                            },
                                            move || {
                                                calls.fetch_add(
                                                    1,
                                                    std::sync::atomic::Ordering::SeqCst,
                                                );
                                                if fail {
                                                    Err(PlatformError::Unavailable)
                                                } else {
                                                    Ok(Sink(effects))
                                                }
                                            },
                                            |_| true,
                                        )?;
                                        assert!(drivers.try_send(driver).is_ok());
                                    }
                                }
                                HostControlState::Active { .. } => {
                                    granted = true;
                                    if matches!(case, Case::ChangeAfterGrant) && receipts.get() >= 1
                                    {
                                        return Ok(None);
                                    }
                                }
                            }
                            Ok(Some(target()))
                        },
                        || {
                            n += 1;
                            Ok(n)
                        },
                        || {
                            ticket += 1;
                            Some(InputTicketId::from_raw(ticket))
                        },
                        block,
                    )
                    .await;
                finished.set(true);
                result
            },
            async {
                let result = viewer
                    .serve_requesting_control(
                        vi,
                        request,
                        InputPolicy::default(),
                        |state, event| {
                            match state {
                                ViewerControlState::Requesting(pending) => {
                                    pending
                                        .confirm_mapping(request.parent, request.target.view)
                                        .unwrap();
                                    if let Some(frame) = pending.presentation()
                                        && visible != Some(frame.frame)
                                    {
                                        assert_eq!(
                                            frame.stage,
                                            PresentationStage::SubmittedToCompositor
                                        );
                                        pending.visible(frame.frame.as_raw()).unwrap();
                                        visible = Some(frame.frame);
                                    }
                                }
                                ViewerControlState::Controlled(controlled) => {
                                    active.set(true);
                                    let at = *activated.get_or_insert_with(|| now(&c).unwrap());
                                    if let Some(frame) = event {
                                        controlled.visible(frame.frame.as_raw()).unwrap();
                                    }
                                    if sent == 0 {
                                        let _ = controlled.action(key(true)).unwrap();
                                        sent = 1;
                                    } else if sent == 1
                                        && receipts.get() >= 1
                                        && !matches!(case, Case::ChangeAfterGrant)
                                    {
                                        let _ = controlled.action(key(false)).unwrap();
                                        sent = 2;
                                    }
                                    if receipts.get() == 2 && now(&c).unwrap() >= at + hold {
                                        stop.revoke();
                                        view_stop.stop();
                                    }
                                }
                                ViewerControlState::Observing => panic!("lost control request"),
                            }
                            if finished.get() {
                                view_stop.stop();
                            }
                            Ok(())
                        },
                        |_| receipts.set(receipts.get() + 1),
                        block,
                    )
                    .await;
                viewer_done.set(true);
                stop.revoke();
                result
            },
        ),
        async {
            let mut driver = None;
            let mut completed = false;
            loop {
                if driver.is_none()
                    && let Ok(received) = incoming.try_recv()
                {
                    driver = Some(received);
                }
                if let Some(d) = &mut driver {
                    std::future::poll_fn(|task| {
                        if !completed && let Poll::Ready(result) = Pin::new(&mut *d).poll(task) {
                            assert!(result.handoff_safe());
                            completed = true;
                        }
                        Poll::Ready(())
                    })
                    .await;
                }
                if finished.get() && viewer_done.get() && (driver.is_none() || completed) {
                    break;
                }
                asupersync::time::sleep(h.now(), Duration::from_millis(1)).await;
            }
        },
    ))
    .await;
    assert!(host_result.is_err() && view_result.is_err());
    if matches!(case, Case::Accept) {
        assert!(
            granted && active.get(),
            "host={host_result:?} viewer={view_result:?}"
        );
        assert_eq!(receipts.get(), 2);
        assert!(host.statistics().unchanged_observations > 0);
        if hold > 3_000_000 {
            let Host::Control(controlled) = &host.host else {
                panic!("lost owner");
            };
            assert!(controlled.control_renewed_until().is_some());
            assert!(controlled.observation_renewed_until().is_some());
        }
    } else if matches!(case, Case::ChangeAfterGrant) {
        assert!(granted && active.get());
        assert_eq!(
            host_result,
            Err(Error::ControlGrant(GrantError::TargetChanged))
        );
        assert!(host.collect_after_close().unwrap().is_some());
    } else {
        assert!(!active.get());
        assert!(effects.lock().unwrap().is_empty());
        if matches!(case, Case::NoConsent) {
            assert_eq!(factory_calls.load(std::sync::atomic::Ordering::SeqCst), 0);
        }
    }
    assert!(sampled > 0);
    assert!(!seat.is_occupied());
    assert_eq!(host.worker_id(), host_pid);
    assert_eq!(viewer.worker_id(), viewer_pid);
    host.reap_media(
        &cleanup,
        Deadline::after(&cleanup, Duration::from_secs(1)).unwrap(),
    )
    .await
    .unwrap();
    viewer
        .reap_media(
            &cleanup,
            Deadline::after(&cleanup, Duration::from_secs(1)).unwrap(),
        )
        .await
        .unwrap();
}
#[test]
fn live_grant_preserves_capture_and_renews_original_input() {
    run3(|c, h, cleanup| exercise(c, h, cleanup, Case::Accept, 0, 3_100_000));
}
#[test]
fn approval_wait_keeps_capture_and_observation_running() {
    run3(|c, h, cleanup| exercise(c, h, cleanup, Case::Accept, 1_100_000, 0));
}
#[test]
fn missing_consent_never_starts_native_input() {
    run3(|c, h, cleanup| exercise(c, h, cleanup, Case::NoConsent, 0, 0));
}
#[test]
fn decoder_and_matching_target_cannot_invent_host_readiness() {
    run3(|c, h, cleanup| exercise(c, h, cleanup, Case::NotReady, 0, 0));
}
#[test]
fn target_loss_after_grant_fences_queued_input() {
    run3(|c, h, cleanup| exercise(c, h, cleanup, Case::ChangeAfterGrant, 0, 0));
}
#[test]
fn failed_native_initialization_never_publishes_a_grant() {
    run3(|c, h, cleanup| exercise(c, h, cleanup, Case::NativeFailure, 0, 0));
}
#[test]
fn unpolled_drop_closes_original_capture_without_native_reservation() {
    run(|c, h| async move {
        let mut f = Box::pin(fixture(&c, &h, true, "unchanged")).await;
        drop(f.host.serve_accepting_control(
            f.seat.clone(),
            f.hi,
            |_| panic!("local ran"),
            || panic!("nonce"),
            || panic!("ticket"),
            block,
        ));
        assert!(!f.seat.is_occupied());
        assert!(h.checkpoint().is_err());
        assert!(f.host.host.session().unwrap().opened.transport.is_closed());
        f.host
            .reap_media(&c, Deadline::after(&c, Duration::from_secs(1)).unwrap())
            .await
            .unwrap();
    });
}

fn run3<F, Fut>(f: F)
where
    F: FnOnce(Cx, Cx, Cx) -> Fut,
    Fut: Future<Output = ()>,
{
    let runtime = support::runtime();
    let c = runtime.request_cx_with_budget(asupersync::types::Budget::INFINITE);
    let h = runtime.request_cx_with_budget(asupersync::types::Budget::INFINITE);
    let cleanup = runtime.request_cx_with_budget(asupersync::types::Budget::INFINITE);
    runtime.block_on(async {
        asupersync::time::timeout(c.now(), Duration::from_secs(12), f(c, h, cleanup))
            .await
            .unwrap();
    });
}
