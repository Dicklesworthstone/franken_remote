//! Public binding allocation is not ticket generation or admission.
use super::*;

#[test]
fn next_binding_is_non_consuming_and_cannot_wrap_at_exhaustion() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut l = Link::new(&cx).await;
        let before = l.h.usage();
        assert_eq!(l.h.next_channel_binding(), Ok(8));
        assert_eq!(l.h.next_channel_binding(), Ok(8));
        assert_eq!(l.h.usage(), before);
        let mut h = l.offer(&cx, u32::MAX, 90);
        let mut c = l.viewer(&cx, &mut h).await;
        l.attached(&cx, &mut h, &mut c).await;
        for q in [&l.h, &l.c] {
            assert_eq!(q.next_channel_binding(), Err(Error::WrongRoute));
            assert!(
                !q.is_closed(),
                "allocation failure must not close the session"
            );
        }
    });
}

#[test]
fn next_binding_preserves_retired_clipboard_tombstones_on_both_peers() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut l = Link::new(&cx).await;
        enable(&mut l);
        let _input = attach(&mut l, &cx, 8, attachment::MediaRole::Input).await;
        assert_eq!(l.h.next_channel_binding(), Ok(9));
        let (mut h, c, _, ca) = attach(&mut l, &cx, 400, attachment::MediaRole::Clipboard).await;
        h.retire_clipboard(&mut l.h, &cx).unwrap();
        let until = clock(&cx) + 1_000_000;
        while l.c.has_route(Route::Stream(ca.inbound)) {
            assert!(clock(&cx) < until);
            l.drive(&cx).await;
        }
        assert_eq!(l.h.next_channel_binding(), Ok(401));
        assert_eq!(l.c.next_channel_binding(), Ok(401));
        // The optional retired pair stays consumed; only a different role may
        // use the fresh ID. Allocation does not reopen clipboard or input.
        assert!(matches!(
            offer(&mut l, &cx, 401, attachment::MediaRole::Clipboard),
            Err(Error::WrongRoute)
        ));
        let mut next = l.offer(&cx, 401, 91);
        let mut peer = l.viewer(&cx, &mut next).await;
        l.attached(&cx, &mut next, &mut peer).await;
        assert_eq!(l.h.next_channel_binding(), Ok(402));
        assert_eq!(l.c.next_channel_binding(), Ok(402));
        drop(c);
    });
}
