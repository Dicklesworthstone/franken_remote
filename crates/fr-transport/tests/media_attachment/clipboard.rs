//! Real UDP/TLS attachment, not native permission or live-tailnet qualification.
use super::*;
use fr_wire::attachment::MediaRole;

fn enable(l: &mut Link) {
    l.selection.role = Role::RequestControl;
    for (name, version) in [
        (attachment::INPUT_CAPABILITY, attachment::INPUT_VERSION),
        (
            attachment::CLIPBOARD_CAPABILITY,
            attachment::CLIPBOARD_VERSION,
        ),
        (fr_wire::clipboard::CAPABILITY, fr_wire::clipboard::VERSION),
    ] {
        l.selection.capabilities.push(Capability {
            name: name.into(),
            version,
            required: true,
        });
    }
    l.selection.capabilities.sort_by(|a, b| a.name.cmp(&b.name));
}
fn offer(l: &mut Link, cx: &Cx, id: u32, role: MediaRole) -> Result<MediaChannel, Error> {
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
}
async fn attach(
    l: &mut Link,
    cx: &Cx,
    id: u32,
    role: MediaRole,
) -> (MediaChannel, MediaChannel, AttachedChannel, AttachedChannel) {
    let mut h = offer(l, cx, id, role).unwrap();
    let mut c = l.viewer(cx, &mut h).await;
    let (ha, ca) = l.attached(cx, &mut h, &mut c).await;
    (h, c, ha, ca)
}
fn record(kind: u16, binding: u32, len: usize) -> Vec<u8> {
    let mut b = vec![0; len];
    b[..4].copy_from_slice(b"FRD0");
    b[6..8].copy_from_slice(&kind.to_be_bytes());
    b[12..16].copy_from_slice(&u32::try_from(len - 24).unwrap().to_be_bytes());
    b[16..20].copy_from_slice(&binding.to_be_bytes());
    b
}
#[test]
fn clipboard_requires_control_both_capabilities_and_completed_input() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut l = Link::new(&cx).await;
        enable(&mut l);
        let before = l.h.usage();
        assert!(matches!(
            offer(&mut l, &cx, 8, MediaRole::Clipboard),
            Err(Error::WrongRoute)
        ));
        assert_eq!(before, l.h.usage());
        let _input = attach(&mut l, &cx, 8, MediaRole::Input).await;
        let before = l.h.usage();
        l.selection.role = Role::Observe;
        assert!(matches!(
            offer(&mut l, &cx, 9, MediaRole::Clipboard),
            Err(Error::WrongRoute)
        ));
        l.selection.role = Role::RequestControl;
        for name in [
            attachment::CLIPBOARD_CAPABILITY,
            fr_wire::clipboard::CAPABILITY,
            attachment::INPUT_CAPABILITY,
        ] {
            l.selection
                .capabilities
                .iter_mut()
                .find(|c| c.name == name)
                .unwrap()
                .version = 2;
            assert!(matches!(
                offer(&mut l, &cx, 9, MediaRole::Clipboard),
                Err(Error::WrongRoute)
            ));
            l.selection
                .capabilities
                .iter_mut()
                .find(|c| c.name == name)
                .unwrap()
                .version = 1;
        }
        assert_eq!(before, l.h.usage());
        assert!(!l.h.is_closed());
        attach(&mut l, &cx, 9, MediaRole::Clipboard).await;
    });
}
#[test]
fn clipboard_pair_is_reliable_bulk_bidirectional_and_not_a_second_input_lane() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut l = Link::new(&cx).await;
        enable(&mut l);
        let _input = attach(&mut l, &cx, 8, MediaRole::Input).await;
        let (_, _, h, c) = attach(&mut l, &cx, 9, MediaRole::Clipboard).await;
        for a in [h, c] {
            assert_eq!(a.outbound.messages, Messages::Clipboard);
            assert_eq!(a.inbound.messages, Messages::Clipboard);
            assert_eq!(a.outbound.priority, Priority::Bulk);
            assert_eq!(a.inbound.priority, Priority::Bulk);
            assert!(a.datagram.is_none());
        }
        for kind in 0x50..=0x53 {
            for host in [true, false] {
                let (send, receive) = if host {
                    (h.outbound, c.inbound)
                } else {
                    (c.outbound, h.inbound)
                };
                input::transfer(
                    &mut l,
                    &cx,
                    host,
                    Route::Stream(send),
                    Route::Stream(receive),
                    &record(kind, 9, 128),
                )
                .await;
            }
        }
        let before = l.c.usage();
        for kind in [0x17, 0x30, 0x34, 0x40, 0x42, 0x48, 0x54] {
            assert_eq!(
                l.c.send(
                    &cx,
                    Route::Stream(c.outbound),
                    &record(kind, 9, 128),
                    clock(&cx) + 100_000,
                    || true
                ),
                Err(Error::WrongRoute)
            );
        }
        assert_eq!(before, l.c.usage());
        assert!(matches!(
            offer(&mut l, &cx, 10, MediaRole::Clipboard),
            Err(Error::WrongRoute)
        ));
    });
}
#[test]
fn bulk_clipboard_backpressure_leaves_critical_control_capacity() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut l = Link::new(&cx).await;
        enable(&mut l);
        let _input = attach(&mut l, &cx, 8, MediaRole::Input).await;
        let (_, _, h, _) = attach(&mut l, &cx, 9, MediaRole::Clipboard).await;
        let b = record(0x51, 9, h.outbound.maximum);
        let until = clock(&cx) + 1_000_000;
        let mut sent = 0;
        loop {
            match l.h.send(&cx, Route::Stream(h.outbound), &b, until, || true) {
                Ok(()) => sent += 1,
                Err(Error::Backpressure) => break,
                e => panic!("unexpected admission: {e:?}"),
            }
            assert!(sent <= 128);
        }
        assert!(sent > 0);
        assert!(l.h.usage().retained_send_upper_bound <= Policy::default().retained_send_bytes);
        // Let the small attachment-control batch drain, leaving bulk blocked.
        while l.h.usage().critical_send_records != 0 {
            assert!(clock(&cx) < until);
            l.drive(&cx).await;
        }
        l.h.send(
            &cx,
            Route::Stream(l.hr.outbound),
            &record(0x12, 7, 96),
            until,
            || true,
        )
        .unwrap();
        assert_eq!(l.h.usage().critical_send_records, 1);
    });
}
#[test]
fn abandoned_clipboard_attachment_fences_before_payload_or_native_work() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut l = Link::new(&cx).await;
        enable(&mut l);
        let _input = attach(&mut l, &cx, 8, MediaRole::Input).await;
        let pending = offer(&mut l, &cx, 9, MediaRole::Clipboard).unwrap();
        assert!(!pending.is_complete());
        drop(pending);
        assert_eq!(l.h.tick(&cx, || true), Err(Error::Expired));
        assert!(l.h.is_closed());
        assert_eq!(l.h.usage().retained_send_upper_bound, 0);
    });
}

#[test]
fn typed_clipboard_routes_check_scope_codec_and_original_connection() {
    use fr_core::clipboard::{Binding as Scope, Endpoint, Stamp};
    use fr_transport::quic::clipboard::ClipboardChannel;
    use fr_wire::clipboard::{self as wire, Body, Message};
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut l = Link::new(&cx).await;
        enable(&mut l);
        let _input = attach(&mut l, &cx, 8, MediaRole::Input).await;
        let (h, c, _, _) = attach(&mut l, &cx, 9, MediaRole::Clipboard).await;
        let scope = Scope {
            session: parent().remote_session,
            lease: InputLeaseId::from_raw(123),
        };
        let h = ClipboardChannel::new(&l.h, h, scope).unwrap();
        let c = ClipboardChannel::new(&l.c, c, scope).unwrap();
        assert_eq!(h.limits().max_control_message_bytes(), 8192);
        assert_eq!(h.limits().max_clipboard_item_bytes(), 1_048_576);
        assert_eq!(h.outgoing(), c.incoming());
        assert_eq!(h.incoming(), c.outgoing());
        let mut bytes = [0; wire::BEGIN_BYTES];
        let n = wire::encode(
            Message {
                stamp: Stamp {
                    id: 1,
                    source: Endpoint::Host,
                    sequence: 1,
                },
                body: Body::Begin {
                    total_bytes: 0,
                    chunks: 0,
                },
            },
            h.outgoing(),
            &h.limits(),
            &mut bytes,
        )
        .unwrap();
        let before = l.c.usage();
        assert_eq!(
            c.send(&mut l.c, &cx, &bytes[..n], clock(&cx) + 1_000_000, || true),
            Err(Error::Malformed)
        );
        assert_eq!(l.c.usage(), before);
        let mut foreign = Link::new(&cx).await;
        assert_eq!(
            h.send(
                &mut foreign.h,
                &cx,
                &bytes[..n],
                clock(&cx) + 1_000_000,
                || true
            ),
            Err(Error::WrongRoute)
        );
        assert!(!foreign.h.is_closed());
        h.send(&mut l.h, &cx, &bytes[..n], clock(&cx) + 1_000_000, || true)
            .unwrap();
        let until = clock(&cx) + 1_000_000;
        let mut received = false;
        while !received {
            assert!(clock(&cx) < until);
            l.drive(&cx).await;
            c.dispatch(
                &mut l.c,
                &cx,
                || true,
                |b| {
                    assert_eq!(b, &bytes[..n]);
                    received = true;
                    Ok(Disposition::Blocked)
                },
            )
            .unwrap();
        }
        let mut repeated = false;
        c.dispatch(
            &mut l.c,
            &cx,
            || true,
            |b| {
                assert_eq!(b, &bytes[..n]);
                repeated = true;
                Ok(Disposition::Consumed)
            },
        )
        .unwrap();
        assert!(repeated, "the exact blocked record must remain retained");
        drop(h);
        assert_eq!(l.h.tick(&cx, || true), Err(Error::Expired));
        assert!(l.h.is_closed());
    });
}

#[path = "clipboard/retire.rs"]
mod retire;

#[path = "clipboard/allocation.rs"]
mod allocation;
