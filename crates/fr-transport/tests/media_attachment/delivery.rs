use super::*;
use fr_wire::attachment::MediaRole;

fn enable(l: &mut Link) {
    l.selection.capabilities.push(Capability {
        name: attachment::DELIVERY_CAPABILITY.into(),
        version: attachment::DELIVERY_VERSION,
        required: true,
    });
}
fn offer(l: &mut Link, cx: &Cx, role: MediaRole, id: u32) -> MediaChannel {
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
        role,
        || true,
    )
    .unwrap()
}
async fn attach(
    l: &mut Link,
    cx: &Cx,
    role: MediaRole,
    id: u32,
) -> (MediaChannel, MediaChannel, AttachedChannel, AttachedChannel) {
    let mut h = offer(l, cx, role, id);
    assert!(matches!(h.completed_on(&l.h), Err(Error::WrongRoute)));
    let mut c = l.viewer(cx, &mut h).await;
    let (ha, ca) = l.attached(cx, &mut h, &mut c).await;
    assert_eq!(ha, h.completed_on(&l.h).unwrap());
    assert_eq!(ca, c.completed_on(&l.c).unwrap());
    (h, c, ha, ca)
}
// Payload semantics belong to the actual media codecs. These independent
// FRD0 envelopes isolate route, direction, length and retained-storage checks.
fn record(kind: u16, binding: u32, len: usize) -> Vec<u8> {
    assert!(len >= fr_wire::HEADER_BYTES);
    let mut b = vec![0; len];
    b[..4].copy_from_slice(b"FRD0");
    b[6..8].copy_from_slice(&kind.to_be_bytes());
    b[12..16].copy_from_slice(&u32::try_from(len - 24).unwrap().to_be_bytes());
    b[16..20].copy_from_slice(&binding.to_be_bytes());
    b
}
async fn transfer(l: &mut Link, cx: &Cx, host: bool, send: Route, receive: Route, bytes: &[u8]) {
    let until = clock(cx) + 1_000_000;
    loop {
        let q = if host { &mut l.h } else { &mut l.c };
        match q.send(cx, send, bytes, until, || true) {
            Ok(()) => break,
            Err(Error::Backpressure) => l.drive(cx).await,
            other => panic!("unexpected send result: {other:?}"),
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
            |r| r == receive,
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
fn legacy_selection_cannot_activate_recovery_or_video() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut l = Link::new(&cx).await;
        for role in [MediaRole::Recovery, MediaRole::Video] {
            let before = l.h.usage();
            let result = l.h.offer_media_role(
                &cx,
                ChannelScope {
                    control: l.hr,
                    parent: parent(),
                    selection: &l.selection,
                },
                ChannelRequest {
                    binding: binding(8),
                    ticket: Ticket(90),
                    timeout: Duration::from_secs(2),
                },
                role,
                || true,
            );
            assert!(matches!(result, Err(Error::WrongRoute)));
            assert_eq!(l.h.usage(), before);
            assert!(!l.h.is_closed());
        }
        enable(&mut l);
        let (_, _, h, c) = attach(&mut l, &cx, MediaRole::Recovery, 8).await;
        assert_eq!(h.outbound.priority, Priority::Bulk);
        assert_eq!(c.inbound.priority, Priority::Bulk);
        assert_eq!(c.outbound.messages, Messages::NoApplication);
    });
}

#[test]
fn all_media_lanes_exchange_on_the_original_connection_without_static_routes() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut l = Link::new(&cx).await;
        enable(&mut l);
        let identity = l.h.binding();
        let (config, _, hc, cc) = attach(&mut l, &cx, MediaRole::Configuration, 8).await;
        let (_, _, hr, cr) = attach(&mut l, &cx, MediaRole::Recovery, 9).await;
        let (video, _, hv, cv) = attach(&mut l, &cx, MediaRole::Video, 10).await;
        assert!(l.h.is_bound_to(&identity));
        assert_eq!(hc.datagram, None);
        assert_eq!(hr.datagram, None);
        assert_eq!(hv.byte_allowance, 1150);
        assert_eq!(hr.byte_allowance, 8192);
        assert_eq!(hv.datagram.unwrap().binding, hv.outbound.binding);
        assert_eq!(hv.inbound.binding, hv.outbound.binding);
        for (host, send, recv, bytes) in [
            (
                true,
                Route::Stream(hc.outbound),
                Route::Stream(cc.inbound),
                record(0x30, 8, 320),
            ),
            (
                false,
                Route::Stream(cc.outbound),
                Route::Stream(hc.inbound),
                record(0x31, 8, 120),
            ),
            (
                true,
                Route::Stream(hr.outbound),
                Route::Stream(cr.inbound),
                record(0x32, 9, 4096),
            ),
            (
                true,
                Route::Stream(hv.outbound),
                Route::Stream(cv.inbound),
                record(0x37, 10, 120),
            ),
            (
                false,
                Route::Stream(cv.outbound),
                Route::Stream(hv.inbound),
                record(0x35, 10, 120),
            ),
            (
                true,
                Route::Datagram(hv.datagram.unwrap()),
                Route::Datagram(cv.datagram.unwrap()),
                record(0x34, 10, 1150),
            ),
        ] {
            transfer(&mut l, &cx, host, send, recv, &bytes).await;
        }
        let before = l.h.usage();
        for bytes in [
            record(0x34, 10, 120),
            record(0x35, 10, 120),
            record(0x30, 10, 120),
        ] {
            assert_eq!(
                l.h.send(
                    &cx,
                    Route::Stream(hv.outbound),
                    &bytes,
                    clock(&cx) + 1_000_000,
                    || true
                ),
                Err(Error::WrongRoute)
            );
        }
        assert_eq!(l.h.usage(), before);
        assert!(matches!(video.completed_on(&l.c), Err(Error::WrongRoute)));
        assert_eq!(config.completed_on(&l.h).unwrap(), hc);
        assert_eq!(
            l.c.send(
                &cx,
                Route::Stream(cr.outbound),
                &record(0x35, 9, 120),
                clock(&cx) + 1_000_000,
                || true
            ),
            Err(Error::WrongRoute)
        );
    });
}

#[test]
fn datagram_is_not_a_pre_attachment_escape_and_has_the_negotiated_cap() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut l = Link::new(&cx).await;
        enable(&mut l);
        l.selection.limits = ProtocolLimits::with_overrides(fr_core::limits::LimitOverrides {
            max_control_message_bytes: Some(640),
            ..Default::default()
        })
        .unwrap();
        let mut h = offer(&mut l, &cx, MediaRole::Video, 8);
        let datagram = DatagramRoute {
            binding: 8,
            kind: 0x34,
            outbound: true,
        };
        assert!(!l.h.has_route(Route::Datagram(datagram)));
        assert_eq!(
            l.h.send(
                &cx,
                Route::Datagram(datagram),
                &record(0x34, 8, 120),
                clock(&cx) + 1_000_000,
                || true
            ),
            Err(Error::WrongRoute)
        );
        let mut c = l.viewer(&cx, &mut h).await;
        assert!(!l.c.has_route(Route::Datagram(DatagramRoute {
            outbound: false,
            ..datagram
        })));
        let (ha, ca) = l.attached(&cx, &mut h, &mut c).await;
        assert_eq!(ha.byte_allowance, 640);
        assert_eq!(ca.byte_allowance, 640);
        let before = l.h.usage();
        assert_eq!(
            l.h.send(
                &cx,
                Route::Datagram(datagram),
                &record(0x34, 8, 641),
                clock(&cx) + 1_000_000,
                || true
            ),
            Err(Error::TooLarge)
        );
        assert_eq!(l.h.usage(), before);
        transfer(
            &mut l,
            &cx,
            true,
            Route::Datagram(datagram),
            Route::Datagram(ca.datagram.unwrap()),
            &record(0x34, 8, 640),
        )
        .await;
        let mut foreign = Link::new(&cx).await;
        assert!(matches!(h.completed_on(&foreign.h), Err(Error::WrongRoute)));
        assert!(!foreign.h.has_route(Route::Datagram(datagram)));
        assert_eq!(
            foreign.h.send(
                &cx,
                Route::Datagram(datagram),
                &record(0x34, 8, 120),
                clock(&cx) + 1_000_000,
                || true
            ),
            Err(Error::WrongRoute)
        );
    });
}

#[test]
fn recovery_keeps_bulk_charges_and_does_not_consume_critical_record_credit() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut l = Link::new(&cx).await;
        enable(&mut l);
        let (_, _, h, _) = attach(&mut l, &cx, MediaRole::Recovery, 8).await;
        let until = clock(&cx) + 1_000_000;
        while l.h.usage().critical_send_records != 0 || l.h.usage().retained_send_records != 0 {
            assert!(clock(&cx) < until);
            l.drive(&cx).await;
        }
        l.h.send(
            &cx,
            Route::Stream(h.outbound),
            &record(0x32, 8, 4096),
            until,
            || true,
        )
        .unwrap();
        let usage = l.h.usage();
        assert_eq!(usage.retained_send_records, 1);
        assert_eq!(usage.retained_send_upper_bound, 4096);
        assert_eq!(usage.critical_send_records, 0);
        l.h.send(
            &cx,
            Route::Stream(l.hr.outbound),
            &record(0x15, 7, 100),
            until,
            || true,
        )
        .unwrap();
        assert_eq!(l.h.usage().critical_send_records, 1);
        assert_eq!(l.h.usage().retained_send_records, 2);
        assert_eq!(l.h.usage().retained_send_upper_bound, 4196);
        assert_eq!(l.h.usage().critical_send_bytes, 100);
    });
}

#[test]
fn fourth_video_slot_is_last_and_refusal_does_not_consume_another_binding() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut l = Link::new(&cx).await;
        enable(&mut l);
        for id in 8..12 {
            attach(&mut l, &cx, MediaRole::Video, id).await;
        }
        let before = l.h.usage();
        let result = l.h.offer_media_role(
            &cx,
            ChannelScope {
                control: l.hr,
                parent: parent(),
                selection: &l.selection,
            },
            ChannelRequest {
                binding: binding(12),
                ticket: Ticket(1012),
                timeout: Duration::from_secs(2),
            },
            MediaRole::Video,
            || true,
        );
        assert!(matches!(result, Err(Error::Backpressure)));
        assert_eq!(l.h.usage(), before);
        assert!(!l.h.is_closed());
        attach(&mut l, &cx, MediaRole::Recovery, 12).await;
    });
}
