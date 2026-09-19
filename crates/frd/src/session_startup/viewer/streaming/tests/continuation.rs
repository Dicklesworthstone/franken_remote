//! Actual running viewer, TLS/UDP, child IPC and deadline/generation owners.
//! Host records and child decoded pixels are test fixtures, not HEVC evidence.
use super::{attach, capabilities, run, support};
use crate::{
    media::{Presenter, decoder_startup},
    media_quic::NegotiatedMedia,
    session_startup::viewer::streaming::StreamingViewer,
};
use asupersync::cx::Cx;
use fr_media::{
    delivery::{DeliveryMode, ReceivePipeline, ReceivePolicy, SendCache, SendPolicy},
    hevc::{HevcGuard, MAX_DECODER_RECORD_BYTES},
};
use fr_transport::quic::{Disposition, Route};
use fr_wire::{
    Channel, FrameDescriptor, PipelineState, Progress, SourceObservation,
    attachment::Ticket,
    decoder::{self, Configuration, Message},
    input::{InputDelivery, InputDirection},
    negotiation::{Capability, Role},
    recovery_request,
};
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    time::{Duration, Instant},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Scenario {
    Success,
    SlowHost,
    NoOffer,
    BadConfiguration,
    Cancel,
}

fn configuration(media: &NegotiatedMedia) -> Vec<u8> {
    // Exactly the canonical parameter sets used by Presenter::stream_fixture.
    let cfg = crate::media::presentation::tests::configuration();
    let mut guard = HevcGuard::new(cfg.codec().unwrap(), *media.limits().protocol(), 4).unwrap();
    let mut au = Vec::new();
    for nal in [
        "40010c01ffff01600000030090000003000003003cba0240",
        "42010101600000030090000003000003003ca00a080f165ba4a4c2f016a020202080000003008000000f04",
        "4401c0718112",
        "2801ade06702f86753c11ead2f1f6a69",
    ] {
        au.extend_from_slice(&[0, 0, 0, 1]);
        for hex in nal.as_bytes().as_chunks::<2>().0 {
            au.push(u8::from_str_radix(std::str::from_utf8(hex).unwrap(), 16).unwrap());
        }
    }
    guard.validate_annex_b(&au, true).unwrap();
    let record = guard.decoder_record().unwrap();
    let codec = record.codec().replacen("hvc1.", "hev1.", 1);
    let value = Configuration {
        coded_width: 320,
        coded_height: 240,
        crop_width: 320,
        crop_height: 240,
        fps: 30,
        primaries: 1,
        transfer: 1,
        matrix: 1,
        full_range: false,
        decoded_pictures: 4,
        codec: &codec,
        hvcc: record.bytes(),
    };
    let limits = media.limits();
    let mut out = vec![0; MAX_DECODER_RECORD_BYTES + 1024];
    let n = decoder::encode(
        Message::Configuration(value),
        media.binding(),
        limits.protocol(),
        &mut out,
        InputDirection::HostToViewer,
        InputDelivery::Reliable,
    )
    .unwrap();
    out.truncate(n);
    out
}
fn stamp() -> u64 {
    crate::media::host_now(&Cx::current().unwrap())
        .unwrap()
        .as_micros()
}
fn put(cache: &mut SendCache, frame: u64, reference: Option<u64>, mode: DeliveryMode, now: u64) {
    cache
        .push(
            Progress {
                descriptor: FrameDescriptor {
                    frame,
                    reference,
                    total_bytes: 4,
                    stride: 1077,
                    capture_micros: now,
                },
                observed_micros: now,
                observation: SourceObservation::Captured,
                pipeline: PipelineState::Running,
            },
            vec![1, 2, 3, 4],
            mode,
            now,
        )
        .unwrap();
}
fn send_cache(
    host: &mut crate::session_startup::running::HostSession,
    media: &NegotiatedMedia,
    cache: &mut SendCache,
) {
    let mut bytes = [0; 1150];
    while let Some(packet) = cache.next_packet(stamp(), &mut bytes).unwrap() {
        cache.authorize_write(&packet, stamp()).unwrap();
        host.io()
            .unwrap()
            .0
            .send(
                &Cx::current().unwrap(),
                media.packet_route_for_test(packet.channel()),
                &bytes[..packet.byte_len()],
                packet.send_by_micros(),
                || true,
            )
            .unwrap();
    }
}
async fn wait_ack(
    host: &mut crate::session_startup::running::HostSession,
    media: &NegotiatedMedia,
    first: bool,
) {
    let (_, route) = media.decoder_routes_for_test();
    let cx = Cx::current().unwrap();
    let until = Instant::now() + Duration::from_secs(2);
    loop {
        let mut found = false;
        host.io()
            .unwrap()
            .0
            .receive_ready(
                &cx,
                || true,
                |r| r == Route::Stream(route),
                |_, bytes| {
                    let reply = decoder::decode(
                        bytes,
                        media.binding(),
                        media.limits().protocol(),
                        InputDirection::ViewerToHost,
                        InputDelivery::Reliable,
                    )
                    .unwrap();
                    if first {
                        assert!(matches!(reply, Message::FirstDecoded { frame: 2, .. }));
                    } else {
                        assert_eq!(reply, Message::Configured);
                    }
                    assert!(!found);
                    found = true;
                    Ok(Disposition::Consumed)
                },
            )
            .unwrap();
        if found {
            return;
        }
        assert!(Instant::now() < until, "missing decoder acknowledgement");
        host.drive(
            Duration::from_millis(5),
            || Ok(u128::from(stamp()) + 8000),
            |_, _| Ok(Disposition::Blocked),
        )
        .await
        .unwrap();
    }
}

#[allow(clippy::too_many_lines)]
fn exercise(scenario: Scenario) {
    run(|c, h| async move {
        let cleanup = Cx::current().unwrap();
        let mut caps = capabilities();
        caps.push(Capability {
            name: recovery_request::CAPABILITY.into(),
            version: recovery_request::VERSION,
            required: true,
        });
        caps.push(Capability {
            name: fr_wire::receiver_metrics::CAPABILITY.into(),
            version: fr_wire::receiver_metrics::VERSION,
            required: true,
        });
        let (mut host, mut viewer) = observing_pair(&c, &h, caps).await;
        let (hm, vm) = media_pair(&mut host, &mut viewer, &c, &h).await;
        let original_connection = host.io().unwrap().0.binding();
        let budget = if scenario == Scenario::NoOffer {
            250_000
        } else {
            5_000_000
        };
        let config = vm
            .receiver_config(
                &viewer.transport,
                ReceivePolicy {
                    reference_budget_micros: 120_000,
                    recovery_budget_micros: budget,
                    ..ReceivePolicy::default()
                },
            )
            .unwrap();
        let mut receiver = ReceivePipeline::new(
            config,
            fr_media::delivery::MediaBudget::new(config.limits.protocol()).unwrap(),
        )
        .unwrap();
        let mut presenter =
            Presenter::stream_fixture(&c, &viewer.transport, &vm, &mut receiver).await;
        let worker = presenter.worker_id();
        let mut cache = SendCache::new(
            config.limits,
            config.bindings,
            config.epoch,
            SendPolicy {
                recovery_horizon_micros: budget,
                ..SendPolicy::default()
            },
        )
        .unwrap();
        put(&mut cache, 0, None, DeliveryMode::Recovery, stamp());
        let mut bytes = [0; 1150];
        while let Some(packet) = cache.next_packet(stamp(), &mut bytes).unwrap() {
            receiver
                .receive(packet.channel(), &bytes[..packet.byte_len()], stamp())
                .unwrap();
        }
        let initial = presenter
            .present_next(&c, &mut receiver)
            .await
            .unwrap()
            .unwrap();
        let first_frame = initial.frame.as_raw();
        let mut running =
            StreamingViewer::from_test_parts(viewer, vm, presenter, receiver, initial);
        let stop = running.control();
        let reported = Rc::new(Cell::new(false));
        let done = Rc::new(Cell::new(false));
        let reports = Rc::new(Cell::new(0u32));
        let frames = Rc::new(RefCell::new(vec![first_frame]));
        let client_done = done.clone();
        let report_seen = reported.clone();
        let visible = frames.clone();
        let client = async {
            let result = running
                .serve(
                    |_, event| {
                        if let Some(event) = event {
                            visible.borrow_mut().push(event.frame.as_raw());
                            if event.frame.as_raw() == 3 {
                                stop.stop();
                            }
                        }
                        if scenario == Scenario::Cancel && report_seen.get() {
                            stop.stop();
                        }
                        Ok(())
                    },
                    |_| {},
                    |_, _| Ok(Disposition::Blocked),
                )
                .await;
            client_done.set(true);
            result
        };
        let server = async {
            let mut hm = hm;
            let parent = host.binding();
            let control_inbound = host.io().unwrap().1.inbound;
            let mut admitted = hm.binding();
            admitted.parent = parent;
            let original_lease = host.renewed_until().unwrap();
            let repair_route = Route::Stream(hm.repair_stream(host.io().unwrap().0).unwrap());
            let old_limits = hm.limits();
            let mut repairs = 0_u32;
            put(&mut cache, 1, Some(0), DeliveryMode::Datagrams, stamp());
            // Announce the real dependency; deliberately drop EVERY video fragment.
            let mut out = [0; 1150];
            while let Some(packet) = cache.next_packet(stamp(), &mut out).unwrap() {
                if packet.channel() == Channel::MediaConfig {
                    host.io()
                        .unwrap()
                        .0
                        .send(
                            &h,
                            hm.packet_route_for_test(packet.channel()),
                            &out[..packet.byte_len()],
                            packet.send_by_micros(),
                            || true,
                        )
                        .unwrap();
                }
            }
            let mut request_wire = None;
            let request_started = Instant::now();
            loop {
                host.drive(
                    Duration::from_millis(5),
                    || Ok(u128::from(stamp()) + 10000),
                    |route, bytes| {
                        if route == repair_route {
                            let Route::Stream(route) = route else {
                                unreachable!()
                            };
                            let record = fr_wire::Record::decode(
                                bytes,
                                &old_limits,
                                route.binding,
                                Channel::Control,
                            )
                            .unwrap();
                            fr_wire::decode_repair(record, old_limits.max_fragments(), &old_limits)
                                .unwrap();
                            repairs += 1;
                            return Ok(Disposition::Consumed);
                        }
                        if bytes.get(6..8)
                            == Some(&(fr_wire::Kind::RecoveryRequest as u16).to_be_bytes())
                        {
                            assert_eq!(route, Route::Stream(control_inbound));
                            recovery_request::decode(
                                bytes,
                                admitted,
                                config.limits.protocol(),
                                InputDirection::ViewerToHost,
                                InputDelivery::Reliable,
                            )
                            .unwrap();
                            reports.set(reports.get() + 1);
                            request_wire = Some(bytes.to_vec());
                            Ok(Disposition::Consumed)
                        } else {
                            Ok(Disposition::Blocked)
                        }
                    },
                )
                .await
                .unwrap();
                if request_wire.is_some() {
                    break;
                }
                assert!(request_started.elapsed() < Duration::from_secs(2));
            }
            assert!(
                repairs > 0,
                "selective repair must be serviced before replacing the failed chain"
            );
            let request = request_wire.unwrap();
            let demand = cache.request_recovery(&request, admitted, stamp()).unwrap();
            assert!(matches!(
                demand,
                fr_media::delivery::RecoveryDisposition::Accepted(_)
            ));
            reported.set(true);
            let failed_at = Instant::now();
            if matches!(scenario, Scenario::NoOffer | Scenario::Cancel) {
                while !done.get() {
                    if host
                        .drive(
                            Duration::from_millis(5),
                            || Ok(u128::from(stamp()) + 11000),
                            |route, _| {
                                Ok(if route == repair_route {
                                    Disposition::Consumed
                                } else {
                                    Disposition::Blocked
                                })
                            },
                        )
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                assert!(failed_at.elapsed() < Duration::from_secs(1));
                return;
            }
            if scenario == Scenario::SlowHost {
                while failed_at.elapsed() < Duration::from_millis(3200) {
                    // Regression: the viewer MUST NOT reset these before our offer.
                    hm.check(host.io().unwrap().0).unwrap();
                    host.drive(
                        Duration::from_millis(5),
                        || Ok(u128::from(stamp()) + 12000),
                        |route, _| {
                            Ok(if route == repair_route {
                                Disposition::Consumed
                            } else {
                                Disposition::Blocked
                            })
                        },
                    )
                    .await
                    .unwrap();
                }
                assert!(host.renewed_until().unwrap() > original_lease);
            }
            let routes = host.io().unwrap().1;
            let until = cache.next_deadline().unwrap();
            let mut replacement = hm
                .begin_replacement(
                    &h,
                    host.io().unwrap().0,
                    routes,
                    parent,
                    until,
                    Some([Ticket(51), Ticket(52), Ticket(53)]),
                    || true,
                )
                .unwrap();
            loop {
                if replacement
                    .advance(&h, host.io().unwrap().0, || true)
                    .unwrap()
                {
                    break;
                }
                host.drive(
                    Duration::from_millis(5),
                    || Ok(u128::from(stamp()) + 13000),
                    |_, _| Ok(Disposition::Blocked),
                )
                .await
                .unwrap();
            }
            hm = replacement.finish(&h, host.io().unwrap().0).unwrap();
            assert!(host.io().unwrap().0.is_bound_to(&original_connection));
            let mut record = configuration(&hm);
            if scenario == Scenario::BadConfiguration {
                // The valid hvcC is unchanged, but a new fps is not the original decoder setup.
                let Message::Configuration(value) = decoder::decode(
                    &record,
                    hm.binding(),
                    config.limits.protocol(),
                    InputDirection::HostToViewer,
                    InputDelivery::Reliable,
                )
                .unwrap() else {
                    unreachable!()
                };
                let mut altered = vec![0; record.len()];
                decoder::encode(
                    Message::Configuration(Configuration { fps: 29, ..value }),
                    hm.binding(),
                    config.limits.protocol(),
                    &mut altered,
                    InputDirection::HostToViewer,
                    InputDelivery::Reliable,
                )
                .unwrap();
                record = altered;
            }
            let (configuration_route, _) = hm.decoder_routes_for_test();
            host.io()
                .unwrap()
                .0
                .send(
                    &h,
                    Route::Stream(configuration_route),
                    &record,
                    until,
                    || true,
                )
                .unwrap();
            if scenario == Scenario::BadConfiguration {
                while !done.get() {
                    if host
                        .drive(
                            Duration::from_millis(5),
                            || Ok(u128::from(stamp()) + 14000),
                            |route, _| {
                                Ok(if route == repair_route {
                                    Disposition::Consumed
                                } else {
                                    Disposition::Blocked
                                })
                            },
                        )
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                return;
            }
            wait_ack(&mut host, &hm, false).await;
            cache
                .replace(
                    fr_media::delivery::MediaEpoch {
                        configuration: hm.binding().configuration,
                        recovery: hm.binding().recovery,
                    },
                    hm.bindings(),
                    stamp(),
                )
                .unwrap();
            put(&mut cache, 2, None, DeliveryMode::Recovery, stamp());
            send_cache(&mut host, &hm, &mut cache);
            wait_ack(&mut host, &hm, true).await;
            put(&mut cache, 3, Some(2), DeliveryMode::Datagrams, stamp());
            send_cache(&mut host, &hm, &mut cache);
            while !done.get() {
                if host
                    .drive(
                        Duration::from_millis(5),
                        || Ok(u128::from(stamp()) + 15000),
                        |route, _| {
                            Ok(if route == repair_route {
                                Disposition::Consumed
                            } else {
                                Disposition::Blocked
                            })
                        },
                    )
                    .await
                    .is_err()
                {
                    break;
                }
                assert!(failed_at.elapsed() < Duration::from_secs(6));
            }
        };
        let ((), result) = Box::pin(support::both(server, client)).await;
        assert!(
            result.is_err(),
            "serve exits only after stop or typed refusal"
        );
        assert_eq!(reports.get(), 1);
        assert_eq!(running.presenter.worker_id(), worker);
        if matches!(scenario, Scenario::Success | Scenario::SlowHost) {
            assert_eq!(frames.borrow().as_slice(), &[0, 2, 3]);
            assert_eq!(running.statistics().recovered_streams, 1);
            assert_eq!(
                running.receiver.state(),
                fr_media::delivery::ReceiveState::Closed
            );
        } else {
            assert_eq!(frames.borrow().as_slice(), &[0]);
            assert_eq!(running.statistics().recovered_streams, 0);
            if scenario == Scenario::BadConfiguration {
                assert_eq!(
                    result,
                    Err(super::Error::Startup(
                        decoder_startup::Error::UnsupportedConfiguration
                    ))
                );
            }
        }
        // Session close does not replace or recycle the original connection proof.

        assert_eq!(
            running.recovery_state(),
            Some(crate::media_quic::recovery::State::Closed)
        );
        running
            .reap_media(
                &cleanup,
                crate::worker::Deadline::after(&cleanup, Duration::from_secs(1)).unwrap(),
            )
            .await
            .unwrap();
    });
}
#[test]
fn running_viewer_recovers_on_the_original_connection_and_decoder() {
    exercise(Scenario::Success);
}
#[test]
fn recovery_wait_keeps_observation_renewing_and_waits_for_the_host_offer() {
    exercise(Scenario::SlowHost);
}
#[test]
fn absent_host_offer_expires_the_original_request_without_restarting_startup() {
    exercise(Scenario::NoOffer);
}
#[test]
fn changed_native_configuration_is_terminal_before_decoder_acknowledgement() {
    exercise(Scenario::BadConfiguration);
}
#[test]
fn local_stop_during_recovery_fences_the_original_session() {
    exercise(Scenario::Cancel);
}

async fn observing_pair(
    c: &Cx,
    h: &Cx,
    mut capabilities: Vec<Capability>,
) -> (
    crate::session_startup::running::HostSession,
    crate::session_startup::ViewerSession,
) {
    use crate::session_startup::{Configuration as Startup, Host, Peer, Viewer};
    use fr_core::{authority::AuthorityPolicy, ids::*, limits::ProtocolLimits};
    use fr_wire::negotiation::{ControlBinding, Offer};
    use std::sync::{Arc, atomic::AtomicBool};
    capabilities.sort_by(|a, b| a.name.cmp(&b.name));
    let cfg = Startup {
        offer: Offer {
            versions: vec![0],
            profile: 1,
            profile_version: 0,
            role: Role::Observe,
            limits: ProtocolLimits::ABSOLUTE,
            capabilities,
        },
        binding: ControlBinding {
            id: 7,
            host_boot: HostBootId::from_raw(11),
            os_session: OsSessionId::from_raw(12),
            remote_session: RemoteSessionId::from_raw(13),
        },
        require_approval: false,
        startup_timeout: Duration::from_secs(2),
        authority: AuthorityPolicy::plan_defaults(),
        transport: fr_transport::quic::Policy::default(),
    };
    let (client, server) = support::native_pair(c, "localhost", fr_transport::quic::ALPN).await;
    let mut viewer = Viewer::new(
        c.clone(),
        client.unwrap(),
        cfg.offer.clone(),
        cfg.transport,
        Duration::from_secs(2),
    )
    .unwrap();
    let peer = Peer::Fixture {
        alive: Arc::new(AtomicBool::new(true)),
        until: crate::session_startup::now(h).unwrap() + 30_000_000,
        control: true,
    };
    let mut host = Host::start(h.clone(), server.unwrap(), peer, cfg).unwrap();
    while !viewer.is_complete() || !host.is_complete() {
        let (a, b) = Box::pin(support::both(
            host.drive(Duration::from_millis(1)),
            viewer.drive(Duration::from_millis(1)),
        ))
        .await;
        a.unwrap();
        b.unwrap();
    }
    (
        host.finish().unwrap().into_running().unwrap(),
        viewer.finish().unwrap(),
    )
}
async fn media_pair(
    host: &mut crate::session_startup::running::HostSession,
    viewer: &mut crate::session_startup::ViewerSession,
    c: &Cx,
    h: &Cx,
) -> (NegotiatedMedia, NegotiatedMedia) {
    use fr_wire::attachment::MediaRole;
    let (hc, vc) = attach(host, viewer, c, h, MediaRole::Configuration, 18).await;
    let (hr, vr) = attach(host, viewer, c, h, MediaRole::Recovery, 19).await;
    let (hv, vv) = attach(host, viewer, c, h, MediaRole::Video, 20).await;
    let selection = host.selection().clone();
    (
        NegotiatedMedia::new(host.io().unwrap().0, &selection, &hc, &hr, &hv).unwrap(),
        NegotiatedMedia::new(viewer.io().unwrap().0, &selection, &vc, &vr, &vv).unwrap(),
    )
}
