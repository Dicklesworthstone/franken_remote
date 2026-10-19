//! Requests discovered after native staging cannot be reset before that same
//! native poll. Exercise the real UDP/TLS owner and its original attachments.
use super::*;
use std::{
    future::{Future, poll_fn},
    pin::pin,
    task::Poll,
};

#[test]
fn retirement_after_staging_fences_before_native_flush() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut l = Link::new(&cx).await;
        enable(&mut l);
        let _input = attach(&mut l, &cx, 8, MediaRole::Input).await;
        let (hp, cp, _, _) = attach(&mut l, &cx, 9, MediaRole::Clipboard).await;
        let h = ClipboardChannel::new(&l.h, hp, scope()).unwrap();
        let c = ClipboardChannel::new(&l.c, cp, scope()).unwrap();
        quiet(&mut l, &cx).await;
        // This complete, valid clipboard record fits in the first native prefix.
        let mut bytes = [0; 128];
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
                    bytes: b"old local copy",
                },
            },
            h.outgoing(),
            &h.limits(),
            &mut bytes,
        )
        .unwrap();
        h.send(&mut l.h, &cx, &bytes[..n], clock(&cx) + 1_000_000, || true)
            .unwrap();
        let mut checks = 0;
        let result =
            l.h.drive(&cx, Duration::ZERO, || {
                checks += 1;
                // drive admission, prefix admission, then the first native poll.
                if checks == 3 {
                    h.request_retirement();
                }
                true
            })
            .await;
        assert_eq!(result, Err(Error::OptionalRetirementDuringIo));
        assert_eq!(checks, 3, "the native operation must not be polled");
        assert!(l.h.is_closed());
        assert_eq!(l.h.usage().retained_send_records, 0);
        assert_eq!(l.h.usage().retained_send_upper_bound, 0);
        for _ in 0..4 {
            l.c.drive(&cx, Duration::ZERO, || true).await.unwrap();
            c.dispatch(
                &mut l.c,
                &cx,
                || true,
                |_| panic!("the invalidated record must not reach the peer"),
            )
            .unwrap();
        }
    });
}

#[test]
fn pending_native_io_rechecks_optional_retirement_before_resuming() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut l = Link::new(&cx).await;
        enable(&mut l);
        let _input = attach(&mut l, &cx, 8, MediaRole::Input).await;
        let (hp, cp, _, _) = attach(&mut l, &cx, 9, MediaRole::Clipboard).await;
        let h = ClipboardChannel::new(&l.h, hp, scope()).unwrap();
        let _c = ClipboardChannel::new(&l.c, cp, scope()).unwrap();
        quiet(&mut l, &cx).await;
        let mut tested = false;
        for _ in 0..16 {
            let mut io = pin!(l.h.drive(&cx, Duration::from_millis(100), || true));
            let pending = poll_fn(|task| Poll::Ready(io.as_mut().poll(task).is_pending())).await;
            if pending {
                h.request_retirement();
                let result = poll_fn(|task| Poll::Ready(io.as_mut().poll(task))).await;
                assert_eq!(result, Poll::Ready(Err(Error::OptionalRetirementDuringIo)));
                tested = true;
                break;
            }
        }
        assert!(tested, "must resume a genuinely pending native I/O future");
        assert!(l.h.is_closed());
        assert_eq!(l.h.usage().retained_send_records, 0);
    });
}

#[test]
fn closing_the_clipboard_owner_inside_native_poll_authorization_fences_io() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut l = Link::new(&cx).await;
        enable(&mut l);
        let _input = attach(&mut l, &cx, 8, MediaRole::Input).await;
        let (hp, cp, _, _) = attach(&mut l, &cx, 9, MediaRole::Clipboard).await;
        let mut h = ClipboardChannel::new(&l.h, hp, scope()).unwrap();
        let _c = ClipboardChannel::new(&l.c, cp, scope()).unwrap();
        quiet(&mut l, &cx).await;
        h.send(&mut l.h, &cx, &chunk(&h), clock(&cx) + 1_000_000, || true)
            .unwrap();
        let mut checks = 0;
        let result =
            l.h.drive(&cx, Duration::ZERO, || {
                checks += 1;
                if checks == 3 {
                    h.close();
                }
                true
            })
            .await;
        assert_eq!(result, Err(Error::Expired));
        assert_eq!(
            checks, 3,
            "actual owner abandonment must precede native I/O"
        );
        assert!(l.h.is_closed());
        assert_eq!(l.h.usage().retained_send_records, 0);
        assert_eq!(l.h.usage().retained_send_upper_bound, 0);
    });
}

#[test]
fn serviced_retirement_and_input_requests_leave_native_io_usable() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut l = Link::new(&cx).await;
        enable(&mut l);
        let (input, _controller_input, hi, ci) = attach(&mut l, &cx, 8, MediaRole::Input).await;
        let (hp, cp, ha, ca) = attach(&mut l, &cx, 9, MediaRole::Clipboard).await;
        let h = ClipboardChannel::new(&l.h, hp, scope()).unwrap();
        let _c = ClipboardChannel::new(&l.c, cp, scope()).unwrap();
        quiet(&mut l, &cx).await;
        h.request_retirement();
        l.h.drive(&cx, Duration::ZERO, || {
            // Input has no optional-retirement authority. The clipboard's
            // request remains set after its ordinary check retires the pair.
            input.request_retirement();
            h.request_retirement();
            true
        })
        .await
        .unwrap();
        assert!(!l.h.has_route(Route::Stream(ha.outbound)));
        assert!(l.h.has_route(Route::Stream(hi.outbound)));
        peer_retired(&mut l, &cx, ca.outbound).await;
        input::transfer(
            &mut l,
            &cx,
            true,
            Route::Stream(hi.outbound),
            Route::Stream(ci.inbound),
            &record(0x17, 8, 96),
        )
        .await;
        let hr = l.hr;
        let cr = l.cr;
        input::transfer(
            &mut l,
            &cx,
            true,
            Route::Stream(hr.outbound),
            Route::Stream(cr.inbound),
            &record(0x12, 7, 96),
        )
        .await;
        assert!(!l.h.is_closed());
        assert!(!l.c.is_closed());
    });
}
