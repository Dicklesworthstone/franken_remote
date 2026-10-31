//! Actual UDP/TLS fixtures for the independently selected microphone lane.
//! Native capture, endpoint readiness, talk consent and lease authority remain
//! session responsibilities; these tests prove bounded transport ownership.
use super::delivery::{attach, record, transfer};
use super::*;
use fr_wire::attachment::MediaRole;

fn enable(l: &mut Link) {
    l.selection.role = Role::RequestControl;
    for (name, version) in [
        (
            attachment::DELIVERY_CAPABILITY,
            attachment::DELIVERY_VERSION,
        ),
        (attachment::INPUT_CAPABILITY, attachment::INPUT_VERSION),
        (fr_wire::audio::CAPABILITY, fr_wire::audio::VERSION),
        (
            fr_wire::audio_control::CAPABILITY,
            fr_wire::audio_control::VERSION,
        ),
        (fr_wire::audio_up::CAPABILITY, fr_wire::audio_up::VERSION),
    ] {
        l.selection.capabilities.push(Capability {
            name: name.into(),
            version,
            required: false,
        });
    }
    l.selection.capabilities.sort_by(|a, b| a.name.cmp(&b.name));
    l.selection.validate().unwrap();
}

fn offer(l: &mut Link, cx: &Cx, selection: &Selection, id: u32) -> Result<MediaChannel, Error> {
    l.h.offer_media_role(
        cx,
        ChannelScope {
            control: l.hr,
            parent: parent(),
            selection,
        },
        ChannelRequest {
            binding: binding(id),
            ticket: Ticket(1000 + u128::from(id)),
            timeout: Duration::from_secs(2),
        },
        MediaRole::AudioUp,
        || true,
    )
}

#[test]
fn microphone_requires_exact_positive_controller_selection_and_completed_input() {
    run_test!(cx, {
        let mut l = Link::new(&cx).await;
        enable(&mut l);
        let selected = l.selection.clone();
        let before = l.h.usage();
        assert!(matches!(
            offer(&mut l, &cx, &selected, 8),
            Err(Error::WrongRoute)
        ));
        assert_eq!(l.h.usage(), before, "no original input attachment");
        let _input = attach(&mut l, &cx, MediaRole::Input, 8).await;
        let before = l.h.usage();
        let mut observer = selected.clone();
        observer.role = Role::Observe;
        assert!(matches!(
            offer(&mut l, &cx, &observer, 9),
            Err(Error::WrongRoute)
        ));
        for name in [
            attachment::CAPABILITY,
            attachment::INPUT_CAPABILITY,
            fr_wire::audio_up::CAPABILITY,
        ] {
            let mut missing = selected.clone();
            missing.capabilities.retain(|cap| cap.name != name);
            assert!(matches!(
                offer(&mut l, &cx, &missing, 9),
                Err(Error::WrongRoute)
            ));
            let mut future = selected.clone();
            future
                .capabilities
                .iter_mut()
                .find(|cap| cap.name == name)
                .unwrap()
                .version += 1;
            assert!(matches!(
                offer(&mut l, &cx, &future, 9),
                Err(Error::WrongRoute)
            ));
        }
        assert_eq!(l.h.usage(), before);
        assert!(!l.h.is_closed());
        // Uplink selection does not require either playback capability.
        l.selection.capabilities.retain(|cap| {
            cap.name != fr_wire::audio::CAPABILITY && cap.name != fr_wire::audio_control::CAPABILITY
        });
        let (_host, _client, h, c) = attach(&mut l, &cx, MediaRole::AudioUp, 9).await;
        assert_eq!(h.descriptor.role, MediaRole::AudioUp);
        assert_eq!(h.descriptor, c.descriptor);
    });
}

#[test]
fn duplex_audio_uses_distinct_bounded_bindings_and_reverses_every_uplink_route() {
    run_test!(cx, {
        let mut l = Link::new(&cx).await;
        enable(&mut l);
        let _video = attach(&mut l, &cx, MediaRole::Video, 8).await;
        let _input = attach(&mut l, &cx, MediaRole::Input, 9).await;
        let (_down_host, _down_client, hd, cd) =
            attach(&mut l, &cx, MediaRole::AudioDown, 10).await;
        let (_up_host, _up_client, hu, cu) = attach(&mut l, &cx, MediaRole::AudioUp, 11).await;
        assert_eq!(hu.outbound.messages, Messages::AudioReplies);
        assert_eq!(hu.inbound.messages, Messages::AudioControl);
        assert_eq!(cu.outbound.messages, Messages::AudioControl);
        assert_eq!(cu.inbound.messages, Messages::AudioReplies);
        assert_eq!(hu.byte_allowance, 1150);
        assert_eq!(hu.outbound.priority, Priority::Critical);
        let hp = hu.datagram.unwrap();
        let cp = cu.datagram.unwrap();
        assert_eq!((hp.kind, hp.outbound, cp.outbound), (0x62, false, true));
        assert_ne!(
            hd.descriptor.binding.parent.id,
            hu.descriptor.binding.parent.id
        );
        assert_ne!(hd.outbound.stream, hu.outbound.stream);
        for (host, send, receive, kind, length) in [
            (false, cu.outbound, hu.inbound, 0x60, 52),
            (false, cu.outbound, hu.inbound, 0x63, 40),
            (true, hu.outbound, cu.inbound, 0x61, 44),
            (true, hu.outbound, cu.inbound, 0x63, 40),
        ] {
            transfer(
                &mut l,
                &cx,
                host,
                Route::Stream(send),
                Route::Stream(receive),
                &record(kind, 11, length),
            )
            .await;
        }
        transfer(
            &mut l,
            &cx,
            false,
            Route::Datagram(cp),
            Route::Datagram(hp),
            &record(0x62, 11, 300),
        )
        .await;
        // Opposite playback remains usable on this same connection.
        transfer(
            &mut l,
            &cx,
            true,
            Route::Datagram(hd.datagram.unwrap()),
            Route::Datagram(cd.datagram.unwrap()),
            &record(0x62, 10, 300),
        )
        .await;
        let before = (l.h.usage(), l.c.usage());
        for (host, route, bytes) in [
            (true, Route::Stream(hu.outbound), record(0x60, 11, 52)),
            (false, Route::Stream(cu.outbound), record(0x61, 11, 44)),
            (false, Route::Stream(cu.outbound), record(0x62, 11, 300)),
            (false, Route::Datagram(cp), record(0x60, 11, 52)),
            (false, Route::Datagram(cp), record(0x34, 11, 300)),
            (false, Route::Datagram(cp), record(0x62, 10, 300)),
            (false, Route::Datagram(cp), record(0x62, 11, 1151)),
            (true, Route::Datagram(hp), record(0x62, 11, 300)),
            (
                false,
                Route::Datagram(cd.datagram.unwrap()),
                record(0x62, 10, 300),
            ),
        ] {
            let q = if host { &mut l.h } else { &mut l.c };
            assert!(matches!(
                q.send(&cx, route, &bytes, clock(&cx) + 1_000_000, || true),
                Err(Error::WrongRoute | Error::TooLarge)
            ));
        }
        assert_eq!((l.h.usage(), l.c.usage()), before);
        let selected = l.selection.clone();
        assert!(matches!(
            offer(&mut l, &cx, &selected, 12),
            Err(Error::WrongRoute)
        ));
    });
}

async fn quiet(l: &mut Link, cx: &Cx) {
    let until = clock(cx) + 1_000_000;
    while l.h.usage().retained_send_records != 0 || l.c.usage().retained_send_records != 0 {
        assert!(clock(cx) < until);
        l.drive(cx).await;
    }
}

#[test]
fn microphone_retirement_drops_its_queue_preserves_other_lanes_and_cannot_reopen() {
    run_test!(cx, {
        let mut l = Link::new(&cx).await;
        enable(&mut l);
        let (_vh, _vc, hv, cv) = attach(&mut l, &cx, MediaRole::Video, 8).await;
        let (_ih, _ic, hi, ci) = attach(&mut l, &cx, MediaRole::Input, 9).await;
        let (_dh, _dc, hd, cd) = attach(&mut l, &cx, MediaRole::AudioDown, 10).await;
        let (mut h, mut c, hu, cu) = attach(&mut l, &cx, MediaRole::AudioUp, 11).await;
        quiet(&mut l, &cx).await;
        // A foreign connection cannot retire either original owner.
        assert!(matches!(
            h.retire_audio(&mut l.c, &cx),
            Err(Error::WrongRoute)
        ));
        assert!(h.completed_on(&l.h).is_ok());
        l.c.send(
            &cx,
            Route::Stream(cu.outbound),
            &record(0x60, 11, 52),
            clock(&cx) + 1_000_000,
            || true,
        )
        .unwrap();
        assert_eq!(l.c.usage().retained_send_records, 1);
        c.retire_audio(&mut l.c, &cx).unwrap();
        assert_eq!(l.c.usage().retained_send_records, 0);
        assert!(!l.c.has_route(Route::Datagram(cu.datagram.unwrap())));
        let until = clock(&cx) + 1_000_000;
        while l.h.has_route(Route::Stream(hu.inbound)) {
            assert!(
                clock(&cx) < until,
                "peer must retire only its uplink reservation"
            );
            l.drive(&cx).await;
        }
        assert!(!l.h.has_route(Route::Datagram(hu.datagram.unwrap())));
        assert!(!l.h.is_closed() && !l.c.is_closed());
        for (send, receive, kind, id) in [
            (hd.datagram.unwrap(), cd.datagram.unwrap(), 0x62, 10),
            (hv.datagram.unwrap(), cv.datagram.unwrap(), 0x34, 8),
        ] {
            transfer(
                &mut l,
                &cx,
                true,
                Route::Datagram(send),
                Route::Datagram(receive),
                &record(kind, id, 300),
            )
            .await;
        }
        transfer(
            &mut l,
            &cx,
            false,
            Route::Stream(ci.outbound),
            Route::Stream(hi.inbound),
            &record(0x40, 9, 96),
        )
        .await;
        let (send, receive) = (l.hr.outbound, l.cr.inbound);
        transfer(
            &mut l,
            &cx,
            true,
            Route::Stream(send),
            Route::Stream(receive),
            &record(0x12, 7, 96),
        )
        .await;
        let selected = l.selection.clone();
        assert!(matches!(
            offer(&mut l, &cx, &selected, 12),
            Err(Error::WrongRoute)
        ));
        h.retire_audio(&mut l.h, &cx).unwrap();
        c.retire_audio(&mut l.c, &cx).unwrap();
        drop((h, c));
        l.h.tick(&cx, || true).unwrap();
        l.c.tick(&cx, || true).unwrap();
    });
}

#[test]
fn an_incomplete_microphone_attachment_cannot_use_optional_retirement() {
    run_test!(cx, {
        let mut l = Link::new(&cx).await;
        enable(&mut l);
        let _input = attach(&mut l, &cx, MediaRole::Input, 8).await;
        let selected = l.selection.clone();
        let mut h = offer(&mut l, &cx, &selected, 9).unwrap();
        assert!(matches!(
            h.retire_audio(&mut l.h, &cx),
            Err(Error::WrongRoute)
        ));
        drop(h);
        assert!(l.h.tick(&cx, || true).is_err());
        assert!(l.h.is_closed());
    });
}
