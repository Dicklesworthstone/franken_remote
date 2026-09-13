//! Real UDP/TLS, broker, worker-process supervision and stream ownership. The
//! decoder replies, local consent, source checks, visibility and OS sink are
//! explicitly simulated; these tests do not qualify HEVC pixels or hardware.
use super::*;
use crate::{
    input_agent::{Driver, Seat},
    input_quic::grant::{Event, GrantBroker},
    media::{ObservationControl, Presenter},
    session_startup::{
        ControlledHost, HostSession,
        running::{
            controlled::tests::attach,
            tests::{pair_initialized, run},
        },
        tests::support,
    },
    worker::Deadline,
};
use fr_client::input::Action;
use fr_core::{
    ids::*,
    input::*,
    input_submission::{
        Capabilities, Capability, InputSink, Operation as InputOperation, PlatformError, Submission,
    },
    time::HostInstant,
};
use fr_media::{delivery::*, freshness::ClockPolicy};
use fr_wire::{
    attachment::{self, MediaRole},
    control::Target,
    negotiation::Capability as WireCapability,
    *,
};
use std::{
    cell::Cell,
    collections::VecDeque,
    pin::Pin,
    sync::{Arc, Mutex},
    task::Poll,
};

struct Sink(Arc<Mutex<Vec<InputOperation>>>);
impl InputSink for Sink {
    fn prepare(&mut self, _: InputOperation) -> Result<(), PlatformError> {
        Ok(())
    }
    fn submit(&mut self, op: InputOperation) -> Submission {
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
    stream: StreamingViewer,
    host: HostSession,
    broker: GrantBroker,
    channels: NegotiatedInput,
    media: crate::media_quic::NegotiatedMedia,
    observation: ObservationControl,
    seat: Seat,
    request: Request,
    descriptor: FrameDescriptor,
    video: Route,
}
#[allow(clippy::too_many_lines)]
async fn fixture(c: &Cx, h: &Cx) -> Fixture {
    let mut capabilities: Vec<_> = [
        clock::CAPABILITY,
        decoder::CAPABILITY,
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
        host.authority
            .as_mut()
            .unwrap()
            .mark_view_ready(HostInstant::from_micros(now(h).unwrap()))
            .unwrap();
    })
    .await;
    let (hc, vc) = attach(&mut host, &mut viewer, c, h, MediaRole::Configuration, 8).await;
    let (hr, vr) = attach(&mut host, &mut viewer, c, h, MediaRole::Recovery, 9).await;
    let (hi, vi) = attach(&mut host, &mut viewer, c, h, MediaRole::Input, 10).await;
    let (hv, vv) = attach(&mut host, &mut viewer, c, h, MediaRole::Video, 11).await;
    let parent = host.binding();
    let selection = host.selection().clone();
    let media =
        crate::media_quic::NegotiatedMedia::new(host.io().unwrap().0, &selection, &hc, &hr, &hv)
            .unwrap();
    let viewer_media =
        crate::media_quic::NegotiatedMedia::new(viewer.io().unwrap().0, &selection, &vc, &vr, &vv)
            .unwrap();
    let channels = NegotiatedInput::new(viewer.io().unwrap().0, &selection, &vc, vi).unwrap();
    let input = NegotiatedInput::new(host.io().unwrap().0, &selection, &hc, hi).unwrap();
    let video = Route::Datagram(
        hv.completed_on(host.io().unwrap().0)
            .unwrap()
            .datagram
            .unwrap(),
    );
    host.enable_clock_sync().unwrap();
    viewer.enable_clock_sync(ClockPolicy::default()).unwrap();
    let mut n = 5000;
    while viewer.clock_correlation().unwrap().is_none() {
        let (host_result, viewer_result) = Box::pin(support::both(
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
        host_result.unwrap();
        viewer_result.unwrap();
    }
    let limits = viewer_media.limits();
    let mut receiver = ReceivePipeline::new(
        viewer_media
            .receiver_config(
                viewer.io().unwrap().0,
                ReceivePolicy {
                    display_budget_micros: 200_000,
                    ..ReceivePolicy::default()
                },
            )
            .unwrap(),
        MediaBudget::new(limits.protocol()).unwrap(),
    )
    .unwrap();
    let mut presenter =
        Presenter::stream_fixture(c, viewer.io().unwrap().0, &viewer_media, &mut receiver).await;
    let stamp = now(h).unwrap();
    let mut bytes = [0; 1150];
    let count = encode_recovery(
        RecoveryChunk {
            frame: 0,
            total_bytes: 4,
            offset: 0,
            capture_micros: stamp,
            bytes: b"data",
        },
        viewer_media.bindings().for_channel(Channel::Recovery),
        &limits,
        &mut bytes,
    )
    .unwrap();
    receiver
        .receive(Channel::Recovery, &bytes[..count], now(c).unwrap())
        .unwrap();
    let initial = presenter
        .present_next(c, &mut receiver)
        .await
        .unwrap()
        .unwrap();
    let descriptor = initial.decoded.descriptor();
    let count = encode_progress(
        Progress {
            descriptor,
            observed_micros: stamp,
            observation: SourceObservation::Captured,
            pipeline: PipelineState::Running,
        },
        viewer_media.bindings().for_channel(Channel::MediaConfig),
        &limits,
        &mut bytes,
    )
    .unwrap();
    receiver
        .receive(Channel::MediaConfig, &bytes[..count], now(c).unwrap())
        .unwrap();
    let mut stream = StreamingViewer::new(
        Peer::Observe {
            session: viewer,
            media: viewer_media,
        },
        presenter,
        receiver,
    )
    .unwrap();
    stream.initial = Some(initial);
    let seat = Seat::default();
    let observation = host.observation().unwrap();
    let broker = host.negotiated_control_broker(seat.clone(), input).unwrap();
    Fixture {
        stream,
        host,
        broker,
        channels,
        media,
        observation,
        seat,
        request: Request {
            parent,
            sequence: 1,
            target: target(),
        },
        descriptor,
        video,
    }
}
fn key(down: bool) -> Action<'static> {
    Action::Key {
        key: PhysicalKey::new(4).unwrap(),
        transition: if down {
            KeyTransition::Press
        } else {
            KeyTransition::Release
        },
    }
}
struct Source {
    pending: VecDeque<(Route, Vec<u8>, u64)>,
    descriptor: FrameDescriptor,
    next: u64,
    updates: bool,
}
impl Source {
    fn send(
        &mut self,
        q: &mut quic::QuicRecords,
        h: &Cx,
        media: &crate::media_quic::NegotiatedMedia,
        video: Route,
        start: u64,
    ) {
        let at = now(h).unwrap();
        if self.pending.is_empty() && at >= self.next {
            let changed = self.updates
                && self.descriptor.frame < 2
                && at >= start + (self.descriptor.frame + 1) * 200_000;
            if changed {
                self.descriptor = FrameDescriptor {
                    frame: self.descriptor.frame + 1,
                    reference: Some(self.descriptor.frame),
                    capture_micros: at,
                    total_bytes: 4,
                    stride: 4,
                };
            }
            let limits = media.limits();
            let mut bytes = vec![0; 1150];
            let count = encode_progress(
                Progress {
                    descriptor: self.descriptor,
                    observed_micros: if changed {
                        self.descriptor.capture_micros
                    } else {
                        at
                    },
                    observation: if changed {
                        SourceObservation::Captured
                    } else {
                        SourceObservation::QualifiedUnchanged
                    },
                    pipeline: PipelineState::Idle,
                },
                media.bindings().for_channel(Channel::MediaConfig),
                &limits,
                &mut bytes,
            )
            .unwrap();
            bytes.truncate(count);
            self.pending.push_back((
                Route::Stream(media.progress_for_test(q)),
                bytes,
                at + 150_000,
            ));
            if changed {
                let mut bytes = vec![0; 1150];
                let count = encode_fragment(
                    Fragment {
                        descriptor: self.descriptor,
                        index: 0,
                        bytes: b"next",
                    },
                    media.bindings().for_channel(Channel::Video),
                    &limits,
                    &mut bytes,
                )
                .unwrap();
                bytes.truncate(count);
                self.pending.push_back((video, bytes, at + 150_000));
            }
            self.next = at + 20_000;
        }
        while let Some((route, bytes, until)) = self.pending.front() {
            match q.send(h, *route, bytes, *until, || true) {
                Ok(()) => {
                    self.pending.pop_front();
                }
                Err(quic::Error::Backpressure) => break,
                Err(e) => panic!("source send {e:?}"),
            }
        }
    }
}
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
async fn host_loop(
    mut host: HostSession,
    mut broker: GrantBroker,
    media: &crate::media_quic::NegotiatedMedia,
    observation: &ObservationControl,
    video: Route,
    descriptor: FrameDescriptor,
    done: &Cell<bool>,
    effects: Arc<Mutex<Vec<InputOperation>>>,
    delay: u64,
    updates: bool,
) {
    let h = observation.context();
    let start = now(&h).unwrap();
    let mut driver: Option<Driver> = None;
    let mut n = 8000;
    let mut ticket = 1000;
    let mut source = Source {
        pending: VecDeque::new(),
        descriptor,
        next: 0,
        updates,
    };
    while !done.get() {
        assert!(now(&h).unwrap() < start + 5_000_000);
        source.send(host.io().unwrap().0, &h, media, video, start);
        broker.receive(host.io().unwrap().0, block).unwrap();
        if driver.is_none() && broker.request().is_some() && now(&h).unwrap() >= start + delay {
            let e = effects.clone();
            driver = Some(
                broker
                    .approve(
                        host.io().unwrap().0,
                        target(),
                        || Some((InputLeaseId::from_raw(19), InputTicketId::from_raw(23))),
                        move || Ok(Sink(e)),
                        |_| true,
                    )
                    .unwrap(),
            );
        }
        if let Some(d) = &mut driver {
            std::future::poll_fn(|task| {
                assert!(Pin::new(&mut *d).poll(task).is_pending());
                Poll::Ready(())
            })
            .await;
        }
        if broker
            .service(host.io().unwrap().0, Some(target()))
            .unwrap()
            == Event::GrantQueued
        {
            let input = broker.finish(host.io().unwrap().0, Some(target())).unwrap();
            let mut host: ControlledHost = host.into_controlled(input).unwrap();
            let mut driver = driver.unwrap();
            let mut shutdown = None;
            while !done.get() {
                std::future::poll_fn(|task| {
                    if shutdown.is_none()
                        && let Poll::Ready(result) = Pin::new(&mut driver).poll(task)
                    {
                        shutdown = Some(result);
                    }
                    Poll::Ready(())
                })
                .await;
                if host.control().is_stopped() {
                    break;
                }
                let q = host.io().unwrap().0;
                source.send(q, &h, media, video, start);
                if host
                    .drive(
                        Duration::from_millis(1),
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
                    .await
                    .is_err()
                {
                    break;
                }
            }
            host.close();
            assert!(
                match shutdown {
                    Some(value) => value,
                    None => driver.await,
                }
                .handoff_safe()
            );
            return;
        }
        if host
            .drive(
                Duration::from_millis(1),
                || {
                    n += 1;
                    Ok(n)
                },
                block,
            )
            .await
            .is_err()
        {
            break;
        }
    }
    host.close();
    if let Some(driver) = driver {
        assert!(driver.await.handoff_safe());
    }
}
#[allow(clippy::too_many_lines)]
async fn exercise(c: Cx, h: Cx, delay: u64, show: bool, map: bool, updates: bool, hold: u64) {
    let Fixture {
        mut stream,
        host,
        broker,
        channels,
        media,
        observation,
        seat,
        request,
        descriptor,
        video,
    } = Box::pin(fixture(&c, &h)).await;
    let worker = stream.worker_id().unwrap();
    let stop = stream.control();
    let done = Cell::new(false);
    let effects = Arc::new(Mutex::new(Vec::new()));
    let receipts = Cell::new(0);
    let mut sent = 0;
    let mut shown = None;
    let mut before_grant = 0;
    let mut active_at = None;
    let result = Box::pin(support::both(
        async {
            let result = stream
                .serve_requesting_control(
                    channels,
                    request,
                    Policy::default(),
                    |state, event| {
                        match state {
                            State::Requesting(pending) => {
                                assert!(
                                    effects.lock().unwrap().is_empty(),
                                    "input before real grant"
                                );
                                if event.is_some() {
                                    before_grant += 1;
                                }
                                if map {
                                    pending
                                        .confirm_mapping(request.parent, request.target.view)
                                        .unwrap();
                                }
                                if show
                                    && let Some(p) = pending.presentation()
                                    && shown != Some(p.frame)
                                {
                                    pending.visible(p.frame.as_raw()).unwrap();
                                    shown = Some(p.frame);
                                }
                            }
                            State::Controlled(viewer) => {
                                assert!(show && map);
                                let start = *active_at.get_or_insert_with(|| now(&c).unwrap());
                                if let Some(p) = event {
                                    viewer.visible(p.frame.as_raw()).unwrap();
                                }
                                if sent == 0 {
                                    let _ = viewer.action(key(true)).unwrap();
                                    sent = 1;
                                } else if sent == 1 && receipts.get() >= 1 {
                                    let _ = viewer.action(key(false)).unwrap();
                                    sent = 2;
                                }
                                if receipts.get() == 2 && now(&c).unwrap() >= start + hold {
                                    stop.stop();
                                    observation.revoke();
                                }
                            }
                            State::Observing => panic!("lost the explicitly requested handoff"),
                        }
                        Ok(())
                    },
                    |_| receipts.set(receipts.get() + 1),
                    block,
                )
                .await;
            done.set(true);
            result
        },
        host_loop(
            host,
            broker,
            &media,
            &observation,
            video,
            descriptor,
            &done,
            effects.clone(),
            delay,
            updates,
        ),
    ))
    .await
    .0;
    assert!(result.is_err());
    if show && map {
        assert_eq!(receipts.get(), 2, "promotion failed: {result:?}");
        assert_eq!(sent, 2);
        assert!(stream.last_result().is_some());
        if updates {
            assert!(before_grant >= 2, "decoder stalled during approval");
            assert_eq!(stream.statistics().decoded, 2);
        } else {
            assert_eq!(
                stream.statistics().decoded,
                0,
                "static initial frame was decoded again"
            );
        }
    } else {
        assert_eq!(receipts.get(), 0);
        assert!(effects.lock().unwrap().is_empty());
        assert_eq!(result, Err(Error::Control(ControlError::Expired)));
    }
    assert_eq!(stream.worker_id(), Some(worker));
    assert!(!seat.is_occupied());
    stream
        .reap_media(&h, Deadline::after(&h, Duration::from_secs(1)).unwrap())
        .await
        .unwrap();
}
#[test]
fn initial_native_completion_promotes_without_redecode_and_renews_same_session() {
    run(|c, h| exercise(c, h, 0, true, true, false, 3_100_000));
}
#[test]
fn approval_wait_keeps_decoder_and_observation_progressing_before_real_grant() {
    run(|c, h| exercise(c, h, 1_100_000, true, true, true, 0));
}
#[test]
fn received_grant_cannot_invent_platform_visibility() {
    run(|c, h| exercise(c, h, 0, false, true, false, 0));
}
#[test]
fn received_grant_cannot_confirm_local_mapping() {
    run(|c, h| exercise(c, h, 0, true, false, false, 0));
}
#[test]
fn abandoning_unpolled_live_request_closes_original_worker_and_connection() {
    run(|c, h| async move {
        let mut f = Box::pin(fixture(&c, &h)).await;
        drop(f.stream.serve_requesting_control(
            f.channels,
            f.request,
            Policy::default(),
            |_, _| panic!("UI ran"),
            |_| panic!("effect"),
            block,
        ));
        assert!(c.checkpoint().is_err());
        assert!(!f.seat.is_occupied());
        assert!(f.broker.request().is_none());
        assert!(f.stream.peer.parts().is_err());
        f.stream
            .reap_media(&h, Deadline::after(&h, Duration::from_secs(1)).unwrap())
            .await
            .unwrap();
    });
}
#[test]
fn unpolled_time_consumes_live_control_request_budget() {
    run(|c, h| async move {
        let mut f = Box::pin(fixture(&c, &h)).await;
        let request = f.stream.serve_requesting_control(
            f.channels,
            f.request,
            Policy::default(),
            |_, _| panic!("UI ran"),
            |_| panic!("effect"),
            block,
        );
        asupersync::time::sleep(c.now(), Duration::from_millis(2050)).await;
        assert_eq!(request.await, Err(Error::Control(ControlError::Expired)));
        assert!(!f.seat.is_occupied());
        assert!(f.broker.request().is_none());
        f.stream
            .reap_media(&h, Deadline::after(&h, Duration::from_secs(1)).unwrap())
            .await
            .unwrap();
    });
}

#[test]
fn foreign_control_target_is_refused_before_request_or_native_effect() {
    run(|c, h| async move {
        let mut f = Box::pin(fixture(&c, &h)).await;
        f.request.parent.remote_session = RemoteSessionId::from_raw(99);
        let result = f
            .stream
            .serve_requesting_control(
                f.channels,
                f.request,
                Policy::default(),
                |_, _| panic!("UI ran"),
                |_| panic!("effect"),
                block,
            )
            .await;
        assert_eq!(result, Err(Error::Control(ControlError::WrongBinding)));
        assert!(!f.seat.is_occupied());
        assert!(f.broker.request().is_none());
        assert!(c.checkpoint().is_err());
        f.stream
            .reap_media(&h, Deadline::after(&h, Duration::from_secs(1)).unwrap())
            .await
            .unwrap();
    });
}
#[test]
fn foreign_same_numbered_initial_decode_cannot_be_promoted() {
    run(|c, h| async move {
        let mut f = Box::pin(fixture(&c, &h)).await;
        let (session, media) = f.stream.peer.parts().unwrap();
        let config = media
            .receiver_config(&session.transport, ReceivePolicy::default())
            .unwrap();
        let mut foreign =
            ReceivePipeline::new(config, MediaBudget::new(config.limits.protocol()).unwrap())
                .unwrap();
        let at = now(&c).unwrap();
        foreign.decoder_configured(at).unwrap();
        let mut bytes = [0; 1150];
        let count = encode_recovery(
            RecoveryChunk {
                frame: 0,
                total_bytes: 4,
                offset: 0,
                capture_micros: at,
                bytes: b"data",
            },
            config.bindings.for_channel(Channel::Recovery),
            &config.limits,
            &mut bytes,
        )
        .unwrap();
        foreign
            .receive(Channel::Recovery, &bytes[..count], at)
            .unwrap();
        let unit = foreign.take_decodable(at).unwrap().unwrap();
        f.stream.initial.as_mut().unwrap().decoded = foreign.complete_decode(&unit, at).unwrap();
        let result = f
            .stream
            .serve_requesting_control(
                f.channels,
                f.request,
                Policy::default(),
                |_, _| panic!("foreign presentation reached UI"),
                |_| panic!("effect"),
                block,
            )
            .await;
        assert_eq!(
            result,
            Err(Error::Freshness(fr_media::freshness::Error::StaleBinding))
        );
        assert!(!f.seat.is_occupied());
        assert!(f.broker.request().is_none());
        f.stream
            .reap_media(&h, Deadline::after(&h, Duration::from_secs(1)).unwrap())
            .await
            .unwrap();
    });
}
#[test]
fn cancelling_during_decode_closes_before_releasing_original_receiver() {
    run(|c, h| async move {
        let mut f = Box::pin(fixture(&c, &h)).await;
        let at = now(&c).unwrap();
        let (_, media) = f.stream.peer.parts().unwrap();
        let descriptor = FrameDescriptor {
            frame: 1,
            reference: Some(0),
            capture_micros: at,
            total_bytes: 4,
            stride: 4,
        };
        let mut bytes = [0; 1150];
        let count = encode_progress(
            Progress {
                descriptor,
                observed_micros: at,
                observation: SourceObservation::Captured,
                pipeline: PipelineState::Running,
            },
            media.bindings().for_channel(Channel::MediaConfig),
            &media.limits(),
            &mut bytes,
        )
        .unwrap();
        f.stream
            .receiver
            .receive(Channel::MediaConfig, &bytes[..count], at)
            .unwrap();
        let count = encode_fragment(
            Fragment {
                descriptor,
                index: 0,
                bytes: b"next",
            },
            media.bindings().for_channel(Channel::Video),
            &media.limits(),
            &mut bytes,
        )
        .unwrap();
        f.stream
            .receiver
            .receive(Channel::Video, &bytes[..count], at)
            .unwrap();
        let mut turns = 0;
        {
            let request = f.stream.serve_requesting_control(
                f.channels,
                f.request,
                Policy::default(),
                |_, _| {
                    turns += 1;
                    Ok(())
                },
                |_| panic!("effect"),
                block,
            );
            let mut request = std::pin::pin!(request);
            std::future::poll_fn(|task| {
                assert!(request.as_mut().poll(task).is_pending());
                Poll::Ready(())
            })
            .await;
        }
        // One executor poll may finish several ready network turns. The
        // relevant boundary is that the queued decoder job has not completed,
        // not how many advisory UI callbacks that poll happened to service.
        assert!(turns >= 1);
        assert_eq!(f.stream.statistics().decoded, 0);
        assert_eq!(f.stream.budget_usage().pictures, 0);
        assert!(c.checkpoint().is_err());
        assert_eq!(f.stream.receiver.state(), ReceiveState::Closed);
        assert!(!f.seat.is_occupied());
        assert!(f.broker.request().is_none());
        f.stream
            .reap_media(&h, Deadline::after(&h, Duration::from_secs(1)).unwrap())
            .await
            .unwrap();
    });
}
