//! The same network function used by `StreamingViewer::serve`, with actual
//! session renewal and TLS/UDP. Decoder completion remains explicitly simulated.
use super::*;
use crate::session_startup::running::HostSession;
use fr_media::delivery::ReceiveState;
use fr_wire::{Kind, recovery_request};

type Ready = (
    HostSession,
    Peer,
    ReceivePipeline,
    recovery_control::Receiver,
    NegotiatedMedia,
);
async fn pair(c: &Cx, h: &Cx) -> Ready {
    let mut caps = capabilities();
    caps.push(negotiation::Capability {
        name: recovery_request::CAPABILITY.into(),
        version: recovery_request::VERSION,
        required: true,
    });
    caps.push(negotiation::Capability {
        name: receiver_metrics::CAPABILITY.into(),
        version: receiver_metrics::VERSION,
        required: true,
    });
    caps.sort_by(|a, b| a.name.cmp(&b.name));
    let (mut host, mut viewer) = pair_initialized(c, h, caps, |_| {}).await;
    let (hc, vc) = attach(
        &mut host,
        &mut viewer,
        c,
        h,
        attachment::MediaRole::Configuration,
        18,
    )
    .await;
    let (hr, vr) = attach(
        &mut host,
        &mut viewer,
        c,
        h,
        attachment::MediaRole::Recovery,
        19,
    )
    .await;
    let (hv, vv) = attach(
        &mut host,
        &mut viewer,
        c,
        h,
        attachment::MediaRole::Video,
        20,
    )
    .await;
    let selection = host.selection().clone();
    let hm = NegotiatedMedia::new(host.io().unwrap().0, &selection, &hc, &hr, &hv).unwrap();
    let vm = NegotiatedMedia::new(viewer.io().unwrap().0, &selection, &vc, &vr, &vv).unwrap();
    let cfg = vm
        .receiver_config(
            viewer.io().unwrap().0,
            ReceivePolicy {
                reference_budget_micros: 120_000,
                recovery_budget_micros: 5_000_000,
                ..ReceivePolicy::default()
            },
        )
        .unwrap();
    let receiver = running(cfg, now(c).unwrap());
    let parent = viewer.metadata().binding;
    let (q, routes) = viewer.io().unwrap();
    let watcher = vm.recovery_receiver(q, routes, parent, &receiver).unwrap();
    (
        host,
        Peer::Observe {
            session: viewer,
            media: vm,
        },
        receiver,
        watcher,
        hm,
    )
}

#[test]
#[allow(clippy::too_many_lines)]
fn normal_network_loop_reports_lost_reference_and_keeps_original_observation_renewing() {
    run(|c, h| async move {
        let (mut host, mut peer, mut receiver, mut watcher, media) = Box::pin(pair(&c, &h)).await;
        let (mut hf, mut vf) = telemetry(&mut host, &mut peer);
        let mut bound = media.binding();
        bound.parent = host.binding();
        let (cfg, descriptor, route, initial, loss) =
            announce_loss(&mut host, &mut peer, &media, &h);
        let mut announced = false;
        let control_in = Route::Stream(host.io().unwrap().1.inbound);
        let repair_in = Route::Stream(media.repair_stream(host.io().unwrap().0).unwrap());
        let mut repairs = Repair::default();
        let mut statistics = Statistics::default();
        let mut nonce = 8000_u128;
        let mut reports = 0;
        let mut original = None;
        let mut obsolete_sent = false;
        let end = initial + 3_250_000;
        while now(&c).unwrap() < end {
            announced = announced || announce(&h, &mut host, route, &loss, initial);
            hf.service(host.io().unwrap().0, &h, now(&h).unwrap())
                .unwrap();
            let (a, b) = Box::pin(support::both(
                host.drive(
                    Duration::from_millis(1),
                    || {
                        nonce += 1;
                        Ok(nonce)
                    },
                    |r, bytes| {
                        if feedback::is_feedback(bytes) {
                            hf.receive(r, bytes, now(&h).unwrap()).unwrap();
                            return Ok(Disposition::Consumed);
                        }
                        if r == control_in
                            && bytes.get(6..8)
                                == Some(&(Kind::RecoveryRequest as u16).to_be_bytes())
                        {
                            let message = recovery_request::decode(
                                bytes,
                                bound,
                                &ProtocolLimits::ABSOLUTE,
                                fr_wire::input::InputDirection::ViewerToHost,
                                fr_wire::input::InputDelivery::Reliable,
                            )
                            .unwrap();
                            assert_eq!(message.reason, recovery_request::Reason::ReferenceExpired);
                            assert_eq!(message.last_useful_frame, None);
                            reports += 1;
                            return Ok(Disposition::Consumed);
                        }
                        // A genuine repair request cannot recover this deliberately
                        // dropped picture. Consuming it permits ordinary ACK progress.
                        if r == repair_in {
                            return Ok(Disposition::Consumed);
                        }
                        block(r, bytes)
                    },
                ),
                network(
                    &mut peer,
                    None,
                    None,
                    &mut receiver,
                    &mut repairs,
                    Some(&mut watcher),
                    &mut statistics,
                    Some(&mut vf),
                    None,
                    None,
                    None,
                    &c,
                    &mut |_| {},
                    &mut block,
                ),
            ))
            .await;
            a.unwrap();
            b.unwrap();
            if let Some(until) = watcher.next_deadline() {
                assert_eq!(*original.get_or_insert(until), until);
                assert_eq!(receiver.state(), ReceiveState::NeedsRecovery);
                assert_eq!(receiver.budget_usage(), BudgetUsage::default());
                assert_eq!(repairs.len, 0);
                if !obsolete_sent {
                    send_obsolete(&h, &mut host, cfg, descriptor, route);
                    obsolete_sent = true;
                }
            }
        }
        assert!(announced);
        assert_eq!(reports, 1);
        assert!(hf.accepted > 15);
        assert!(vf.sent >= hf.accepted);
        assert!(obsolete_sent);
        assert_eq!(watcher.state(), recovery_control::State::Requested);
        assert!(host.renewed_until().unwrap().as_micros() > initial + 3_000_000);
        assert!(statistics.network_turns > 100);
        assert!(statistics.repair_requests > 0);
        assert_eq!(statistics.decoded, 0);
        host.close();
        peer.close();
        receiver.close();
        watcher.close();
    });
}

#[test]
fn malformed_receiver_remains_terminal_without_emitting_a_recovery_record() {
    run(|c, h| async move {
        let (mut host, mut peer, mut receiver, mut watcher, _) = Box::pin(pair(&c, &h)).await;
        assert!(
            receiver
                .receive(Channel::Video, b"not a media record", now(&c).unwrap())
                .is_err()
        );
        let result = network(
            &mut peer,
            None,
            None,
            &mut receiver,
            &mut Repair::default(),
            Some(&mut watcher),
            &mut Statistics::default(),
            None,
            None,
            None,
            None,
            &c,
            &mut |_| {},
            &mut block,
        )
        .await;
        assert!(matches!(
            result,
            Err(Error::Recovery(recovery_control::Error::Delivery(
                DeliveryError::Wire(_)
            )))
        ));
        assert_eq!(watcher.state(), recovery_control::State::Closed);
        assert_eq!(watcher.next_deadline(), None);
        host.close();
        peer.close();
        receiver.close();
    });
}

#[test]
fn omitted_recovery_owner_preserves_the_original_terminal_failure() {
    run(|c, h| async move {
        let (mut host, mut peer, mut receiver, mut watcher, _) = Box::pin(pair(&c, &h)).await;
        let (session, media) = peer.parts().unwrap();
        let cfg = media
            .receiver_config(&session.transport, ReceivePolicy::default())
            .unwrap();
        let stamp = now(&c).unwrap();
        let (_, bytes) = progress(cfg, stamp);
        receiver
            .receive(Channel::MediaConfig, &bytes, stamp)
            .unwrap();
        asupersync::time::sleep(c.now(), Duration::from_millis(130)).await;
        assert_eq!(
            network(
                &mut peer,
                None,
                None,
                &mut receiver,
                &mut Repair::default(),
                None,
                &mut Statistics::default(),
                None,
                None,
                None,
                None,
                &c,
                &mut |_| {},
                &mut block
            )
            .await,
            Err(Error::Delivery(DeliveryError::ReferenceExpired))
        );
        assert_eq!(watcher.state(), recovery_control::State::Receiving);
        assert_eq!(watcher.next_deadline(), None);
        host.close();
        peer.close();
        receiver.close();
        watcher.close();
    });
}

fn send_obsolete(
    h: &Cx,
    host: &mut HostSession,
    cfg: ReceiveConfig,
    descriptor: FrameDescriptor,
    route: Route,
) {
    // These bytes cross the real reliable lane, not a direct receiver call.
    let mut old = descriptor;
    old.frame = 2;
    old.reference = Some(1);
    let mut bytes = [0; 1150];
    let n = encode_progress(
        Progress {
            descriptor: old,
            observed_micros: old.capture_micros,
            observation: SourceObservation::Captured,
            pipeline: PipelineState::Running,
        },
        cfg.bindings.for_channel(Channel::MediaConfig),
        &cfg.limits,
        &mut bytes,
    )
    .unwrap();
    host.io()
        .unwrap()
        .0
        .send(h, route, &bytes[..n], now(h).unwrap() + 1_000_000, || true)
        .unwrap();
}

fn telemetry(host: &mut HostSession, peer: &mut Peer) -> (feedback::HostFeedback, ViewerFeedback) {
    let (session, media) = peer.parts().unwrap();
    let setup = Setup::selected(host.selection(), host.binding(), media.binding())
        .unwrap()
        .unwrap();
    let hr = host.io().unwrap().1;
    let cr = session.routes;
    (
        feedback::HostFeedback::new(setup, Route::Stream(hr.inbound), Route::Stream(hr.outbound))
            .unwrap(),
        ViewerFeedback::new(setup, Route::Stream(cr.inbound), Route::Stream(cr.outbound)).unwrap(),
    )
}

/// The progress record announcing frame 1, whose picture is never sent. The
/// caller sends it with `announce` from its drive loop, so earlier writes can
/// drain on a loaded host instead of failing the first send.
fn announce_loss(
    host: &mut HostSession,
    peer: &mut Peer,
    media: &NegotiatedMedia,
    h: &Cx,
) -> (ReceiveConfig, FrameDescriptor, Route, u64, Vec<u8>) {
    let initial = now(h).unwrap();
    let (session, vm) = peer.parts().unwrap();
    let cfg = vm
        .receiver_config(&session.transport, ReceivePolicy::default())
        .unwrap();
    let (descriptor, bytes) = progress(cfg, initial);
    let route = Route::Stream(media.progress_for_test(host.io().unwrap().0));
    (cfg, descriptor, route, initial, bytes)
}

/// True once the announcement is admitted; false on Backpressure, which
/// admitted no bytes and asks for the same record again.
fn announce(h: &Cx, host: &mut HostSession, route: Route, bytes: &[u8], initial: u64) -> bool {
    match host
        .io()
        .unwrap()
        .0
        .send(h, route, bytes, initial + 2_000_000, || true)
    {
        Ok(()) => true,
        Err(quic::Error::Backpressure) => false,
        Err(error) => panic!("loss announcement refused: {error:?}"),
    }
}

mod completion;

#[cfg(target_os = "linux")]
mod native_drain;

mod selection;
