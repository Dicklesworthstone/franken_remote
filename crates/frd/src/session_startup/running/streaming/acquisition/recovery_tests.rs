//! Actual authenticated UDP/QUIC attachment replacement and the production
//! grant broker. No codec, visibility, native input, or live-tailnet claim.
use super::*;
use crate::{
    input_quic::grant::CAPABILITY,
    media_quic::NegotiatedMedia,
    session_startup::{
        now,
        running::{
            controlled::tests::attach,
            tests::{pair_initialized, run},
        },
        tests::support,
    },
};
use fr_core::input::{DesktopPoint, InputBounds, InputView};
use fr_core::input_submission::{Capabilities, Capability};
use fr_wire::{
    attachment::{self, MediaRole, Ticket},
    control,
    input::{InputDelivery, InputDirection},
    negotiation::Capability as WireCapability,
};

fn capabilities(recovery: bool) -> Vec<WireCapability> {
    let mut caps: Vec<_> = [
        CAPABILITY,
        fr_wire::decoder::CAPABILITY,
        attachment::CAPABILITY,
        attachment::DELIVERY_CAPABILITY,
        attachment::INPUT_CAPABILITY,
    ]
    .into_iter()
    .map(|name| WireCapability {
        name: name.into(),
        version: 1,
        required: true,
    })
    .collect();
    if recovery {
        caps.push(WireCapability {
            name: fr_wire::recovery_request::CAPABILITY.into(),
            version: fr_wire::recovery_request::VERSION,
            required: true,
        });
    }
    caps.sort_by(|a, b| a.name.cmp(&b.name));
    caps
}
fn request(parent: fr_wire::negotiation::ControlBinding, media: &NegotiatedMedia) -> Request {
    let binding = media.binding();
    Request {
        parent,
        sequence: 1,
        target: Target {
            display_binding: binding.parent.id,
            view: InputView {
                geometry: binding.geometry,
                viewport: binding.viewport,
                configuration: binding.configuration,
                recovery: binding.recovery,
            },
            bounds: InputBounds::new(DesktopPoint { x: 0, y: 0 }, 320, 240).unwrap(),
            capabilities: Capabilities::default().with(Capability::Keys),
        },
    }
}
fn wire(request: Request, media: &NegotiatedMedia) -> Vec<u8> {
    let mut bytes = vec![0; 1024];
    let n = control::encode_request(
        request,
        &mut bytes,
        media.limits().protocol(),
        InputDirection::ViewerToHost,
        InputDelivery::Reliable,
    )
    .unwrap();
    bytes.truncate(n);
    bytes
}

#[test]
#[allow(clippy::too_many_lines)]
fn idle_broker_survives_two_real_replacements_without_a_grant_or_new_input_lane() {
    run(|c, h| async move {
        let (mut host, mut viewer) = pair_initialized(&c, &h, capabilities(true), |_| {}).await;
        let (hc, vc) = attach(&mut host, &mut viewer, &c, &h, MediaRole::Configuration, 8).await;
        let (hr, vr) = attach(&mut host, &mut viewer, &c, &h, MediaRole::Recovery, 9).await;
        let (hi, vi) = attach(&mut host, &mut viewer, &c, &h, MediaRole::Input, 10).await;
        let (hv, vv) = attach(&mut host, &mut viewer, &c, &h, MediaRole::Video, 11).await;
        let selection = host.selection().clone();
        let parent = host.binding();
        let channels = NegotiatedInput::new(host.io().unwrap().0, &selection, &hc, hi).unwrap();
        let mut client = NegotiatedInput::new(viewer.io().unwrap().0, &selection, &vc, vi).unwrap();
        let mut hm = NegotiatedMedia::new(host.io().unwrap().0, &selection, &hc, &hr, &hv).unwrap();
        let mut vm =
            NegotiatedMedia::new(viewer.io().unwrap().0, &selection, &vc, &vr, &vv).unwrap();
        let original = host.io().unwrap().0.binding();
        let seat = Seat::default();
        let mut broker = host
            .negotiated_control_broker(seat.clone(), channels)
            .unwrap();
        let old_request = request(parent, &hm);
        let mut nonce = 10_000;
        for iteration in 0..2 {
            let previous = hm.binding();
            let until = now(&h).unwrap() + 1_000_000;
            broker
                .begin_reference_recovery(host.io().unwrap().0, &hm, until)
                .unwrap();
            assert_eq!(broker.configuration(host.io().unwrap().0).unwrap(), None);
            assert!(!broker.view_ready(host.io().unwrap().0).unwrap());
            assert_eq!(
                broker.begin_reference_recovery(host.io().unwrap().0, &hm, until + 1),
                Err(GrantError::RequestPending)
            );
            client.begin_recovery(viewer.io().unwrap().0).unwrap();
            let h_routes = host.io().unwrap().1;
            let v_routes = viewer.io().unwrap().1;
            let mut hs = hm
                .begin_replacement(
                    &h,
                    host.io().unwrap().0,
                    h_routes,
                    parent,
                    until,
                    Some([
                        Ticket(200 + iteration * 3),
                        Ticket(201 + iteration * 3),
                        Ticket(202 + iteration * 3),
                    ]),
                    || true,
                )
                .unwrap();
            let mut vs = vm
                .begin_replacement(
                    &c,
                    viewer.io().unwrap().0,
                    v_routes,
                    parent,
                    until,
                    None,
                    || true,
                )
                .unwrap();
            while !hs.is_complete() || !vs.is_complete() {
                hs.advance(&h, host.io().unwrap().0, || true).unwrap();
                vs.advance(&c, viewer.io().unwrap().0, || true).unwrap();
                assert_eq!(
                    broker.service(host.io().unwrap().0, None).unwrap(),
                    Event::Idle
                );
                let (a, b) = Box::pin(support::both(
                    host.drive(
                        Duration::from_millis(1),
                        || {
                            nonce += 1;
                            Ok(nonce)
                        },
                        |r, b| {
                            Ok(broker
                                .dispatch_record(r, b)
                                .unwrap()
                                .unwrap_or(Disposition::Blocked))
                        },
                    ),
                    viewer.drive(Duration::from_millis(1), |_, _| Ok(Disposition::Blocked)),
                ))
                .await;
                a.unwrap();
                b.unwrap();
            }
            hm = hs.finish(&h, host.io().unwrap().0).unwrap();
            vm = vs.finish(&c, viewer.io().unwrap().0).unwrap();
            // This request may race the first-decode acknowledgement. It is
            // retained, not consumed or approved against the retired display.
            let next_request = request(parent, &vm);
            let bytes = wire(next_request, &vm);
            assert_eq!(
                broker
                    .dispatch_record(Route::Stream(h_routes.inbound), &bytes)
                    .unwrap(),
                Some(Disposition::Blocked)
            );
            assert!(broker.request().is_none());
            assert!(broker.native_status().is_none());
            assert_eq!(
                broker.finish_reference_recovery(host.io().unwrap().0, &vm),
                Err(GrantError::Input(crate::input_quic::Error::InvalidRoutes)),
                "matching numeric channels on the peer are not this host's proof"
            );
            assert_eq!(broker.configuration(host.io().unwrap().0).unwrap(), None);
            broker
                .finish_reference_recovery(host.io().unwrap().0, &hm)
                .unwrap();
            client.follow_recovery(viewer.io().unwrap().0, &vm).unwrap();
            assert_eq!(client.channel_binding(), 10);
            assert_eq!(
                broker.configuration(host.io().unwrap().0).unwrap(),
                Some(hm.binding())
            );
            assert_eq!(hm.binding().recovery, previous.recovery.next().unwrap());
            assert_ne!(hm.binding().parent.id, previous.parent.id);
            assert!(host.io().unwrap().0.is_bound_to(&original));
            assert!(broker.request().is_none());
            assert!(broker.native_status().is_none());
            // The seat is genuinely still reservable: replacing display media
            // did not mint a provisional input owner.
            drop(seat.reserve().unwrap());
        }
        let request = request(parent, &vm);
        let route = Route::Stream(host.io().unwrap().1.inbound);
        let bytes = wire(request, &vm);
        client
            .check_request(viewer.io().unwrap().0, request)
            .unwrap();
        assert_eq!(
            broker.dispatch_record(route, &bytes).unwrap(),
            Some(Disposition::Consumed)
        );
        assert_eq!(broker.request(), Some(request));
        let until = now(&h).unwrap() + 1_000_000;
        assert_eq!(
            broker.begin_reference_recovery(host.io().unwrap().0, &hm, until),
            Err(GrantError::RequestPending)
        );
        assert_eq!(
            broker.request(),
            Some(request),
            "recovery rewrote pending consent"
        );
        broker.deny();
        let mut obsolete = old_request;
        obsolete.sequence = 2;
        assert_eq!(
            broker.dispatch_record(route, &wire(obsolete, &vm)),
            Err(GrantError::TargetChanged)
        );
        assert_eq!(
            broker.dispatch_record(route, &bytes),
            Err(GrantError::Replay)
        );
        assert!(broker.native_status().is_none());
        drop(seat.reserve().unwrap());
        viewer.close();
        host.close();
    });
}

#[test]
fn idle_broker_recovery_requires_positive_negotiation() {
    run(|c, h| async move {
        let (mut host, mut viewer) = pair_initialized(&c, &h, capabilities(false), |_| {}).await;
        let (hc, vc) = attach(&mut host, &mut viewer, &c, &h, MediaRole::Configuration, 8).await;
        let (hr, _) = attach(&mut host, &mut viewer, &c, &h, MediaRole::Recovery, 9).await;
        let (hi, _) = attach(&mut host, &mut viewer, &c, &h, MediaRole::Input, 10).await;
        let (hv, _) = attach(&mut host, &mut viewer, &c, &h, MediaRole::Video, 11).await;
        let selection = host.selection().clone();
        let input = NegotiatedInput::new(host.io().unwrap().0, &selection, &hc, hi).unwrap();
        let media = NegotiatedMedia::new(host.io().unwrap().0, &selection, &hc, &hr, &hv).unwrap();
        let mut broker = host
            .negotiated_control_broker(Seat::default(), input)
            .unwrap();
        let until = now(&h).unwrap() + 1_000_000;
        assert_eq!(
            broker.begin_reference_recovery(host.io().unwrap().0, &media, until),
            Err(GrantError::Input(crate::input_quic::Error::InvalidRoutes))
        );
        assert_eq!(
            broker.configuration(host.io().unwrap().0).unwrap(),
            Some(media.binding())
        );
        assert!(vc.completed_on(viewer.io().unwrap().0).is_ok());
        assert!(broker.request().is_none());
        viewer.close();
        host.close();
    });
}
