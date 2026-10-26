//! Real UDP/TLS routing, with synthetic identity/selection and no OS effects.
//! Large envelopes test transport bounds; the typed test also checks that the
//! production clipboard sender uses its full 16 KiB payload without changing
//! the one-MiB item limit or authority scope.
use super::*;
use fr_core::limits::LimitOverrides;

#[path = "bulk/recovery.rs"]
mod recovery;

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
    for (name, version) in [
        (
            attachment::BULK_RECORD_CAPABILITY,
            attachment::BULK_RECORD_VERSION,
        ),
        (attachment::FILES_CAPABILITY, attachment::FILES_VERSION),
        (fr_wire::files::CAPABILITY, fr_wire::files::VERSION),
    ] {
        l.selection.capabilities.push(Capability {
            name: name.into(),
            version,
            required: true,
        });
    }
    l.selection.capabilities.sort_by(|a, b| a.name.cmp(&b.name));
    l
}

fn kind(role: MediaRole) -> u16 {
    match role {
        MediaRole::Clipboard => 0x51,
        MediaRole::Files => 0x72,
        _ => panic!("not a bidirectional bulk role"),
    }
}

#[test]
fn full_window_bulk_records_cross_both_directions_and_replenish_receive_credit() {
    run_test!(cx, {
        for role in [MediaRole::Clipboard, MediaRole::Files] {
            let mut l = link(&cx, Policy::default(), Policy::default()).await;
            let _input = attach(&mut l, &cx, 8, MediaRole::Input).await;
            let (_, _, h, c) = attach(&mut l, &cx, 9, role).await;
            for a in [h, c] {
                assert_eq!(a.byte_allowance, 65_536);
                assert_eq!(a.outbound.maximum, 65_536);
                assert_eq!(a.inbound.maximum, 65_536);
                assert_eq!(a.outbound.priority, Priority::Bulk);
                assert_eq!(a.inbound.priority, Priority::Bulk);
            }
            // More than one stream window must cross, not merely fit once.
            for pass in 0..2 {
                for host in [true, false] {
                    let (send, recv) = if host {
                        (h.outbound, c.inbound)
                    } else {
                        (c.outbound, h.inbound)
                    };
                    let mut bytes = record(kind(role), 9, send.maximum);
                    bytes[24..].fill(0x61 + pass);
                    input::transfer(
                        &mut l,
                        &cx,
                        host,
                        Route::Stream(send),
                        Route::Stream(recv),
                        &bytes,
                    )
                    .await;
                }
            }
            for (q, route) in [(&mut l.h, h.outbound), (&mut l.c, c.outbound)] {
                let before = q.usage();
                assert_eq!(
                    q.send(
                        &cx,
                        Route::Stream(route),
                        &record(kind(role), 9, route.maximum + 1),
                        clock(&cx) + 1_000_000,
                        || true
                    ),
                    Err(Error::TooLarge)
                );
                assert_eq!(q.usage(), before);
                assert!(!q.is_closed());
            }
        }
    });
}

#[test]
fn bulk_allowance_is_the_smallest_negotiated_stream_and_retention_limit() {
    run_test!(cx, {
        for (record_bytes, stream_window, retained_send_bytes, expected) in [
            (20_000, 65_536, 131_072, 20_000),
            (65_536, 24_000, 131_072, 24_000),
            (65_536, 65_536, 32_000, 32_000),
            (65_536, 65_536, 4096, 4096),
        ] {
            let policy = Policy {
                stream_window,
                retained_send_bytes,
                ..Policy::default()
            };
            let mut l = link(&cx, policy, policy).await;
            l.selection.limits = ProtocolLimits::with_overrides(LimitOverrides {
                max_control_message_bytes: Some(record_bytes),
                ..Default::default()
            })
            .unwrap();
            let _input = attach(&mut l, &cx, 8, MediaRole::Input).await;
            let clipboard = attach(&mut l, &cx, 9, MediaRole::Clipboard).await;
            let files = attach(&mut l, &cx, 10, MediaRole::Files).await;
            for a in [clipboard.2, clipboard.3, files.2, files.3] {
                assert_eq!(a.byte_allowance, expected);
                assert_eq!(u64::try_from(a.outbound.maximum).unwrap(), expected);
                assert_eq!(u64::try_from(a.inbound.maximum).unwrap(), expected);
            }
        }
    });
}

#[test]
fn critical_record_budget_does_not_limit_bulk_or_grow_with_it() {
    run_test!(cx, {
        for critical_send_bytes in [4096, 8192, 16_384] {
            let policy = Policy {
                critical_send_bytes,
                ..Policy::default()
            };
            let mut l = link(&cx, policy, policy).await;
            let configuration = attach(&mut l, &cx, 8, MediaRole::Configuration).await;
            assert_eq!(configuration.2.outbound.maximum, critical_send_bytes);
            assert_eq!(configuration.2.outbound.priority, Priority::Critical);
            let input = attach(&mut l, &cx, 9, MediaRole::Input).await;
            assert_eq!(
                input.2.outbound.maximum,
                critical_send_bytes.min(fr_wire::input::MAX_INPUT_RECORD_BYTES)
            );
            let clipboard = attach(&mut l, &cx, 10, MediaRole::Clipboard).await;
            assert_eq!(clipboard.2.outbound.maximum, 65_536);
            let before = l.h.usage();
            assert_eq!(
                l.h.send(
                    &cx,
                    Route::Stream(configuration.2.outbound),
                    &record(0x38, 8, critical_send_bytes + 1),
                    clock(&cx) + 1_000_000,
                    || true
                ),
                Err(Error::TooLarge)
            );
            assert_eq!(l.h.usage(), before);
        }
    });
}

#[test]
fn peer_with_smaller_bulk_budget_refuses_the_grant_before_promotion() {
    run_test!(cx, {
        for role in [MediaRole::Clipboard, MediaRole::Files] {
            let mut l = link(
                &cx,
                Policy::default(),
                Policy {
                    retained_send_bytes: 8192,
                    ..Policy::default()
                },
            )
            .await;
            let _input = attach(&mut l, &cx, 8, MediaRole::Input).await;
            let mut h = offer(&mut l, &cx, 9, role).unwrap();
            let mut c = l.viewer(&cx, &mut h).await;
            let until = clock(&cx) + 1_500_000;
            loop {
                assert!(clock(&cx) < until);
                h.transmit(&mut l.h, &cx, || true).unwrap();
                c.transmit(&mut l.c, &cx, || true).unwrap();
                l.drive(&cx).await;
                h.dispatch(&mut l.h, &cx, || true).unwrap();
                match c.dispatch(&mut l.c, &cx, || true) {
                    Err(Error::TooLarge) => break,
                    Ok(()) => (),
                    other => panic!("unexpected grant result: {other:?}"),
                }
                assert!(!c.is_complete());
            }
            assert!(!c.is_complete());
            assert!(l.c.is_closed());
            assert_eq!(l.c.usage().retained_send_upper_bound, 0);
        }
    });
}

#[test]
fn typed_clipboard_sender_delivers_a_full_sized_chunk_without_relaxing_item_limits() {
    use fr_core::clipboard::{Binding as Scope, Endpoint, MAX_CHUNK_BYTES, Stamp};
    use fr_transport::quic::clipboard::ClipboardChannel;
    use fr_wire::clipboard::{self as wire, Body};
    run_test!(cx, {
        let mut l = link(&cx, Policy::default(), Policy::default()).await;
        let _input = attach(&mut l, &cx, 8, MediaRole::Input).await;
        let (hp, cp, _, _) = attach(&mut l, &cx, 9, MediaRole::Clipboard).await;
        let scope = Scope {
            session: parent().remote_session,
            lease: InputLeaseId::from_raw(123),
        };
        let h = ClipboardChannel::new(&l.h, hp, scope).unwrap();
        let c = ClipboardChannel::new(&l.c, cp, scope).unwrap();
        let text = "界".repeat(MAX_CHUNK_BYTES / 3 + 1);
        for host in [true, false] {
            let (send, recv, source) = if host {
                (&h, &c, Endpoint::Host)
            } else {
                (&c, &h, Endpoint::Controller)
            };
            assert_eq!(send.limits().max_clipboard_item_bytes(), 1_048_576);
            let mut sender = wire::send::Sender::new(
                &text,
                Stamp {
                    id: 1,
                    source,
                    sequence: 1,
                },
                send.outgoing(),
                send.limits(),
            )
            .unwrap();
            let mut bytes = vec![0; send.limits().max_control_message_bytes() as usize];
            let mut payload = Vec::new();
            let mut chunks = 0;
            while let Some(n) = sender.encode_next(&mut bytes).unwrap() {
                let until = clock(&cx) + 1_000_000;
                loop {
                    let q = if host { &mut l.h } else { &mut l.c };
                    match send.send(q, &cx, &bytes[..n], until, || true) {
                        Ok(()) => break,
                        Err(Error::Backpressure) => l.drive(&cx).await,
                        other => panic!("unexpected send: {other:?}"),
                    }
                    assert!(clock(&cx) < until);
                }
                sender.accepted();
                let mut received = false;
                while !received {
                    assert!(clock(&cx) < until);
                    l.drive(&cx).await;
                    let q = if host { &mut l.c } else { &mut l.h };
                    recv.dispatch(
                        q,
                        &cx,
                        || true,
                        |b| {
                            assert_eq!(b, &bytes[..n]);
                            match wire::decode(b, recv.incoming(), &recv.limits())
                                .unwrap()
                                .body
                            {
                                Body::Begin { chunks, .. } => assert_eq!(chunks, 2),
                                Body::Chunk { bytes, .. } => {
                                    if chunks == 0 {
                                        assert_eq!(bytes.len(), MAX_CHUNK_BYTES);
                                    }
                                    payload.extend_from_slice(bytes);
                                    chunks += 1;
                                }
                                Body::Commit { .. } => assert_eq!(payload, text.as_bytes()),
                                Body::Cancel(_) => panic!("unexpected cancellation"),
                            }
                            received = true;
                            Ok(Disposition::Consumed)
                        },
                    )
                    .unwrap();
                }
            }
            assert_eq!(chunks, 2);
            assert_eq!(payload, text.as_bytes());
        }
    });
}

#[test]
fn legacy_selection_retains_small_bulk_records_without_losing_the_connection() {
    run_test!(cx, {
        for role in [MediaRole::Clipboard, MediaRole::Files] {
            let mut l = link(&cx, Policy::default(), Policy::default()).await;
            l.selection
                .capabilities
                .retain(|c| c.name != attachment::BULK_RECORD_CAPABILITY);
            let _input = attach(&mut l, &cx, 8, MediaRole::Input).await;
            let (_, _, h, c) = attach(&mut l, &cx, 9, role).await;
            assert_eq!(h.byte_allowance, 8192);
            assert_eq!(c.byte_allowance, 8192);
            let bytes = record(kind(role), 9, 8192);
            input::transfer(
                &mut l,
                &cx,
                true,
                Route::Stream(h.outbound),
                Route::Stream(c.inbound),
                &bytes,
            )
            .await;
            assert_eq!(
                l.h.send(
                    &cx,
                    Route::Stream(h.outbound),
                    &record(kind(role), 9, 8193),
                    clock(&cx) + 1_000_000,
                    || true
                ),
                Err(Error::TooLarge)
            );
            assert!(!l.h.is_closed());
            assert!(!l.c.is_closed());
        }
    });
}
