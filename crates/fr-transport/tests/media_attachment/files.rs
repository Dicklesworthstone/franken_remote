//! Real native UDP/TLS file attachment; identity/selection are explicit fixtures.
use super::*;
use fr_transport::quic::files::FilesChannel;
use fr_wire::{
    attachment::MediaRole,
    files::{self as wire, Body, Message as FileMessage},
};

fn enable(l: &mut Link) {
    l.selection.role = Role::RequestControl;
    for (name, version) in [
        (attachment::INPUT_CAPABILITY, attachment::INPUT_VERSION),
        (attachment::FILES_CAPABILITY, attachment::FILES_VERSION),
        (wire::CAPABILITY, wire::VERSION),
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
fn encoded_file(channel: &FilesChannel) -> Vec<u8> {
    let mut bytes = vec![0; channel.limits().record_bytes()];
    let n = wire::encode(
        FileMessage {
            id: 1,
            body: Body::Offer {
                profile: 1,
                atp: b"route-test-only",
            },
        },
        channel.outgoing(),
        channel.limits(),
        &mut bytes,
    )
    .unwrap();
    bytes.truncate(n);
    bytes
}

#[test]
fn files_require_control_positive_profiles_and_original_completed_input() {
    run_test!(cx, {
        let mut l = Link::new(&cx).await;
        enable(&mut l);
        let before = l.h.usage();
        assert!(matches!(
            offer(&mut l, &cx, 8, MediaRole::Files),
            Err(Error::WrongRoute)
        ));
        assert_eq!(before, l.h.usage());
        let _input = attach(&mut l, &cx, 8, MediaRole::Input).await;
        let before = l.h.usage();
        l.selection.role = Role::Observe;
        assert!(matches!(
            offer(&mut l, &cx, 9, MediaRole::Files),
            Err(Error::WrongRoute)
        ));
        l.selection.role = Role::RequestControl;
        for name in [
            attachment::FILES_CAPABILITY,
            attachment::INPUT_CAPABILITY,
            wire::CAPABILITY,
        ] {
            l.selection
                .capabilities
                .iter_mut()
                .find(|c| c.name == name)
                .unwrap()
                .version = 2;
            assert!(matches!(
                offer(&mut l, &cx, 9, MediaRole::Files),
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
        attach(&mut l, &cx, 9, MediaRole::Files).await;
        assert!(matches!(
            offer(&mut l, &cx, 10, MediaRole::Files),
            Err(Error::WrongRoute)
        ));
    });
}

#[test]
fn files_use_a_separate_reliable_bulk_family_never_an_input_parser_exception() {
    run_test!(cx, {
        let mut l = Link::new(&cx).await;
        enable(&mut l);
        let _input = attach(&mut l, &cx, 8, MediaRole::Input).await;
        let (_, _, h, c) = attach(&mut l, &cx, 9, MediaRole::Files).await;
        assert_eq!(h.descriptor.role.primary_direction(), 2);
        for a in [h, c] {
            assert_eq!(a.outbound.messages, Messages::Files);
            assert_eq!(a.inbound.messages, Messages::Files);
            assert_eq!(a.outbound.priority, Priority::Bulk);
            assert_eq!(a.inbound.priority, Priority::Bulk);
            assert!(a.datagram.is_none());
        }
        // Raw route classification only; the FilesChannel codec checks bodies.
        for kind in 0x70..=0x74 {
            input::transfer(
                &mut l,
                &cx,
                false,
                Route::Stream(c.outbound),
                Route::Stream(h.inbound),
                &record(kind, 9, 128),
            )
            .await;
        }
        let before = l.c.usage();
        for kind in [0x17, 0x30, 0x40, 0x48, 0x51, 0x75] {
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
    });
}

#[test]
fn typed_file_channel_retains_blocked_records_and_rejects_foreign_connection_and_wrong_role() {
    run_test!(cx, {
        let mut l = Link::new(&cx).await;
        enable(&mut l);
        let _input = attach(&mut l, &cx, 8, MediaRole::Input).await;
        let (hp, cp, _, _) = attach(&mut l, &cx, 9, MediaRole::Files).await;
        let h = FilesChannel::new(&l.h, hp, InputLeaseId::from_raw(123), 456).unwrap();
        let c = FilesChannel::new(&l.c, cp, InputLeaseId::from_raw(123), 456).unwrap();
        assert_eq!(h.incoming(), c.outgoing());
        assert_eq!(h.outgoing(), c.incoming());
        let bytes = encoded_file(&c);
        let mut foreign = Link::new(&cx).await;
        assert_eq!(
            c.send(&mut foreign.c, &cx, &bytes, clock(&cx) + 1_000_000, || true),
            Err(Error::WrongRoute)
        );
        assert!(!foreign.c.is_closed());
        assert_eq!(
            h.send(&mut l.h, &cx, &bytes, clock(&cx) + 1_000_000, || true),
            Err(Error::Malformed)
        );
        c.send(&mut l.c, &cx, &bytes, clock(&cx) + 1_000_000, || true)
            .unwrap();
        let until = clock(&cx) + 1_000_000;
        let mut got = false;
        while !got {
            assert!(clock(&cx) < until);
            l.drive(&cx).await;
            h.dispatch(
                &mut l.h,
                &cx,
                || true,
                |b| {
                    assert_eq!(b, bytes);
                    got = true;
                    Ok(Disposition::Blocked)
                },
            )
            .unwrap();
        }
        let mut again = false;
        h.dispatch(
            &mut l.h,
            &cx,
            || true,
            |b| {
                assert_eq!(b, bytes);
                again = true;
                Ok(Disposition::Consumed)
            },
        )
        .unwrap();
        assert!(again);
    });
}

#[test]
fn retiring_files_keeps_control_input_and_clipboard_usable_and_consumes_file_ids() {
    run_test!(cx, {
        let mut l = Link::new(&cx).await;
        enable(&mut l);
        for (name, version) in [
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
        let (_, _, hi, ci) = attach(&mut l, &cx, 8, MediaRole::Input).await;
        let (_, _, hc, cc) = attach(&mut l, &cx, 9, MediaRole::Clipboard).await;
        let (hp, cp, _, _) = attach(&mut l, &cx, 10, MediaRole::Files).await;
        let mut h = FilesChannel::new(&l.h, hp, InputLeaseId::from_raw(123), 456).unwrap();
        let mut c = FilesChannel::new(&l.c, cp, InputLeaseId::from_raw(123), 456).unwrap();
        let mut foreign = Link::new(&cx).await;
        assert_eq!(h.retire(&mut foreign.h, &cx), Err(Error::WrongRoute));
        assert!(!foreign.h.is_closed());
        h.retire(&mut l.h, &cx).unwrap();
        h.retire(&mut l.h, &cx).unwrap();
        let until = clock(&cx) + 1_000_000;
        while c.check(&l.c).is_ok() {
            assert!(clock(&cx) < until);
            l.drive(&cx).await;
        }
        c.retire(&mut l.c, &cx).unwrap();
        assert!(matches!(
            offer(&mut l, &cx, 11, MediaRole::Files),
            Err(Error::WrongRoute)
        ));
        assert!(!l.h.is_closed() && !l.c.is_closed());
        input::transfer(
            &mut l,
            &cx,
            false,
            Route::Stream(ci.outbound),
            Route::Stream(hi.inbound),
            &record(0x40, 8, 128),
        )
        .await;
        input::transfer(
            &mut l,
            &cx,
            true,
            Route::Stream(hc.outbound),
            Route::Stream(cc.inbound),
            &record(0x51, 9, 128),
        )
        .await;
        let (outgoing, incoming) = (l.hr.outbound, l.cr.inbound);
        input::transfer(
            &mut l,
            &cx,
            true,
            Route::Stream(outgoing),
            Route::Stream(incoming),
            &record(0x12, 7, 128),
        )
        .await;
    });
}

#[test]
fn saturated_file_bulk_storage_preserves_critical_control_credit() {
    run_test!(cx, {
        let mut l = Link::new(&cx).await;
        enable(&mut l);
        let _input = attach(&mut l, &cx, 8, MediaRole::Input).await;
        let (_, _, _, c) = attach(&mut l, &cx, 9, MediaRole::Files).await;
        let bytes = record(0x72, 9, c.outbound.maximum);
        let until = clock(&cx) + 1_000_000;
        let mut queued = 0;
        loop {
            match l
                .c
                .send(&cx, Route::Stream(c.outbound), &bytes, until, || true)
            {
                Ok(()) => queued += 1,
                Err(Error::Backpressure) => break,
                other => panic!("unexpected admission {other:?}"),
            }
            assert!(queued <= 128);
        }
        assert!(queued > 0);
        assert!(l.c.usage().retained_send_upper_bound <= Policy::default().retained_send_bytes);
        while l.c.usage().critical_send_records != 0 {
            assert!(clock(&cx) < until);
            l.drive(&cx).await;
        }
        l.c.send(
            &cx,
            Route::Stream(l.cr.outbound),
            &record(0x12, 7, 128),
            until,
            || true,
        )
        .unwrap();
        assert_eq!(l.c.usage().critical_send_records, 1);
    });
}

fn record(kind: u16, binding: u32, len: usize) -> Vec<u8> {
    let mut b = vec![0; len];
    b[..4].copy_from_slice(b"FRD0");
    b[6..8].copy_from_slice(&kind.to_be_bytes());
    b[12..16].copy_from_slice(&u32::try_from(len - 24).unwrap().to_be_bytes());
    b[16..20].copy_from_slice(&binding.to_be_bytes());
    b
}
