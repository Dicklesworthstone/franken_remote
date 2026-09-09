#![cfg(target_os = "linux")]
//! Real localhost UDP/TLS, with explicit consent/identity fixtures. Media tests
//! use opaque bytes and explicit completion tokens, not a fake native decoder.
#[path = "../../fr-transport/tests/support/mod.rs"]
#[allow(dead_code)]
mod network;
use asupersync::{cx::Cx, net::quic_native::NativeQuicUdpConnection, types::Budget};
use fr_client::input::ClientInstant;
use fr_core::{
    authority::{AuthorityPolicy, SessionAuthority},
    ids::{HostBootId, OsSessionId, RemoteSessionId},
    limits::ProtocolLimits,
};
use fr_media::freshness::{ClockCorrelation, ClockPolicy};
use fr_transport::quic::{
    ALPN, ControlRoutes, Disposition, Messages, Policy, Priority, QuicRecords, Route, StreamRoute,
};
use fr_wire::{
    clock::{self, Message},
    input::{InputDelivery, InputDirection},
    negotiation::{Capability, ControlBinding, Role, Selection},
};
use frd::media::renewal::ObservationRenewal;
use frd::media::{
    ObservationControl,
    clock::{ClockSync, Error, Event},
    host_now,
};
use std::time::Duration;
const LIMITS: ProtocolLimits = ProtocolLimits::ABSOLUTE;
fn binding() -> ControlBinding {
    ControlBinding {
        id: 17,
        host_boot: HostBootId::from_raw(1),
        os_session: OsSessionId::from_raw(2),
        remote_session: RemoteSessionId::from_raw(3),
    }
}
fn selection() -> Selection {
    Selection {
        version: 0,
        profile: 1,
        profile_version: 0,
        role: Role::Observe,
        limits: LIMITS,
        capabilities: vec![Capability {
            name: clock::CAPABILITY.into(),
            version: clock::VERSION,
            required: true,
        }],
    }
}
fn approved(cx: Cx) -> ObservationControl {
    let mut a = SessionAuthority::new(binding().remote_session, AuthorityPolicy::plan_defaults());
    a.mark_capabilities_checked().unwrap();
    a.require_approval().unwrap();
    a.authorize_observation(host_now(&cx).unwrap()).unwrap();
    ObservationControl::new(cx, a).unwrap()
}
fn reverse(routes: ControlRoutes) -> ControlRoutes {
    ControlRoutes {
        outbound: StreamRoute {
            outbound: true,
            ..routes.inbound
        },
        inbound: StreamRoute {
            outbound: false,
            ..routes.outbound
        },
    }
}
async fn native(cx: &Cx) -> (NativeQuicUdpConnection, QuicRecords, ControlRoutes) {
    let (c, s) = network::native_pair(cx, "localhost", ALPN).await;
    let (mut c, mut s) = (c.unwrap(), s.unwrap());
    let inbound = StreamRoute {
        stream: c.connection_mut().open_uni_stream(cx).unwrap(),
        binding: binding().id,
        messages: Messages::SessionControl,
        priority: Priority::Critical,
        maximum: 1024,
        outbound: false,
    };
    let outbound = StreamRoute {
        stream: s.connection_mut().open_uni_stream(cx).unwrap(),
        outbound: true,
        ..inbound
    };
    let policy = Policy {
        critical_send_records: 1,
        ..Policy::default()
    };
    let s = QuicRecords::new(s, cx, &[inbound, outbound], &[], policy).unwrap();
    (c, s, ControlRoutes { inbound, outbound })
}
async fn pair(cx: &Cx) -> (QuicRecords, QuicRecords, ControlRoutes) {
    let (c, s, routes) = native(cx).await;
    let r = reverse(routes);
    let c = QuicRecords::new(
        c,
        cx,
        &[r.inbound, r.outbound],
        &[],
        Policy {
            critical_send_records: 1,
            ..Policy::default()
        },
    )
    .unwrap();
    (c, s, routes)
}
#[allow(clippy::unnecessary_wraps)] // Exact transport callback signature.
fn blocked(_: Route, _: &[u8]) -> Result<Disposition, ()> {
    Ok(Disposition::Blocked)
}
async fn drive(
    c: &mut QuicRecords,
    s: &mut QuicRecords,
    viewer: &mut ClockSync,
    host: &mut ClockSync,
) {
    let (a, b) = Box::pin(network::both(
        viewer.drive(c, Duration::from_millis(2)),
        host.drive(s, Duration::from_millis(2)),
    ))
    .await;
    a.unwrap();
    b.unwrap();
}
async fn raw_drive(cx: &Cx, c: &mut QuicRecords, s: &mut QuicRecords) {
    let (a, b) = Box::pin(network::both(
        c.drive(cx, Duration::from_millis(2), || true),
        s.drive(cx, Duration::from_millis(2), || true),
    ))
    .await;
    a.unwrap();
    b.unwrap();
}
async fn measured(
    cx: &Cx,
    c: &mut QuicRecords,
    s: &mut QuicRecords,
    viewer: &mut ClockSync,
    host: &mut ClockSync,
) -> ClockCorrelation {
    let deadline = network::clock(cx) + 900_000;
    loop {
        assert!(
            network::clock(cx) < deadline,
            "real exchange did not complete"
        );
        viewer.service(c).unwrap();
        host.service(s).unwrap();
        drive(c, s, viewer, host).await;
        host.receive(s, blocked).unwrap();
        viewer.receive(c, blocked).unwrap();
        if let Some(result) = viewer.correlation(c).unwrap() {
            return result;
        }
    }
}
fn record(msg: Message) -> Vec<u8> {
    let mut bytes = [0; clock::REPLY_BYTES];
    let n = clock::encode(
        msg,
        binding(),
        &LIMITS,
        &mut bytes,
        if matches!(msg, Message::Probe { .. }) {
            InputDirection::ViewerToHost
        } else {
            InputDirection::HostToViewer
        },
        InputDelivery::Reliable,
    )
    .unwrap();
    bytes[..n].to_vec()
}
fn send(cx: &Cx, c: &mut QuicRecords, route: StreamRoute, bytes: &[u8]) {
    c.send(
        cx,
        Route::Stream(route),
        bytes,
        network::clock(cx) + 900_000,
        || true,
    )
    .unwrap();
}
#[test]
fn real_exchange_samples_both_ends_and_preserves_uncertainty_under_delayed_receive() {
    let rt = network::runtime();
    let cx = rt.request_cx_with_budget(Budget::INFINITE);
    let vc = rt.request_cx_with_budget(Budget::INFINITE);
    let hc = rt.request_cx_with_budget(Budget::INFINITE);
    rt.block_on(async {
        let (mut c, mut s, routes) = pair(&cx).await;
        let control = approved(hc);
        let until = control.deadline(Duration::from_secs(3)).unwrap().time();
        let mut host =
            ClockSync::host(control.clone(), &mut s, routes, binding(), &selection()).unwrap();
        let mut viewer = ClockSync::viewer(
            vc,
            &mut c,
            reverse(routes),
            binding(),
            &selection(),
            ClockPolicy::default(),
        )
        .unwrap();
        assert!(viewer.correlation(&mut c).unwrap().is_none());
        assert_eq!(viewer.service(&mut c), Ok(Event::ProbeQueued));
        let deadline = network::clock(&cx) + 900_000;
        loop {
            assert!(network::clock(&cx) < deadline);
            drive(&mut c, &mut s, &mut viewer, &mut host).await;
            if host.receive(&mut s, blocked).unwrap() > 0 {
                break;
            }
        }
        let after_sample = network::clock(&cx);
        // Delay host transmission, not just the test assertion. No new sample.
        std::thread::sleep(Duration::from_millis(70));
        assert_eq!(host.service(&mut s), Ok(Event::ReplyQueued));
        loop {
            assert!(network::clock(&cx) < deadline);
            drive(&mut c, &mut s, &mut viewer, &mut host).await;
            if viewer.receive(&mut c, blocked).unwrap() > 0 {
                break;
            }
        }
        let result = viewer.correlation(&mut c).unwrap().unwrap();
        assert!(
            result
                .age_upper_us(after_sample, result.received_at_us())
                .unwrap()
                >= 60_000,
            "queueing must not resample the host clock or reset viewer start"
        );
        assert_eq!(result.host_boot(), binding().host_boot);
        assert_eq!(
            control.deadline(Duration::from_secs(3)).unwrap().time(),
            until,
            "clock exchange cannot renew observation"
        );
    });
}
#[test]
fn client_send_backpressure_stays_inside_the_exchange_interval() {
    let rt = network::runtime();
    let cx = rt.request_cx_with_budget(Budget::INFINITE);
    let vc = rt.request_cx_with_budget(Budget::INFINITE);
    let hc = rt.request_cx_with_budget(Budget::INFINITE);
    rt.block_on(async {
        let (mut c, mut s, r) = pair(&cx).await;
        // Occupy actual critical send storage with a separate valid control record.
        let mut other = [0; 82];
        let n = fr_wire::authority::encode(
            fr_wire::authority::Message::Response {
                scope: fr_wire::authority::Scope::Observation,
                nonce: 7,
            },
            fr_wire::authority::Binding {
                channel: 17,
                session: binding().remote_session,
            },
            &LIMITS,
            &mut other,
            InputDirection::ViewerToHost,
            InputDelivery::Reliable,
        )
        .unwrap();
        send(&cx, &mut c, reverse(r).outbound, &other[..n]);
        let mut host = ClockSync::host(approved(hc), &mut s, r, binding(), &selection()).unwrap();
        let mut viewer = ClockSync::viewer(
            vc,
            &mut c,
            reverse(r),
            binding(),
            &selection(),
            ClockPolicy::default(),
        )
        .unwrap();
        assert_eq!(viewer.service(&mut c), Ok(Event::Backpressure));
        std::thread::sleep(Duration::from_millis(70));
        assert_eq!(viewer.service(&mut c), Ok(Event::Backpressure));
        let deadline = network::clock(&cx) + 800_000;
        let mut delegated = 0;
        let mut sample_before = 0;
        loop {
            assert!(network::clock(&cx) < deadline);
            drive(&mut c, &mut s, &mut viewer, &mut host).await;
            host.receive(&mut s, |_, bytes| {
                assert_eq!(bytes, &other[..n]);
                delegated += 1;
                Ok(Disposition::Consumed)
            })
            .unwrap();
            if viewer.service(&mut c).unwrap() == Event::ProbeQueued {
                break;
            }
        }
        while host.service(&mut s).unwrap() != Event::ReplyQueued {
            assert!(network::clock(&cx) < deadline);
            drive(&mut c, &mut s, &mut viewer, &mut host).await;
            sample_before = network::clock(&cx);
            host.receive(&mut s, blocked).unwrap();
        }
        loop {
            assert!(network::clock(&cx) < deadline);
            drive(&mut c, &mut s, &mut viewer, &mut host).await;
            viewer.receive(&mut c, blocked).unwrap();
            if let Some(result) = viewer.correlation(&mut c).unwrap() {
                assert!(
                    result
                        .age_upper_us(sample_before, result.received_at_us())
                        .unwrap()
                        >= 70_000
                );
                break;
            }
        }
        assert_eq!(delegated, 1);
    });
}
#[test]
fn missing_capability_wrong_routes_and_repeat_attachment_are_refused() {
    let rt = network::runtime();
    let cx = rt.request_cx_with_budget(Budget::INFINITE);
    let hc = rt.request_cx_with_budget(Budget::INFINITE);
    rt.block_on(async {
        let (_c, mut s, r) = pair(&cx).await;
        let control = approved(hc);
        let mut sel = selection();
        sel.capabilities.clear();
        assert!(matches!(
            ClockSync::host(control.clone(), &mut s, r, binding(), &sel),
            Err(Error::CapabilityMissing)
        ));
        assert!(control.check().is_ok());
        let wrong = ControlBinding {
            id: 18,
            ..binding()
        };
        assert!(matches!(
            ClockSync::host(control.clone(), &mut s, r, wrong, &selection()),
            Err(Error::Configuration)
        ));
        let owner = ClockSync::host(control.clone(), &mut s, r, binding(), &selection()).unwrap();
        assert!(matches!(
            ClockSync::host(control.clone(), &mut s, r, binding(), &selection()),
            Err(Error::Transport(_))
        ));
        assert!(control.check().is_ok());
        drop(owner);
        assert!(control.check().is_err());
        assert!(
            s.claim_clock(r).is_err(),
            "claim must stay consumed after drop"
        );
    });
}
#[test]
fn lost_reply_times_out_without_another_packet_and_cancels_viewer_session() {
    let rt = network::runtime();
    let cx = rt.request_cx_with_budget(Budget::INFINITE);
    let vc = rt.request_cx_with_budget(Budget::INFINITE);
    rt.block_on(async {
        let (mut c, mut s, r) = pair(&cx).await;
        let mut viewer = ClockSync::viewer(
            vc.clone(),
            &mut c,
            reverse(r),
            binding(),
            &selection(),
            ClockPolicy {
                max_exchange_us: 250_000,
                ..ClockPolicy::default()
            },
        )
        .unwrap();
        viewer.service(&mut c).unwrap();
        // Actually deliver and ACK the probe, but deliberately never reply.
        // This isolates exchange expiry from unsent transport-record expiry.
        let deadline = network::clock(&cx) + 200_000;
        let mut received = 0;
        while received == 0 || c.usage().critical_send_records != 0 {
            assert!(network::clock(&cx) < deadline);
            raw_drive(&cx, &mut c, &mut s).await;
            received += s
                .receive(
                    &cx,
                    || true,
                    |_, bytes| {
                        assert_eq!(bytes, record(Message::Probe { sequence: 1 }));
                        Ok(Disposition::Consumed)
                    },
                )
                .unwrap();
        }
        std::thread::sleep(Duration::from_millis(260));
        assert_eq!(
            viewer.service(&mut c),
            Err(Error::Client(fr_client::clock::Error::Expired))
        );
        assert!(c.is_closed());
        assert!(vc.checkpoint().is_err());
        assert!(viewer.correlation(&mut c).is_err());
    });
}
#[test]
fn unpolled_io_drop_cancels_only_its_bound_lifetime_not_a_successor() {
    let rt = network::runtime();
    let cx = rt.request_cx_with_budget(Budget::INFINITE);
    let hc = rt.request_cx_with_budget(Budget::INFINITE);
    rt.block_on(async {
        let (_c, mut s, r) = pair(&cx).await;
        let control = approved(hc);
        let mut host =
            ClockSync::host(control.clone(), &mut s, r, binding(), &selection()).unwrap();
        drop(host.drive(&mut s, Duration::from_millis(2)));
        assert!(s.is_closed());
        assert!(control.check().is_err());
        let (_other_c, mut other_s, _) = pair(&cx).await;
        assert_eq!(host.service(&mut other_s), Err(Error::ForeignConnection));
        assert!(!other_s.is_closed());
    });
}
#[test]
fn replayed_probe_or_wrong_boot_ends_host_observation() {
    for foreign in [false, true] {
        let rt = network::runtime();
        let cx = rt.request_cx_with_budget(Budget::INFINITE);
        let hc = rt.request_cx_with_budget(Budget::INFINITE);
        rt.block_on(async {
            let (mut c, mut s, r) = pair(&cx).await;
            let control = approved(hc);
            let mut host =
                ClockSync::host(control.clone(), &mut s, r, binding(), &selection()).unwrap();
            let mut bytes = record(Message::Probe {
                sequence: if foreign { 1 } else { 2 },
            });
            if foreign {
                bytes[39] ^= 1;
            }
            send(&cx, &mut c, reverse(r).outbound, &bytes);
            let deadline = network::clock(&cx) + 800_000;
            loop {
                assert!(network::clock(&cx) < deadline);
                raw_drive(&cx, &mut c, &mut s).await;
                if let Err(e) = host.receive(&mut s, blocked) {
                    assert!(matches!(e, Error::Sequence | Error::Wire(_)));
                    break;
                }
            }
            assert!(s.is_closed());
            assert!(control.check().is_err());
        });
    }
}
#[test]
fn actual_correlation_drives_view_freshness_but_cannot_invent_visibility() {
    use fr_core::ids::{CodecConfigurationGeneration, RecoveryGeneration};
    use fr_media::{
        delivery::*,
        freshness::{Error as ViewError, ViewTracker},
    };
    use fr_wire::{
        Channel, MediaLimits, PipelineState, Progress, RecoveryChunk, SourceObservation,
        encode_progress, encode_recovery,
    };
    let rt = network::runtime();
    let cx = rt.request_cx_with_budget(Budget::INFINITE);
    let vc = rt.request_cx_with_budget(Budget::INFINITE);
    let hc = rt.request_cx_with_budget(Budget::INFINITE);
    rt.block_on(async {
        let (mut client, mut server, routes) = pair(&cx).await;
        let control = approved(hc.clone());
        let mut host =
            ClockSync::host(control, &mut server, routes, binding(), &selection()).unwrap();
        let mut viewer = ClockSync::viewer(
            vc,
            &mut client,
            reverse(routes),
            binding(),
            &selection(),
            ClockPolicy::default(),
        )
        .unwrap();
        let sample = measured(&cx, &mut client, &mut server, &mut viewer, &mut host).await;
        let limits = MediaLimits::new(LIMITS, 1150, 16384, 64).unwrap();
        let mut receiver = ReceivePipeline::new(
            ReceiveConfig {
                limits,
                bindings: MediaBindings::new(1, 2, 3, 4).unwrap(),
                epoch: MediaEpoch {
                    configuration: CodecConfigurationGeneration::INITIAL,
                    recovery: RecoveryGeneration::INITIAL,
                },
                policy: ReceivePolicy::default(),
            },
            MediaBudget::new(&LIMITS).unwrap(),
        )
        .unwrap();
        let now = network::clock(&cx);
        receiver.decoder_configured(now).unwrap();
        let mut view = ViewTracker::new(&receiver, sample, 300_000, now).unwrap();
        assert!(matches!(view.evidence(now), Err(ViewError::NotSubmitted)));
        let captured = host_now(&hc).unwrap().as_micros();
        let mut buf = [0; 1150];
        let size = encode_recovery(
            RecoveryChunk {
                frame: 0,
                total_bytes: 4,
                offset: 0,
                capture_micros: captured,
                bytes: b"data",
            },
            2,
            &limits,
            &mut buf,
        )
        .unwrap();
        let now = network::clock(&cx);
        receiver
            .receive(Channel::Recovery, &buf[..size], now)
            .unwrap();
        let picture = receiver.take_decodable(now).unwrap().unwrap();
        let descriptor = picture.descriptor();
        let size = encode_progress(
            Progress {
                descriptor,
                observed_micros: captured,
                observation: SourceObservation::Captured,
                pipeline: PipelineState::Running,
            },
            3,
            &limits,
            &mut buf,
        )
        .unwrap();
        view.progress(&buf[..size], &limits, now).unwrap();
        let completion = receiver.complete_decode(&picture, now).unwrap();
        view.decoded(completion, true, now).unwrap();
        assert!(matches!(view.evidence(now), Err(ViewError::NotSubmitted)));
        assert_eq!(
            view.visible(0, now).unwrap().source_age_upper_us,
            sample.age_upper_us(captured, now).unwrap()
        );
        receiver.close();
        assert!(view.evidence(network::clock(&cx)).is_err());
    });
}

#[test]
fn peer_fin_and_reset_stop_sampling_before_receiving_another_record() {
    for reset in [false, true] {
        let rt = network::runtime();
        let cx = rt.request_cx_with_budget(Budget::INFINITE);
        let hc = rt.request_cx_with_budget(Budget::INFINITE);
        rt.block_on(async {
            let (mut peer, mut s, r) = native(&cx).await;
            let control = approved(hc);
            let mut host =
                ClockSync::host(control.clone(), &mut s, r, binding(), &selection()).unwrap();
            if reset {
                peer.connection_mut()
                    .reset_stream(&cx, r.inbound.stream, 0)
                    .unwrap();
            } else {
                peer.connection_mut()
                    .write_stream(&cx, r.inbound.stream, asupersync::bytes::Bytes::new(), true)
                    .unwrap();
            }
            let deadline = network::clock(&cx) + 500_000;
            loop {
                assert!(network::clock(&cx) < deadline);
                let ((), result) = Box::pin(network::both(
                    async {
                        peer.flush(&cx).await.unwrap();
                        let _ = peer.drive_io_once(&cx, Duration::from_millis(1)).await;
                    },
                    host.drive(&mut s, Duration::from_millis(1)),
                ))
                .await;
                if let Err(e) = result {
                    assert_eq!(e, Error::PeerClosed);
                    break;
                }
            }
            assert!(s.is_closed());
            assert!(control.check().is_err());
        });
    }
}
#[test]
fn duplicate_probe_cannot_replace_an_unsent_reply() {
    let rt = network::runtime();
    let cx = rt.request_cx_with_budget(Budget::INFINITE);
    let hc = rt.request_cx_with_budget(Budget::INFINITE);
    rt.block_on(async {
        let (mut c, mut s, r) = pair(&cx).await;
        let control = approved(hc);
        let mut host =
            ClockSync::host(control.clone(), &mut s, r, binding(), &selection()).unwrap();
        send(
            &cx,
            &mut c,
            reverse(r).outbound,
            &record(Message::Probe { sequence: 1 }),
        );
        let deadline = network::clock(&cx) + 800_000;
        loop {
            assert!(network::clock(&cx) < deadline);
            raw_drive(&cx, &mut c, &mut s).await;
            if host.receive(&mut s, blocked).unwrap() > 0 {
                break;
            }
        }
        while c.usage().critical_send_records > 0 {
            raw_drive(&cx, &mut c, &mut s).await;
        }
        send(
            &cx,
            &mut c,
            reverse(r).outbound,
            &record(Message::Probe { sequence: 1 }),
        );
        for _ in 0..3 {
            raw_drive(&cx, &mut c, &mut s).await;
            assert_eq!(
                host.receive(&mut s, blocked),
                Ok(0),
                "second probe must not replace pending sample"
            );
        }
        assert_eq!(host.service(&mut s), Ok(Event::ReplyQueued));
        // Once the first reply is staged, the duplicate is rejected rather
        // than being sampled again or resetting the sequence space.
        assert_eq!(host.receive(&mut s, blocked), Err(Error::Sequence));
        assert!(control.check().is_err());
        assert!(s.is_closed());
    });
}

#[test]
fn host_reply_backpressure_preserves_the_original_sample() {
    let rt = network::runtime();
    let cx = rt.request_cx_with_budget(Budget::INFINITE);
    let vc = rt.request_cx_with_budget(Budget::INFINITE);
    let hc = rt.request_cx_with_budget(Budget::INFINITE);
    rt.block_on(async {
        let (mut client, mut server, routes) = pair(&cx).await;
        let mut host =
            ClockSync::host(approved(hc), &mut server, routes, binding(), &selection()).unwrap();
        let mut viewer = ClockSync::viewer(
            vc,
            &mut client,
            reverse(routes),
            binding(),
            &selection(),
            ClockPolicy::default(),
        )
        .unwrap();
        viewer.service(&mut client).unwrap();
        let deadline = network::clock(&cx) + 900_000;
        loop {
            assert!(network::clock(&cx) < deadline);
            drive(&mut client, &mut server, &mut viewer, &mut host).await;
            if host.receive(&mut server, blocked).unwrap() > 0 {
                break;
            }
        }
        let before_backpressure = network::clock(&cx);
        let filler = record(Message::Reply {
            sequence: 999,
            host_sample_us: 0,
        });
        send(&cx, &mut server, routes.outbound, &filler);
        assert_eq!(host.service(&mut server), Ok(Event::Backpressure));
        std::thread::sleep(Duration::from_millis(70));
        assert_eq!(host.service(&mut server), Ok(Event::Backpressure));
        let mut filler_read = false;
        let mut host_sample = None;
        let mut sent = false;
        loop {
            assert!(network::clock(&cx) < deadline);
            raw_drive(&cx, &mut client, &mut server).await;
            client
                .receive(
                    &cx,
                    || true,
                    |_, bytes| {
                        match clock::decode(
                            bytes,
                            binding(),
                            &LIMITS,
                            InputDirection::HostToViewer,
                            InputDelivery::Reliable,
                        )
                        .unwrap()
                        {
                            Message::Reply { sequence: 999, .. } => {
                                assert_eq!(bytes, filler);
                                filler_read = true;
                            }
                            Message::Reply {
                                sequence: 1,
                                host_sample_us,
                            } => host_sample = Some(host_sample_us),
                            _ => panic!("unexpected clock record"),
                        }
                        Ok(Disposition::Consumed)
                    },
                )
                .unwrap();
            if !sent {
                sent = host.service(&mut server).unwrap() == Event::ReplyQueued;
            }
            if let Some(sample) = host_sample {
                assert!(
                    sample <= before_backpressure,
                    "backpressure resampled the host timestamp"
                );
                assert!(network::clock(&cx) - sample >= 70_000);
                break;
            }
        }
        assert!(sent && filler_read);
        // The raw receiver above deliberately inspected bytes instead of handing
        // them to the viewer. That cannot publish a client correlation.
        assert!(viewer.correlation(&mut client).unwrap().is_none());
    });
}

fn responder(cx: &Cx) -> fr_client::authority::ObservationResponder {
    fr_client::authority::ObservationResponder::new(
        fr_wire::authority::Binding {
            channel: binding().id,
            session: binding().remote_session,
        },
        LIMITS,
        fr_client::input::ClientInstant(network::clock(cx)),
    )
    .unwrap()
}

#[test]
fn renewal_and_clock_sampling_share_control_streams_past_initial_authorization() {
    let rt = network::runtime();
    let cx = rt.request_cx_with_budget(Budget::INFINITE);
    let vc = rt.request_cx_with_budget(Budget::INFINITE);
    let hc = rt.request_cx_with_budget(Budget::INFINITE);
    rt.block_on(async {
        let (mut client, mut server, routes) = pair(&cx).await;
        let control = approved(hc);
        let initial = control
            .deadline(Duration::from_secs(3))
            .unwrap()
            .time()
            .as_nanos()
            / 1000;
        let mut host = ClockSync::host(
            control.clone(),
            &mut server,
            routes,
            binding(),
            &selection(),
        )
        .unwrap();
        let mut renewal =
            ObservationRenewal::new(control.clone(), &server, routes, LIMITS).unwrap();
        let mut viewer = ClockSync::viewer(
            vc,
            &mut client,
            reverse(routes),
            binding(),
            &selection(),
            ClockPolicy {
                valid_for_us: 1_000_000,
                ..ClockPolicy::default()
            },
        )
        .unwrap();
        let mut responder = responder(&cx);
        let mut nonce = 0;
        let mut response_until = None;
        let mut last_sample = None;
        let mut samples = 0;
        let end = initial + 150_000;
        while network::clock(&cx) < end {
            viewer.service(&mut client).unwrap();
            host.service(&mut server).unwrap();
            renewal
                .service(&mut server, || {
                    nonce += 1;
                    Ok(nonce)
                })
                .unwrap();
            drive(&mut client, &mut server, &mut viewer, &mut host).await;
            host.receive(&mut server, blocked).unwrap();
            renewal.receive(&mut server, blocked).unwrap();
            viewer
                .receive(&mut client, |route, bytes| {
                    assert_eq!(route, Route::Stream(reverse(routes).inbound));
                    let current = network::clock(&cx);
                    match responder.accept(bytes, ClientInstant(current)) {
                        Ok(()) => {
                            response_until = Some(current + 1_000_000);
                            Ok(Disposition::Consumed)
                        }
                        Err(fr_client::authority::Error::Backpressure) => Ok(Disposition::Blocked),
                        Err(e) => panic!("unexpected authority response refusal: {e:?}"),
                    }
                })
                .unwrap();
            let current = network::clock(&cx);
            if let Some(bytes) = responder.pending(ClientInstant(current)).unwrap() {
                match client.send(
                    &cx,
                    Route::Stream(reverse(routes).outbound),
                    bytes,
                    response_until.unwrap(),
                    || true,
                ) {
                    Ok(()) => {
                        responder.sent(ClientInstant(network::clock(&cx))).unwrap();
                        response_until = None;
                    }
                    Err(fr_transport::quic::Error::Backpressure) => {}
                    Err(e) => panic!("response send failed: {e:?}"),
                }
            }
            if let Some(sample) = viewer.correlation(&mut client).unwrap()
                && last_sample != Some(sample.received_at_us())
            {
                samples += 1;
                last_sample = Some(sample.received_at_us());
            }
        }
        assert!(control.check().is_ok());
        assert!(renewal.renewed_until().unwrap().as_micros() > initial);
        assert!(samples >= 5);
        assert!(nonce >= 4);
        // Revoking observation invalidates the clock endpoint before any new
        // response can be generated, even though the socket itself was healthy.
        control.revoke();
        assert!(host.service(&mut server).is_err());
        assert!(server.is_closed());
    });
}
