//! Real UDP/TLS receive-credit regression. The reader remains parked until a
//! bounded flight has been acknowledged; no OS clipboard effect is claimed.
use super::*;
use fr_wire::attachment::credit;

fn endpoint(l: &mut Link, host: bool) -> &mut QuicRecords {
    if host { &mut l.h } else { &mut l.c }
}

fn bulk_records(q: &QuicRecords) -> usize {
    let usage = q.usage();
    usage.retained_send_records - usage.critical_send_records
}

async fn drained(l: &mut Link, cx: &Cx, host: bool, until: u64) {
    while bulk_records(endpoint(l, host)) != 0 {
        assert!(
            clock(cx) < until,
            "the clipboard flight must be acknowledged before any application read"
        );
        l.drive(cx).await;
    }
}

fn payload(route: StreamRoute, len: usize, tag: u8) -> Vec<u8> {
    let mut bytes = record(0x51, route.binding, len);
    bytes[24..].fill(tag);
    bytes
}

async fn warm(l: &mut Link, cx: &Cx, host: bool, send: StreamRoute, recv: StreamRoute) {
    // Consume the fixture's entire initial 64 KiB credit before measuring the
    // next flight. Initial handshake credit must not mask a one-record window.
    for _ in 0..65_536 / send.maximum {
        input::transfer(
            l,
            cx,
            host,
            Route::Stream(send),
            Route::Stream(recv),
            &payload(send, send.maximum, 0xa0),
        )
        .await;
    }
    drained(l, cx, host, clock(cx) + 1_000_000).await;
}

fn take_without_io(q: &mut QuicRecords, cx: &Cx, route: StreamRoute, expected: &[Vec<u8>]) {
    let mut count = 0;
    // Native reads may expose reassembled fragments separately. Only drain
    // already received bytes: no drive, ACK or MAX_STREAM_DATA can reach the
    // sender while this loop runs.
    for _ in 0..256 {
        q.receive_ready(
            cx,
            || true,
            |r| r == Route::Stream(route),
            |_, bytes| {
                assert_eq!(bytes, expected[count]);
                count += 1;
                Ok(Disposition::Consumed)
            },
        )
        .unwrap();
        if count == expected.len() {
            break;
        }
    }
    assert_eq!(count, expected.len());
    assert_eq!(
        q.receive_ready(
            cx,
            || true,
            |r| r == Route::Stream(route),
            |_, _| panic!("bytes beyond the local window arrived before credit was returned")
        ),
        Ok(0)
    );
}

async fn flight(
    l: &mut Link,
    cx: &Cx,
    host: bool,
    send: StreamRoute,
    recv: StreamRoute,
    window: u64,
) {
    let count = usize::try_from(window).unwrap() / send.maximum;
    // Native credit advances only after window/16 bytes are read. Fragmented
    // reads may leave that much credit unadvertised, so reserve the same bounded
    // headroom. Eight legacy records still fit in a 64 KiB window; a one-record
    // window cannot acknowledge this flight while its application is parked.
    let bytes_per_record = send.maximum - send.maximum / 16;
    let expected: Vec<_> = (0..count)
        .map(|index| payload(send, bytes_per_record, u8::try_from(index).unwrap()))
        .collect();
    let until = clock(cx) + 1_000_000;
    for bytes in &expected {
        endpoint(l, host)
            .send(cx, Route::Stream(send), bytes, until, || true)
            .unwrap();
    }
    assert_eq!(bulk_records(endpoint(l, host)), count);
    drained(l, cx, host, until).await;

    // The configured local window is still a cap. The next full record exceeds
    // the reserved headroom: native ownership may accept a prefix, but cannot
    // stage the complete record even after repeated I/O without a reader.
    let next = payload(send, send.maximum, 0xf0);
    let until = clock(cx) + 1_000_000;
    endpoint(l, host)
        .send(cx, Route::Stream(send), &next, until, || true)
        .unwrap();
    for _ in 0..16 {
        l.drive(cx).await;
        assert!(!endpoint(l, host).send_staged(send).unwrap());
    }
    assert_eq!(bulk_records(endpoint(l, host)), 1);
    take_without_io(endpoint(l, !host), cx, recv, &expected);

    // Reading the first flight returns credit on the same stream. No new
    // attachment, record admission, lifetime or application retry is involved.
    let mut received = false;
    while !received {
        assert!(clock(cx) < until);
        l.drive(cx).await;
        endpoint(l, !host)
            .receive_ready(
                cx,
                || true,
                |r| r == Route::Stream(recv),
                |_, bytes| {
                    assert_eq!(bytes, next);
                    received = true;
                    Ok(Disposition::Consumed)
                },
            )
            .unwrap();
    }
    drained(l, cx, host, until).await;
}

fn record_ceiling(l: &mut Link, cx: &Cx, host: bool, send: StreamRoute) {
    let q = endpoint(l, host);
    let before = q.usage();
    assert_eq!(
        q.send(
            cx,
            Route::Stream(send),
            &record(0x51, send.binding, send.maximum + 1),
            clock(cx) + 1_000_000,
            || true
        ),
        Err(Error::TooLarge)
    );
    assert_eq!(q.usage(), before);
    assert!(!q.is_closed());
}

#[test]
fn clipboard_window_pipelines_legacy_records_without_increasing_either_record_ceiling() {
    run_test!(cx, {
        for bulk in [false, true] {
            let mut l = link(&cx, Policy::default(), Policy::default()).await;
            if !bulk {
                l.selection
                    .capabilities
                    .retain(|cap| cap.name != attachment::BULK_RECORD_CAPABILITY);
            }
            let _input = attach(&mut l, &cx, 8, MediaRole::Input).await;
            let (_, _, h, c) = attach(&mut l, &cx, 9, MediaRole::Clipboard).await;
            for a in [h, c] {
                assert_eq!(a.byte_allowance, if bulk { 65_536 } else { 8192 });
                assert_eq!(a.outbound.maximum as u64, a.byte_allowance);
                assert_eq!(a.inbound.maximum as u64, a.byte_allowance);
            }
            for host in [true, false] {
                let (send, recv) = if host {
                    (h.outbound, c.inbound)
                } else {
                    (c.outbound, h.inbound)
                };
                record_ceiling(&mut l, &cx, host, send);
                warm(&mut l, &cx, host, send, recv).await;
                flight(&mut l, &cx, host, send, recv, 65_536).await;
            }
        }
    });
}

#[test]
fn narrowed_clipboard_grant_keeps_each_endpoints_independent_receive_window() {
    run_test!(cx, {
        let small = Policy {
            stream_window: 8192,
            ..Policy::default()
        };
        for (hp, cp) in [(small, Policy::default()), (Policy::default(), small)] {
            let mut l = link(&cx, hp, cp).await;
            l.selection.capabilities.push(Capability {
                name: credit::CAPABILITY.into(),
                version: credit::VERSION,
                required: true,
            });
            l.selection.capabilities.sort_by(|a, b| a.name.cmp(&b.name));
            let _input = attach(&mut l, &cx, 8, MediaRole::Input).await;
            let (_, _, h, c) = attach(&mut l, &cx, 9, MediaRole::Clipboard).await;
            for a in [h, c] {
                assert_eq!(a.byte_allowance, 8192);
                assert_eq!(a.outbound.maximum, 8192);
                assert_eq!(a.inbound.maximum, 8192);
            }
            for host in [true, false] {
                let (send, recv, window) = if host {
                    (h.outbound, c.inbound, cp.stream_window)
                } else {
                    (c.outbound, h.inbound, hp.stream_window)
                };
                record_ceiling(&mut l, &cx, host, send);
                warm(&mut l, &cx, host, send, recv).await;
                flight(&mut l, &cx, host, send, recv, window).await;
            }
        }
    });
}
