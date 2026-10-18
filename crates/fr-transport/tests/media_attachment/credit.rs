//! Real UDP/TLS exchanges with independently bounded endpoint policies.
//! Synthetic selection and FRD0 payloads prove routing, not native OS effects.
use super::delivery::{attach, record, transfer};
use super::*;
use fr_wire::attachment::{MediaRole, credit};

fn enable(l: &mut Link) {
    l.selection.role = Role::RequestControl;
    for (name, version) in [
        (credit::CAPABILITY, credit::VERSION),
        (
            attachment::DELIVERY_CAPABILITY,
            attachment::DELIVERY_VERSION,
        ),
        (attachment::INPUT_CAPABILITY, attachment::INPUT_VERSION),
        (
            attachment::CLIPBOARD_CAPABILITY,
            attachment::CLIPBOARD_VERSION,
        ),
        (attachment::FILES_CAPABILITY, attachment::FILES_VERSION),
        (
            attachment::BULK_RECORD_CAPABILITY,
            attachment::BULK_RECORD_VERSION,
        ),
        (fr_wire::clipboard::CAPABILITY, fr_wire::clipboard::VERSION),
        (fr_wire::files::CAPABILITY, fr_wire::files::VERSION),
        (fr_wire::audio::CAPABILITY, fr_wire::audio::VERSION),
        (
            fr_wire::audio_control::CAPABILITY,
            fr_wire::audio_control::VERSION,
        ),
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

async fn link(cx: &Cx, host: Policy, client: Policy) -> Link {
    let (c, h) = support::native_pair(cx, "localhost", ALPN).await;
    let (mut c, cr) = QuicRecords::bootstrap(c.unwrap(), cx, client).unwrap();
    let (mut h, hr) = QuicRecords::bootstrap(h.unwrap(), cx, host).unwrap();
    let cr = c.bind_control(cx, cr, 7, 4096, || true).unwrap();
    let hr = h.bind_control(cx, hr, 7, 4096, || true).unwrap();
    let mut l = Link {
        c,
        h,
        cr,
        hr,
        selection: selection(),
    };
    enable(&mut l);
    l
}

fn offer(l: &mut Link, cx: &Cx, id: u32, role: MediaRole) -> MediaChannel {
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

fn kinds(role: MediaRole) -> (u16, Option<u16>) {
    match role {
        MediaRole::Configuration => (0x30, Some(0x31)),
        MediaRole::Recovery => (0x32, None),
        MediaRole::Video => (0x37, Some(0x35)),
        MediaRole::Input => (0x48, Some(0x40)),
        MediaRole::Clipboard => (0x51, Some(0x51)),
        MediaRole::Files => (0x72, Some(0x72)),
        MediaRole::AudioDown => (0x60, Some(0x61)),
    }
}

async fn bounded_records(l: &mut Link, cx: &Cx, h: AttachedChannel, c: AttachedChannel) {
    let (host_kind, client_kind) = kinds(h.descriptor.role);
    for (host, send, receive, kind) in [
        (true, h.outbound, c.inbound, Some(host_kind)),
        (false, c.outbound, h.inbound, client_kind),
    ] {
        let Some(kind) = kind else { continue };
        // Exercise repeated records after promotion, including the retained
        // send accounting and native receive-credit replenishment paths.
        for _ in 0..2 {
            transfer(
                l,
                cx,
                host,
                Route::Stream(send),
                Route::Stream(receive),
                &record(kind, send.binding, send.maximum),
            )
            .await;
        }
        let q = if host { &mut l.h } else { &mut l.c };
        let before = q.usage();
        assert_eq!(
            q.send(
                cx,
                Route::Stream(send),
                &record(kind, send.binding, send.maximum + 1),
                clock(cx) + 1_000_000,
                || true
            ),
            Err(Error::TooLarge)
        );
        assert_eq!(q.usage(), before);
        assert!(!q.is_closed());
    }
    if h.descriptor.role != MediaRole::Input
        && let (Some(send), Some(receive)) = (h.datagram, c.datagram)
    {
        assert!(send.outbound && !receive.outbound);
        let maximum = usize::try_from(h.byte_allowance).unwrap();
        transfer(
            l,
            cx,
            true,
            Route::Datagram(send),
            Route::Datagram(receive),
            &record(send.kind, send.binding, maximum),
        )
        .await;
        let before = l.h.usage();
        assert_eq!(
            l.h.send(
                cx,
                Route::Datagram(send),
                &record(send.kind, send.binding, maximum + 1),
                clock(cx) + 1_000_000,
                || true
            ),
            Err(Error::TooLarge)
        );
        assert_eq!(l.h.usage(), before);
    }
}

#[test]
fn every_role_intersects_the_actual_endpoint_caps_and_enforces_the_narrowed_routes() {
    run_test!(cx, {
        let small = Policy {
            critical_send_bytes: 4096,
            retained_send_bytes: 16_384,
            datagram_record_bytes: 640,
            ..Policy::default()
        };
        for (host, client) in [(Policy::default(), small), (small, Policy::default())] {
            let mut l = link(&cx, host, client).await;
            let original = (l.h.binding(), l.c.binding());
            for (offset, role, expected) in [
                (0, MediaRole::Configuration, 4096),
                (1, MediaRole::Recovery, 4096),
                (2, MediaRole::Video, 640),
                (
                    3,
                    MediaRole::Input,
                    4096.min(fr_wire::input::MAX_INPUT_RECORD_BYTES),
                ),
                (4, MediaRole::Clipboard, 16_384),
                (5, MediaRole::Files, 16_384),
                (6, MediaRole::AudioDown, 640),
            ] {
                let (ho, co, h, c) = attach(&mut l, &cx, role, 8 + offset).await;
                for a in [h, c] {
                    assert_eq!(a.byte_allowance, expected as u64, "{role:?}");
                    assert_eq!(a.outbound.maximum, expected);
                    assert_eq!(a.inbound.maximum, expected);
                }
                assert_eq!(ho.completed_limits(&l.h).unwrap(), l.selection.limits);
                assert_eq!(co.completed_limits(&l.c).unwrap(), l.selection.limits);
                bounded_records(&mut l, &cx, h, c).await;
            }
            assert!(l.h.is_bound_to(&original.0));
            assert!(l.c.is_bound_to(&original.1));
        }
    });
}

#[test]
fn parallel_clipboard_and_files_accept_their_own_credit_in_reverse_ack_order() {
    run_test!(cx, {
        let mut l = link(
            &cx,
            Policy::default(),
            Policy {
                retained_send_bytes: 4096,
                ..Policy::default()
            },
        )
        .await;
        let _input = attach(&mut l, &cx, MediaRole::Input, 8).await;
        let mut hc = offer(&mut l, &cx, 9, MediaRole::Clipboard);
        let mut hf = offer(&mut l, &cx, 10, MediaRole::Files);
        let mut vc = l.viewer(&cx, &mut hc).await;
        let mut vf = l.viewer(&cx, &mut hf).await;
        let until = clock(&cx) + 1_500_000;
        loop {
            assert!(clock(&cx) < until, "{hc:?} {hf:?} {vc:?} {vf:?}");
            vf.transmit(&mut l.c, &cx, || true).unwrap();
            vc.transmit(&mut l.c, &cx, || true).unwrap();
            hc.transmit(&mut l.h, &cx, || true).unwrap();
            hf.transmit(&mut l.h, &cx, || true).unwrap();
            l.drive(&cx).await;
            hc.dispatch(&mut l.h, &cx, || true).unwrap();
            hf.dispatch(&mut l.h, &cx, || true).unwrap();
            vc.dispatch(&mut l.c, &cx, || true).unwrap();
            vf.dispatch(&mut l.c, &cx, || true).unwrap();
            let done = [
                hc.finish(&mut l.h, &cx, || true).unwrap(),
                hf.finish(&mut l.h, &cx, || true).unwrap(),
                vc.finish(&mut l.c, &cx, || true).unwrap(),
                vf.finish(&mut l.c, &cx, || true).unwrap(),
            ];
            if done.iter().all(Option::is_some) {
                let [hclip, hfile, cclip, cfile] = done.map(Option::unwrap);
                for a in [hclip, hfile, cclip, cfile] {
                    assert_eq!(a.byte_allowance, 4096);
                }
                assert_eq!(hclip.descriptor, cclip.descriptor);
                assert_eq!(hfile.descriptor, cfile.descriptor);
                bounded_records(&mut l, &cx, hclip, cclip).await;
                bounded_records(&mut l, &cx, hfile, cfile).await;
                break;
            }
        }
        hc.retire_clipboard(&mut l.h, &cx).unwrap();
        assert!(hf.completed_on(&l.h).is_ok());
        assert!(!l.h.is_closed());
    });
}

// Independently written FRD0 acceptance. The exchange must check its selected
// layout and owner even though the shared control router admits this kind.
fn accepted(binding: u32, allowance: u64) -> Vec<u8> {
    let mut bytes = record(0x1c, parent().id, credit::ACCEPTED_BYTES);
    bytes[24..28].copy_from_slice(&binding.to_be_bytes());
    bytes[28..36].copy_from_slice(&allowance.to_be_bytes());
    bytes
}

async fn refused_ack(l: &mut Link, cx: &Cx, h: &mut MediaChannel, bytes: &[u8]) -> Error {
    let until = clock(cx) + 1_000_000;
    loop {
        match l
            .c
            .send(cx, Route::Stream(l.cr.outbound), bytes, until, || true)
        {
            Ok(()) => break,
            Err(Error::Backpressure) => l.drive(cx).await,
            other => panic!("unexpected ACK send: {other:?}"),
        }
        assert!(clock(cx) < until);
    }
    loop {
        assert!(clock(cx) < until);
        l.drive(cx).await;
        if let Err(error) = h.dispatch(&mut l.h, cx, || true) {
            assert!(l.h.is_closed());
            assert!(!h.is_complete());
            return error;
        }
    }
}

#[test]
fn selected_layout_unknown_binding_and_invalid_credit_are_never_silently_accepted() {
    run_test!(cx, {
        for (extended, bytes, expected) in [
            (
                true,
                encoded(Message::Accepted(8), D::ViewerToHost),
                Error::Malformed,
            ),
            (false, accepted(8, 4096), Error::Malformed),
            (true, accepted(99, 4096), Error::WrongRoute),
            (
                true,
                accepted(8, attachment::GRANT_RECORD_BYTES as u64 - 1),
                Error::Malformed,
            ),
            (true, accepted(8, 65_537), Error::Malformed),
        ] {
            let mut l = Link::new(&cx).await;
            if extended {
                enable(&mut l);
            }
            let mut h = l.offer(&cx, 8, 90);
            let _viewer = l.viewer(&cx, &mut h).await;
            assert_eq!(refused_ack(&mut l, &cx, &mut h, &bytes).await, expected);
        }
    });
}

#[test]
fn a_consumed_acceptance_cannot_supply_credit_to_another_binding() {
    run_test!(cx, {
        let mut l = Link::new(&cx).await;
        enable(&mut l);
        let _first = attach(&mut l, &cx, MediaRole::Configuration, 8).await;
        let mut next = l.offer(&cx, 9, 91);
        let _viewer = l.viewer(&cx, &mut next).await;
        assert_eq!(
            refused_ack(&mut l, &cx, &mut next, &accepted(8, 4096)).await,
            Error::WrongRoute
        );
    });
}

#[test]
fn malformed_credit_for_a_pending_sibling_is_not_hidden_by_dispatch() {
    run_test!(cx, {
        let mut l = Link::new(&cx).await;
        enable(&mut l);
        let _input = attach(&mut l, &cx, MediaRole::Input, 8).await;
        let mut clipboard = offer(&mut l, &cx, 9, MediaRole::Clipboard);
        let mut files = offer(&mut l, &cx, 10, MediaRole::Files);
        let _viewer_clipboard = l.viewer(&cx, &mut clipboard).await;
        let _viewer_files = l.viewer(&cx, &mut files).await;
        assert_eq!(
            refused_ack(&mut l, &cx, &mut clipboard, &accepted(10, 0)).await,
            Error::Malformed
        );
        assert!(!files.is_complete());
    });
}

#[test]
fn an_unknown_optional_credit_version_uses_the_legacy_attachment() {
    run_test!(cx, {
        let mut l = Link::new(&cx).await;
        enable(&mut l);
        l.selection
            .capabilities
            .iter_mut()
            .find(|cap| cap.name == credit::CAPABILITY)
            .unwrap()
            .version += 1;
        assert!(!credit::selected(&l.selection));
        let (_, _, h, c) = attach(&mut l, &cx, MediaRole::Configuration, 8).await;
        assert_eq!(h.byte_allowance, 8192);
        assert_eq!(h.byte_allowance, c.byte_allowance);
        bounded_records(&mut l, &cx, h, c).await;
    });
}
