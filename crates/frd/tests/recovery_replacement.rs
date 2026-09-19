#![cfg(target_os = "linux")]
//! Real TLS/UDP and completed media attachments. Decode completion is simulated;
//! these tests do not qualify HEVC, native presentation or live-tailnet admission.
#[path = "../../fr-transport/tests/support/mod.rs"]
#[allow(dead_code)]
mod net;
use asupersync::cx::Cx;
use fr_core::{ids::*, limits::ProtocolLimits};
use fr_media::delivery::{MediaBudget, ReceivePipeline, ReceivePolicy};
use fr_transport::quic::{self, *};
use fr_wire::{
    attachment::{self, MediaRole, Ticket},
    decoder::{self, Binding},
    negotiation::{Capability, ControlBinding, Offer, Role, Selection},
    recovery_request as wire,
};
use frd::media_quic::{
    NegotiatedMedia,
    replacement::{Error as ReplacementError, Replacement},
};
use std::{cell::Cell, time::Duration};
fn parent() -> ControlBinding {
    ControlBinding {
        id: 7,
        host_boot: HostBootId::from_raw(11),
        os_session: OsSessionId::from_raw(12),
        remote_session: RemoteSessionId::from_raw(13),
    }
}
fn binding(id: u32) -> Binding {
    Binding {
        parent: ControlBinding { id, ..parent() },
        display: 14,
        geometry: DisplayGeometryGeneration::INITIAL,
        configuration: CodecConfigurationGeneration::INITIAL,
        recovery: RecoveryGeneration::INITIAL,
        viewport: ViewportMappingGeneration::INITIAL,
    }
}
struct Link {
    c: QuicRecords,
    h: QuicRecords,
    cr: ControlRoutes,
    hr: ControlRoutes,
    selection: Selection,
}
impl Link {
    async fn new(cx: &Cx, enabled: bool) -> Self {
        let (c, h) = net::native_pair(cx, "localhost", ALPN).await;
        let policy = Policy {
            critical_send_records: 1,
            ..Policy::default()
        };
        let (mut c, cr) = QuicRecords::bootstrap(c.unwrap(), cx, policy).unwrap();
        let (mut h, hr) = QuicRecords::bootstrap(h.unwrap(), cx, policy).unwrap();
        let cr = c.bind_control(cx, cr, 7, 4096, || true).unwrap();
        let hr = h.bind_control(cx, hr, 7, 4096, || true).unwrap();
        let mut capabilities: Vec<_> = [
            decoder::CAPABILITY,
            attachment::CAPABILITY,
            attachment::DELIVERY_CAPABILITY,
        ]
        .into_iter()
        .map(|name| Capability {
            name: name.into(),
            version: 1,
            required: true,
        })
        .collect();
        if enabled {
            capabilities.push(Capability {
                name: wire::CAPABILITY.into(),
                version: wire::VERSION,
                required: true,
            });
        }
        capabilities.sort_by(|a, b| a.name.cmp(&b.name));
        let selection = Offer {
            versions: vec![0],
            profile: 1,
            profile_version: 0,
            role: Role::Observe,
            limits: ProtocolLimits::ABSOLUTE,
            capabilities,
        }
        .select()
        .unwrap();
        Self {
            c,
            h,
            cr,
            hr,
            selection,
        }
    }
    async fn drive(&mut self, cx: &Cx) {
        let (a, b) = Box::pin(net::both(
            self.h.drive(cx, Duration::from_millis(1), || true),
            self.c.drive(cx, Duration::from_millis(1), || true),
        ))
        .await;
        a.unwrap();
        b.unwrap();
    }
    async fn attach(&mut self, cx: &Cx, role: MediaRole, id: u32) -> (MediaChannel, MediaChannel) {
        let until = net::clock(cx) + 1_500_000;
        let mut h = self
            .h
            .offer_media_role(
                cx,
                ChannelScope {
                    control: self.hr,
                    parent: parent(),
                    selection: &self.selection,
                },
                ChannelRequest {
                    binding: binding(id),
                    ticket: Ticket(1000 + u128::from(id)),
                    timeout: Duration::from_secs(2),
                },
                role,
                || true,
            )
            .unwrap();
        let mut sent = false;
        let mut c = loop {
            assert!(net::clock(cx) < until);
            if !sent {
                sent = h.transmit(&mut self.h, cx, || true).unwrap();
            }
            self.drive(cx).await;
            let ready = Cell::new(true);
            let mut record = None;
            self.c
                .receive_ready(
                    cx,
                    || true,
                    |r| ready.get() && r == Route::Stream(self.cr.inbound),
                    |_, b| {
                        ready.set(false);
                        record = Some(b.to_vec());
                        Ok(Disposition::Consumed)
                    },
                )
                .unwrap();
            if let Some(bytes) = record {
                break self
                    .c
                    .accept_media_channel(
                        cx,
                        ChannelScope {
                            control: self.cr,
                            parent: parent(),
                            selection: &self.selection,
                        },
                        &bytes,
                        Duration::from_secs(2),
                        || true,
                    )
                    .unwrap();
            }
        };
        loop {
            assert!(net::clock(cx) < until);
            h.transmit(&mut self.h, cx, || true).unwrap();
            c.transmit(&mut self.c, cx, || true).unwrap();
            self.drive(cx).await;
            h.dispatch(&mut self.h, cx, || true).unwrap();
            c.dispatch(&mut self.c, cx, || true).unwrap();
            let a = h.finish(&mut self.h, cx, || true).unwrap();
            let b = c.finish(&mut self.c, cx, || true).unwrap();
            if a.is_some() && b.is_some() {
                break;
            }
        }
        (h, c)
    }
    async fn media(&mut self, cx: &Cx) -> (NegotiatedMedia, NegotiatedMedia, ReceivePipeline) {
        let (hc, cc) = self.attach(cx, MediaRole::Configuration, 8).await;
        let (hr, cr) = self.attach(cx, MediaRole::Recovery, 9).await;
        let (hv, cv) = self.attach(cx, MediaRole::Video, 10).await;
        let host = NegotiatedMedia::new(&self.h, &self.selection, &hc, &hr, &hv).unwrap();
        let viewer = NegotiatedMedia::new(&self.c, &self.selection, &cc, &cr, &cv).unwrap();
        let cfg = viewer
            .receiver_config(&self.c, ReceivePolicy::default())
            .unwrap();
        let receiver =
            ReceivePipeline::new(cfg, MediaBudget::new(cfg.limits.protocol()).unwrap()).unwrap();
        (host, viewer, receiver)
    }
}
fn tickets() -> [Ticket; 3] {
    [Ticket(8001), Ticket(8002), Ticket(8003)]
}
async fn complete(link: &mut Link, cx: &Cx, host: &mut Replacement, viewer: &mut Replacement) {
    let original = (host.deadline_micros(), viewer.deadline_micros());
    loop {
        assert!(net::clock(cx) < original.0);
        let h = host.advance(cx, &mut link.h, || true).unwrap();
        let c = viewer.advance(cx, &mut link.c, || true).unwrap();
        assert_eq!((host.deadline_micros(), viewer.deadline_micros()), original);
        if h && c {
            break;
        }
        link.drive(cx).await;
    }
}
async fn replace(
    link: &mut Link,
    cx: &Cx,
    host: NegotiatedMedia,
    viewer: NegotiatedMedia,
) -> (NegotiatedMedia, NegotiatedMedia) {
    let until = net::clock(cx) + 1_500_000;
    let mut h = host
        .begin_replacement(
            cx,
            &mut link.h,
            link.hr,
            parent(),
            until,
            Some(tickets()),
            || true,
        )
        .unwrap();
    let mut c = viewer
        .begin_replacement(cx, &mut link.c, link.cr, parent(), until, None, || true)
        .unwrap();
    complete(link, cx, &mut h, &mut c).await;
    (
        h.finish(cx, &mut link.h).unwrap(),
        c.finish(cx, &mut link.c).unwrap(),
    )
}
fn observation(cx: &Cx) -> frd::media::ObservationControl {
    use fr_core::authority::{AuthorityPolicy, SessionAuthority};
    let now = frd::media::host_now(cx).unwrap();
    let mut authority =
        SessionAuthority::new(parent().remote_session, AuthorityPolicy::plan_defaults());
    authority.mark_capabilities_checked().unwrap();
    authority.authorize_observation(now).unwrap();
    frd::media::ObservationControl::new(cx.clone(), authority).unwrap()
}
async fn picture(
    link: &mut Link,
    cx: &Cx,
    sender: &mut frd::media_quic::QuicEgress,
    media: &NegotiatedMedia,
    receiver: &mut ReceivePipeline,
    frame: u64,
) -> fr_media::delivery::DecodedFrame {
    use fr_media::access_unit::{EncodedAccessUnit, FrameId, FrameKind};
    sender
        .enqueue(
            EncodedAccessUnit::new(
                &ProtocolLimits::ABSOLUTE,
                FrameId::from_raw(frame),
                if frame == 0 {
                    FrameKind::Idr {
                        recovery: media.binding().recovery,
                    }
                } else {
                    FrameKind::Predicted {
                        references: FrameId::FIRST,
                    }
                },
                media.binding().configuration,
                net::clock(cx),
                vec![7; 3000],
            )
            .unwrap(),
        )
        .unwrap();
    let until = net::clock(cx) + 200_000;
    loop {
        assert!(net::clock(cx) < until);
        sender
            .transmit(cx, &mut link.h, frd::media_egress::Lane::Original)
            .unwrap();
        link.drive(cx).await;
        media
            .receive_ready(
                cx,
                &mut link.c,
                || true,
                |channel, b| {
                    receiver.receive(channel, b, net::clock(cx)).unwrap();
                    Ok(Disposition::Consumed)
                },
            )
            .unwrap();
        if let Some(picture) = receiver.take_decodable(net::clock(cx)).unwrap() {
            assert_eq!(picture.descriptor().frame, frame);
            assert_eq!(picture.bytes(), &[7; 3000]);
            return receiver.complete_decode(&picture, net::clock(cx)).unwrap();
        }
    }
}
#[test]
#[allow(clippy::too_many_lines)] // One ordered scenario retains the same live owners throughout.
fn loss_report_fresh_media_and_resumed_frames_use_the_original_connection_and_receiver() {
    net::runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut link = Link::new(&cx, true).await;
        let (hm, vm, mut receiver) = link.media(&cx).await;
        let original = (link.h.binding(), link.c.binding());
        let old_repair = vm.repair_stream(&link.c).unwrap();
        let old_epoch = vm.binding();
        let mut reporter = vm
            .recovery_receiver(&link.c, link.cr, parent(), &receiver)
            .unwrap();
        receiver.decoder_configured(net::clock(&cx)).unwrap();
        let mut sender = hm
            .sender(
                &link.h,
                observation(&cx),
                fr_media::delivery::SendPolicy::default(),
            )
            .unwrap();
        let decoded = picture(&mut link, &cx, &mut sender, &vm, &mut receiver, 0).await;
        reporter.observe_decoded(&decoded).unwrap();
        // Announce an actual dependency loss over reliable transport, withholding
        // its video fragments. No direct fail flag or replacement pipeline.
        let stamp = net::clock(&cx);
        let mut buffer = [0; 1150];
        let length = fr_wire::encode_progress(
            fr_wire::Progress {
                descriptor: fr_wire::FrameDescriptor {
                    frame: 1,
                    total_bytes: 3000,
                    stride: vm.limits().fragment_stride(),
                    capture_micros: stamp,
                    reference: Some(0),
                },
                observed_micros: stamp,
                observation: fr_wire::SourceObservation::Captured,
                pipeline: fr_wire::PipelineState::Running,
            },
            vm.bindings().for_channel(fr_wire::Channel::MediaConfig),
            &vm.limits(),
            &mut buffer,
        )
        .unwrap();
        let route = Route::Stream(StreamRoute {
            outbound: true,
            ..vm.progress_route(&link.c).unwrap()
        });
        loop {
            match link
                .h
                .send(&cx, route, &buffer[..length], stamp + 1_000_000, || true)
            {
                Ok(()) => break,
                Err(quic::Error::Backpressure) => link.drive(&cx).await,
                error => panic!("loss announcement: {error:?}"),
            }
        }
        let until = net::clock(&cx) + 1_000_000;
        loop {
            assert!(net::clock(&cx) < until);
            link.drive(&cx).await;
            vm.receive_ready(
                &cx,
                &mut link.c,
                || true,
                |channel, b| {
                    receiver.receive(channel, b, net::clock(&cx)).unwrap();
                    Ok(Disposition::Consumed)
                },
            )
            .unwrap();
            if receiver
                .latest_progress()
                .is_some_and(|p| p.descriptor.frame == 1)
            {
                break;
            }
        }
        let expired = receiver.next_deadline().unwrap();
        asupersync::time::sleep_until(asupersync::types::Time::from_nanos((expired + 1000) * 1000))
            .await;
        let mut reports = 0;
        while reports == 0 {
            assert!(net::clock(&cx) < until);
            reporter
                .service(&cx, &mut link.c, &mut receiver, || true)
                .unwrap();
            link.drive(&cx).await;
            link.h
                .receive_ready(
                    &cx,
                    || true,
                    |r| r == Route::Stream(link.hr.inbound),
                    |_, bytes| {
                        let report = wire::decode(
                            bytes,
                            binding(7),
                            &ProtocolLimits::ABSOLUTE,
                            fr_wire::input::InputDirection::ViewerToHost,
                            fr_wire::input::InputDelivery::Reliable,
                        )
                        .unwrap();
                        assert_eq!(report.reason, wire::Reason::ReferenceExpired);
                        assert_eq!(report.last_useful_frame, Some(0));
                        reports += 1;
                        Ok(Disposition::Consumed)
                    },
                )
                .unwrap();
        }
        assert_eq!(reports, 1);
        let deadline = reporter.next_deadline().unwrap();
        sender.close(); // Stop old application offers before resetting native streams.
        let mut h = hm
            .begin_replacement(
                &cx,
                &mut link.h,
                link.hr,
                parent(),
                deadline,
                Some(tickets()),
                || true,
            )
            .unwrap();
        let mut c = vm
            .begin_replacement(&cx, &mut link.c, link.cr, parent(), deadline, None, || true)
            .unwrap();
        complete(&mut link, &cx, &mut h, &mut c).await;
        assert_eq!(h.deadline_micros(), deadline);
        let hm = h.finish(&cx, &mut link.h).unwrap();
        let vm = c.finish(&cx, &mut link.c).unwrap();
        assert!(link.h.is_bound_to(&original.0));
        assert!(link.c.is_bound_to(&original.1));
        assert!(!link.c.has_route(Route::Stream(old_repair)));
        assert_eq!(vm.binding().recovery, old_epoch.recovery.next().unwrap());
        assert_eq!(vm.binding().configuration, old_epoch.configuration);
        assert!(vm.binding().parent.id > 10);
        assert_eq!(
            receiver.state(),
            fr_media::delivery::ReceiveState::NeedsRecovery
        );
        receiver
            .replace(
                fr_media::delivery::MediaEpoch {
                    configuration: vm.binding().configuration,
                    recovery: vm.binding().recovery,
                },
                vm.bindings(),
                net::clock(&cx),
            )
            .unwrap();
        assert!(
            reporter
                .service(&cx, &mut link.c, &mut receiver, || true)
                .is_err()
        );
        assert!(!link.c.is_closed());
        receiver.decoder_configured(net::clock(&cx)).unwrap();
        let mut sender = hm
            .sender(
                &link.h,
                observation(&cx),
                fr_media::delivery::SendPolicy::default(),
            )
            .unwrap();
        picture(&mut link, &cx, &mut sender, &vm, &mut receiver, 0).await;
        picture(&mut link, &cx, &mut sender, &vm, &mut receiver, 1).await;
        assert!(net::clock(&cx) < deadline);
        assert_eq!(
            receiver.state(),
            fr_media::delivery::ReceiveState::Streaming
        );
        assert!(!link.c.is_closed());
        assert!(!link.h.is_closed());
    });
}
#[test]
fn preflight_refusals_do_not_retire_healthy_media_or_consume_new_bindings() {
    net::runtime().block_on(async {
        let cx = Cx::current().unwrap();
        for case in 0..4 {
            let mut link = Link::new(&cx, case != 0).await;
            let (hm, vm, _) = link.media(&cx).await;
            let old = vm.repair_stream(&link.c).unwrap();
            let next = link.h.next_channel_binding().unwrap();
            let result = hm.begin_replacement(
                &cx,
                &mut link.h,
                link.hr,
                parent(),
                net::clock(&cx) + 1_000_000,
                if case == 1 {
                    Some([Ticket(1), Ticket(1), Ticket(2)])
                } else if case == 2 {
                    None
                } else {
                    Some(tickets())
                },
                || case != 3,
            );
            assert!(result.is_err());
            assert_eq!(link.h.next_channel_binding().unwrap(), next);
            assert!(link.c.has_route(Route::Stream(old)));
            vm.check(&link.c).unwrap();
            assert!(!link.h.is_closed());
        }
    });
}
#[test]
fn original_deadline_expires_while_waiting_for_first_offer_without_reopening_a_connection() {
    net::runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut link = Link::new(&cx, true).await;
        let (_, vm, _) = link.media(&cx).await;
        let until = net::clock(&cx) + 50_000;
        let mut c = vm
            .begin_replacement(&cx, &mut link.c, link.cr, parent(), until, None, || true)
            .unwrap();
        for _ in 0..10 {
            assert!(!c.advance(&cx, &mut link.c, || true).unwrap());
        }
        asupersync::time::sleep_until(asupersync::types::Time::from_nanos((until + 1000) * 1000))
            .await;
        assert_eq!(
            c.advance(&cx, &mut link.c, || true),
            Err(ReplacementError::Expired)
        );
        assert!(link.c.is_closed());
        assert_eq!(c.deadline_micros(), until);
        assert_eq!(
            c.advance(&cx, &mut link.c, || true),
            Err(ReplacementError::Closed)
        );
    });
}
#[test]
fn a_foreign_connection_cannot_advance_another_views_replacement() {
    net::runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut link = Link::new(&cx, true).await;
        let (_, vm, _) = link.media(&cx).await;
        let mut c = vm
            .begin_replacement(
                &cx,
                &mut link.c,
                link.cr,
                parent(),
                net::clock(&cx) + 1_500_000,
                None,
                || true,
            )
            .unwrap();
        let mut foreign = Link::new(&cx, true).await;
        let usage = foreign.c.usage();
        assert_eq!(
            c.advance(&cx, &mut foreign.c, || true),
            Err(ReplacementError::WrongBinding)
        );
        assert_eq!(foreign.c.usage(), usage);
        assert!(!foreign.c.is_closed());
        assert!(!c.advance(&cx, &mut link.c, || true).unwrap());
        assert_eq!(
            c.advance(&cx, &mut link.c, || false),
            Err(ReplacementError::Transport(quic::Error::Unauthorized))
        );
        assert!(link.c.is_closed());
    });
}
#[test]
fn namespace_exhaustion_refuses_before_discarding_the_working_replacement() {
    net::runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut link = Link::new(&cx, true).await;
        let (hm, vm, _) = link.media(&cx).await;
        let (hm, vm) = replace(&mut link, &cx, hm, vm).await;
        assert_eq!(link.h.remaining_channel_pairs(), 1);
        let next = link.h.next_channel_binding().unwrap();
        let result = hm.begin_replacement(
            &cx,
            &mut link.h,
            link.hr,
            parent(),
            net::clock(&cx) + 1_000_000,
            Some([Ticket(9001), Ticket(9002), Ticket(9003)]),
            || true,
        );
        assert!(matches!(
            result,
            Err(ReplacementError::Transport(quic::Error::Backpressure))
        ));
        assert_eq!(link.h.next_channel_binding().unwrap(), next);
        vm.check(&link.c).unwrap();
        assert!(!link.h.is_closed());
    });
}
#[test]
fn wrong_generation_offer_is_refused_before_new_viewer_route_allocation() {
    net::runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut link = Link::new(&cx, true).await;
        let (hm, vm, _) = link.media(&cx).await;
        // Leave the host with its first retired set and send a genuinely encoded
        // but inadmissible generation through the real control connection.
        let _host = hm
            .begin_replacement(
                &cx,
                &mut link.h,
                link.hr,
                parent(),
                net::clock(&cx) + 1_500_000,
                Some(tickets()),
                || true,
            )
            .unwrap();
        let mut c = vm
            .begin_replacement(
                &cx,
                &mut link.c,
                link.cr,
                parent(),
                net::clock(&cx) + 1_500_000,
                None,
                || true,
            )
            .unwrap();
        let mut wrong = binding(link.h.next_channel_binding().unwrap());
        wrong.recovery = wrong.recovery.next().unwrap().next().unwrap();
        let mut h = link
            .h
            .offer_media_role(
                &cx,
                ChannelScope {
                    control: link.hr,
                    parent: parent(),
                    selection: &link.selection,
                },
                ChannelRequest {
                    binding: wrong,
                    ticket: Ticket(9999),
                    timeout: Duration::from_secs(1),
                },
                MediaRole::Configuration,
                || true,
            )
            .unwrap();
        let initial = link.c.next_channel_binding().unwrap();
        let until = net::clock(&cx) + 1_000_000;
        loop {
            assert!(net::clock(&cx) < until);
            h.transmit(&mut link.h, &cx, || true).unwrap();
            link.drive(&cx).await;
            match c.advance(&cx, &mut link.c, || true) {
                Err(ReplacementError::WrongBinding) => break,
                Ok(false) => {}
                other => panic!("unexpected replacement result: {other:?}"),
            }
        }
        assert_eq!(link.c.next_channel_binding().unwrap(), initial);
        assert!(link.c.is_closed());
    });
}

#[test]
fn queued_control_precedes_replacement_offers_without_being_discarded_or_retimed() {
    net::runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut link = Link::new(&cx, true).await;
        let (hm, vm, _) = link.media(&cx).await;
        let until = net::clock(&cx) + 1_500_000;
        let mut challenge = [0; fr_wire::authority::MAX_AUTHORITY_BYTES];
        let length = fr_wire::authority::encode(
            fr_wire::authority::Message::Challenge {
                scope: fr_wire::authority::Scope::Observation,
                nonce: 345,
                deadline_micros: until,
            },
            fr_wire::authority::Binding {
                channel: parent().id,
                session: parent().remote_session,
            },
            &ProtocolLimits::ABSOLUTE,
            &mut challenge,
            fr_wire::input::InputDirection::HostToViewer,
            fr_wire::input::InputDelivery::Reliable,
        )
        .unwrap();
        loop {
            match link.h.send(
                &cx,
                Route::Stream(link.hr.outbound),
                &challenge[..length],
                until,
                || true,
            ) {
                Ok(()) => break,
                Err(quic::Error::Backpressure) => link.drive(&cx).await,
                other => panic!("challenge admission: {other:?}"),
            }
        }
        let mut h = hm
            .begin_replacement(
                &cx,
                &mut link.h,
                link.hr,
                parent(),
                until,
                Some(tickets()),
                || true,
            )
            .unwrap();
        let mut c = vm
            .begin_replacement(&cx, &mut link.c, link.cr, parent(), until, None, || true)
            .unwrap();
        let critical = link.h.usage().critical_send_bytes;
        assert_eq!(critical, length);
        for _ in 0..20 {
            assert!(!h.advance(&cx, &mut link.h, || true).unwrap());
            assert!(!c.advance(&cx, &mut link.c, || true).unwrap());
            assert_eq!(link.h.usage().critical_send_bytes, critical);
            assert_eq!(h.deadline_micros(), until);
        }
        let mut delivered = false;
        while !delivered {
            assert!(net::clock(&cx) < until);
            link.drive(&cx).await;
            link.c
                .receive_ready(
                    &cx,
                    || true,
                    |r| r == Route::Stream(link.cr.inbound),
                    |_, b| {
                        assert_eq!(b, &challenge[..length]);
                        delivered = true;
                        Ok(Disposition::Consumed)
                    },
                )
                .unwrap();
        }
        complete(&mut link, &cx, &mut h, &mut c).await;
        h.finish(&cx, &mut link.h).unwrap();
        c.finish(&cx, &mut link.c).unwrap();
        assert!(delivered);
        assert!(!link.h.is_closed());
        assert!(!link.c.is_closed());
    });
}

#[test]
fn dropping_a_started_replacement_keeps_the_original_partial_exchange_fence() {
    net::runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut link = Link::new(&cx, true).await;
        let (hm, _, _) = link.media(&cx).await;
        let mut h = hm
            .begin_replacement(
                &cx,
                &mut link.h,
                link.hr,
                parent(),
                net::clock(&cx) + 1_000_000,
                Some(tickets()),
                || true,
            )
            .unwrap();
        h.advance(&cx, &mut link.h, || true).unwrap();
        let next = link.h.next_channel_binding().unwrap();
        drop(h);
        assert!(link.h.tick(&cx, || true).is_err());
        assert!(link.h.is_closed());
        assert_eq!(link.h.next_channel_binding().unwrap(), next);
    });
}
