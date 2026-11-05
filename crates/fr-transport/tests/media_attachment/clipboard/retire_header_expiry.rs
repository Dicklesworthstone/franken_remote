//! Real TLS stream credit deliberately splits the next FRD0 header. No private
//! framer mutation or fabricated peer reset supplies the expiry evidence.
use super::*;

async fn narrow_link(cx: &Cx) -> Link {
    let (c, h) = support::native_pair_with_windows(cx, "localhost", ALPN, 512, 524_288).await;
    let (mut c, mut h) = (c.unwrap(), h.unwrap());
    let outbound = StreamRoute {
        stream: h.connection_mut().open_uni_stream(cx).unwrap(),
        binding: 7,
        messages: Messages::SessionControl,
        priority: Priority::Critical,
        outbound: true,
        maximum: 512,
    };
    let inbound = StreamRoute {
        stream: c.connection_mut().open_uni_stream(cx).unwrap(),
        outbound: false,
        ..outbound
    };
    let hr = ControlRoutes { outbound, inbound };
    let cr = ControlRoutes {
        outbound: StreamRoute {
            outbound: true,
            ..inbound
        },
        inbound: StreamRoute {
            outbound: false,
            ..outbound
        },
    };
    let policy = Policy {
        stream_window: 512,
        critical_send_records: 1,
        ..Policy::default()
    };
    Link {
        h: QuicRecords::new(h, cx, &[hr.outbound, hr.inbound], &[], policy).unwrap(),
        c: QuicRecords::new(c, cx, &[cr.outbound, cr.inbound], &[], policy).unwrap(),
        hr,
        cr,
        selection: selection(),
    }
}

fn small_chunk(channel: &ClipboardChannel) -> Vec<u8> {
    let mut bytes = vec![0; 500];
    let payload = vec![0x61; 500 - wire::CHUNK_OVERHEAD];
    let n = wire::encode(
        Message {
            stamp: Stamp {
                id: 1,
                source: Endpoint::Host,
                sequence: 1,
            },
            body: Body::Chunk {
                index: 0,
                offset: 0,
                bytes: &payload,
            },
        },
        channel.outgoing(),
        &channel.limits(),
        &mut bytes,
    )
    .unwrap();
    assert_eq!(n, 500);
    bytes
}

#[test]
fn a_stalled_header_retires_the_original_clipboard_pair_without_renewing_its_deadline() {
    runtime().block_on(Box::pin(async {
        let cx = Cx::current().unwrap();
        let mut l = Box::pin(narrow_link(&cx)).await;
        enable(&mut l);
        let (_ih, _ic, hi, ci) = attach(&mut l, &cx, 8, MediaRole::Input).await;
        let (hp, cp, ha, _) = attach(&mut l, &cx, 9, MediaRole::Clipboard).await;
        let h = ClipboardChannel::new(&l.h, hp, scope()).unwrap();
        let c = ClipboardChannel::new(&l.c, cp, scope()).unwrap();
        quiet(&mut l, &cx).await;
        let capacity = l.c.usage().framed_capacity;
        let bytes = small_chunk(&h);
        for _ in 0..2 {
            h.send(&mut l.h, &cx, &bytes, clock(&cx) + 4_000_000, || true)
                .unwrap();
        }
        // The actual peer stream has 512 bytes of credit: one complete
        // 500-byte record followed by just 12 of the next 24-byte header.
        // Read no application bytes until both native prefixes are delivered.
        l.h.drive(&cx, Duration::ZERO, || true).await.unwrap();
        for _ in 0..3 {
            l.c.drive(&cx, Duration::from_millis(1), || true)
                .await
                .unwrap();
        }
        let mut complete = 0;
        for _ in 0..3 {
            c.dispatch(
                &mut l.c,
                &cx,
                || true,
                |received| {
                    assert_eq!(received, bytes);
                    complete += 1;
                    Ok(Disposition::Consumed)
                },
            )
            .unwrap();
        }
        assert_eq!(complete, 1);
        assert!(!h.send_drained(&l.h).unwrap());
        expire_partial(&mut l, &cx, true, &c).await;
        assert_eq!(l.c.usage().framed_capacity, capacity);
        let until = clock(&cx) + 1_000_000;
        while l.h.has_route(Route::Stream(ha.outbound)) {
            assert!(clock(&cx) < until);
            l.drive(&cx).await;
        }
        assert!(h.receive_expired(&l.h));
        survives(&mut l, &cx, hi, ci).await;
        assert!(matches!(
            offer(&mut l, &cx, 10, MediaRole::Clipboard),
            Err(Error::WrongRoute)
        ));
    }));
}
