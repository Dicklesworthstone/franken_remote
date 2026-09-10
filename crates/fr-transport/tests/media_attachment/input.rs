//! Real transport admission only. Native effects are tested by fr-native.
use super::*;
use fr_wire::attachment::MediaRole;

fn enable(l: &mut Link) {
    l.selection.role = Role::RequestControl;
    l.selection.capabilities.push(Capability {
        name: attachment::INPUT_CAPABILITY.into(),
        version: attachment::INPUT_VERSION,
        required: true,
    });
    l.selection.capabilities.sort_by(|a, b| a.name.cmp(&b.name));
}
fn offer(l: &mut Link, cx: &Cx, id: u32) -> Result<MediaChannel, Error> {
    l.h.offer_media_role(
        cx,
        ChannelScope {
            control: l.hr,
            parent: parent(),
            selection: &l.selection,
        },
        ChannelRequest {
            binding: binding(id),
            ticket: Ticket(1000 + u128::from(id)),
            timeout: Duration::from_secs(2),
        },
        MediaRole::Input,
        || true,
    )
}
async fn attach(
    l: &mut Link,
    cx: &Cx,
) -> (MediaChannel, MediaChannel, AttachedChannel, AttachedChannel) {
    let mut h = offer(l, cx, 8).unwrap();
    let mut c = l.viewer(cx, &mut h).await;
    let (ha, ca) = l.attached(cx, &mut h, &mut c).await;
    (h, c, ha, ca)
}
// Independent FRD0 envelopes exercise the transport family and size gates.
fn record(kind: u16, len: usize) -> Vec<u8> {
    let mut b = vec![0; len];
    b[..4].copy_from_slice(b"FRD0");
    b[6..8].copy_from_slice(&kind.to_be_bytes());
    b[12..16].copy_from_slice(&u32::try_from(len - 24).unwrap().to_be_bytes());
    b[16..20].copy_from_slice(&8u32.to_be_bytes());
    b
}
async fn transfer(l: &mut Link, cx: &Cx, host: bool, send: Route, recv: Route, bytes: &[u8]) {
    let until = clock(cx) + 1_000_000;
    loop {
        let q = if host { &mut l.h } else { &mut l.c };
        match q.send(cx, send, bytes, until, || true) {
            Ok(()) => break,
            Err(Error::Backpressure) => l.drive(cx).await,
            e => panic!("unexpected send: {e:?}"),
        }
        assert!(clock(cx) < until);
    }
    let mut got = false;
    while !got {
        assert!(clock(cx) < until);
        l.drive(cx).await;
        let q = if host { &mut l.c } else { &mut l.h };
        q.receive_ready(
            cx,
            || true,
            |r| r == recv,
            |_, b| {
                assert_eq!(b, bytes);
                got = true;
                Ok(Disposition::Consumed)
            },
        )
        .unwrap();
    }
}

#[test]
fn input_requires_its_own_capability_and_control_intent_before_reserving() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut l = Link::new(&cx).await;
        let before = l.h.usage();
        assert!(matches!(offer(&mut l, &cx, 8), Err(Error::WrongRoute)));
        enable(&mut l);
        l.selection.role = Role::Observe;
        assert!(matches!(offer(&mut l, &cx, 8), Err(Error::WrongRoute)));
        l.selection.role = Role::RequestControl;
        l.selection
            .capabilities
            .iter_mut()
            .find(|c| c.name == attachment::INPUT_CAPABILITY)
            .unwrap()
            .version += 1;
        assert!(matches!(offer(&mut l, &cx, 8), Err(Error::WrongRoute)));
        assert_eq!(before, l.h.usage());
        assert!(!l.h.is_closed());
        l.selection
            .capabilities
            .iter_mut()
            .find(|c| c.name == attachment::INPUT_CAPABILITY)
            .unwrap()
            .version = attachment::INPUT_VERSION;
        attach(&mut l, &cx).await;
    });
}

#[test]
fn one_negotiated_input_family_preserves_order_and_reverse_feedback() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut l = Link::new(&cx).await;
        enable(&mut l);
        let original = l.h.binding();
        let (_, _, h, c) = attach(&mut l, &cx).await;
        assert!(l.h.is_bound_to(&original));
        assert_eq!(c.outbound.messages, Messages::InputActions);
        assert_eq!(h.outbound.messages, Messages::InputFeedback);
        assert_eq!(c.outbound.maximum, fr_wire::input::MAX_INPUT_RECORD_BYTES);
        assert_eq!(c.datagram.unwrap().kind, 0x42);
        assert!(c.datagram.unwrap().outbound);
        assert!(!h.datagram.unwrap().outbound);
        assert_eq!(h.inbound.priority, Priority::Critical);
        for kind in [0x40, 0x41, 0x43, 0x44, 0x45, 0x46, 0x47] {
            transfer(
                &mut l,
                &cx,
                false,
                Route::Stream(c.outbound),
                Route::Stream(h.inbound),
                &record(kind, 160),
            )
            .await;
        }
        for kind in [0x17, 0x48] {
            transfer(
                &mut l,
                &cx,
                true,
                Route::Stream(h.outbound),
                Route::Stream(c.inbound),
                &record(kind, 128),
            )
            .await;
        }
        transfer(
            &mut l,
            &cx,
            false,
            Route::Datagram(c.datagram.unwrap()),
            Route::Datagram(h.datagram.unwrap()),
            &record(0x42, 160),
        )
        .await;
        let before = l.c.usage();
        for kind in [0x17, 0x48, 0x34, 0x30, 0x42] {
            assert_eq!(
                l.c.send(
                    &cx,
                    Route::Stream(c.outbound),
                    &record(kind, 160),
                    clock(&cx) + 100_000,
                    || true
                ),
                Err(Error::WrongRoute)
            );
        }
        assert_eq!(before, l.c.usage());
        assert_eq!(
            l.h.send(
                &cx,
                Route::Datagram(h.datagram.unwrap()),
                &record(0x42, 160),
                clock(&cx) + 100_000,
                || true
            ),
            Err(Error::WrongRoute)
        );
    });
}

#[test]
fn pointer_cannot_bypass_attachment_and_keeps_the_native_datagram_ceiling() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut l = Link::new(&cx).await;
        enable(&mut l);
        let r = DatagramRoute {
            binding: 8,
            kind: 0x42,
            outbound: true,
        };
        let mut h = offer(&mut l, &cx, 8).unwrap();
        let mut c = l.viewer(&cx, &mut h).await;
        assert!(!l.c.has_route(Route::Datagram(r)));
        assert_eq!(
            l.c.send(
                &cx,
                Route::Datagram(r),
                &record(0x42, 160),
                clock(&cx) + 100_000,
                || true
            ),
            Err(Error::WrongRoute)
        );
        let (ha, ca) = l.attached(&cx, &mut h, &mut c).await;
        assert!(ca.byte_allowance > MAX_DATAGRAM_RECORD as u64);
        let before = l.c.usage();
        assert_eq!(
            l.c.send(
                &cx,
                Route::Datagram(r),
                &record(0x42, MAX_DATAGRAM_RECORD + 1),
                clock(&cx) + 100_000,
                || true
            ),
            Err(Error::TooLarge)
        );
        assert_eq!(before, l.c.usage());
        transfer(
            &mut l,
            &cx,
            false,
            Route::Datagram(r),
            Route::Datagram(ha.datagram.unwrap()),
            &record(0x42, MAX_DATAGRAM_RECORD),
        )
        .await;
    });
}

#[test]
fn a_second_input_ordering_domain_is_refused_even_after_owner_drop() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut l = Link::new(&cx).await;
        enable(&mut l);
        let (h, c, _, _) = attach(&mut l, &cx).await;
        drop(h);
        drop(c);
        let before = l.h.usage();
        assert!(matches!(offer(&mut l, &cx, 9), Err(Error::WrongRoute)));
        assert_eq!(before, l.h.usage());
        assert!(!l.h.is_closed());
        // Refusal does not consume an ID needed by an unrelated media channel.
        let mut h = l.offer(&cx, 9, 2000);
        let mut c = l.viewer(&cx, &mut h).await;
        l.attached(&cx, &mut h, &mut c).await;
    });
}

#[test]
fn abandoning_input_before_ack_closes_without_installing_pointer() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut l = Link::new(&cx).await;
        enable(&mut l);
        let mut h = offer(&mut l, &cx, 8).unwrap();
        let c = l.viewer(&cx, &mut h).await;
        drop(c);
        assert!(l.c.tick(&cx, || true).is_err());
        assert!(l.c.is_closed());
        assert!(h.transmit(&mut l.h, &cx, || false).is_err());
        assert!(l.h.is_closed());
    });
}

#[test]
fn smaller_selected_record_limits_bound_both_input_directions_and_pointer() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut l = Link::new(&cx).await;
        enable(&mut l);
        l.selection.limits = ProtocolLimits::with_overrides(fr_core::limits::LimitOverrides {
            max_control_message_bytes: Some(640),
            ..Default::default()
        })
        .unwrap();
        let (_, _, h, c) = attach(&mut l, &cx).await;
        assert_eq!(h.byte_allowance, 640);
        assert_eq!(c.outbound.maximum, 640);
        for (route, kind) in [
            (Route::Stream(c.outbound), 0x40),
            (Route::Datagram(c.datagram.unwrap()), 0x42),
        ] {
            assert_eq!(
                l.c.send(&cx, route, &record(kind, 641), clock(&cx) + 100_000, || {
                    true
                }),
                Err(Error::TooLarge)
            );
        }
        transfer(
            &mut l,
            &cx,
            false,
            Route::Stream(c.outbound),
            Route::Stream(h.inbound),
            &record(0x40, 640),
        )
        .await;
    });
}
