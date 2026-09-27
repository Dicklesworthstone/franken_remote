//! Two optional roles, one authenticated control stream; no test-only transport.
use super::*;

fn enable_files(link: &mut Link) {
    for name in [attachment::FILES_CAPABILITY, fr_wire::files::CAPABILITY] {
        link.selection.capabilities.push(Capability {
            name: name.into(),
            version: 1,
            required: true,
        });
    }
    link.selection
        .capabilities
        .sort_by(|a, b| a.name.cmp(&b.name));
}

#[test]
fn overlapping_clipboard_and_file_exchanges_keep_their_own_tickets_and_routes() {
    run_test!(cx, {
        let mut link = Link::new(&cx).await;
        enable(&mut link);
        enable_files(&mut link);
        let _input = attach(&mut link, &cx, 8, MediaRole::Input).await;
        let original = (link.h.binding(), link.c.binding());
        let mut hc = offer(&mut link, &cx, 9, MediaRole::Clipboard).unwrap();
        let mut hf = offer(&mut link, &cx, 10, MediaRole::Files).unwrap();
        let usage = link.h.usage();
        for role in [
            MediaRole::Clipboard,
            MediaRole::Files,
            MediaRole::Configuration,
        ] {
            assert!(
                offer(&mut link, &cx, 13, role).is_err(),
                "no third pending role"
            );
        }
        assert_eq!(
            link.h.usage(),
            usage,
            "rejected attempts allocate no transport state"
        );
        let mut vc = link.viewer(&cx, &mut hc).await;
        let mut vf = link.viewer(&cx, &mut hf).await;
        let until = clock(&cx) + 1_000_000;
        loop {
            assert!(clock(&cx) < until, "{hc:?} {hf:?} {vc:?} {vf:?}");
            // File ACK deliberately precedes clipboard ACK. The clipboard
            // dispatcher must leave it for the original file owner.
            vf.transmit(&mut link.c, &cx, || true).unwrap();
            vc.transmit(&mut link.c, &cx, || true).unwrap();
            hc.transmit(&mut link.h, &cx, || true).unwrap();
            hf.transmit(&mut link.h, &cx, || true).unwrap();
            link.drive(&cx).await;
            hc.dispatch(&mut link.h, &cx, || true).unwrap();
            hf.dispatch(&mut link.h, &cx, || true).unwrap();
            vf.dispatch(&mut link.c, &cx, || true).unwrap();
            vc.dispatch(&mut link.c, &cx, || true).unwrap();
            let done = [
                hc.finish(&mut link.h, &cx, || true).unwrap(),
                hf.finish(&mut link.h, &cx, || true).unwrap(),
                vc.finish(&mut link.c, &cx, || true).unwrap(),
                vf.finish(&mut link.c, &cx, || true).unwrap(),
            ];
            if done.iter().all(Option::is_some) {
                let [hc, hf, vc, vf] = done.map(Option::unwrap);
                assert_eq!(hc.descriptor, vc.descriptor);
                assert_eq!(hf.descriptor, vf.descriptor);
                assert_eq!(hc.descriptor.role, MediaRole::Clipboard);
                assert_eq!(hf.descriptor.role, MediaRole::Files);
                assert_ne!(hc.inbound, hf.inbound);
                assert_ne!(hc.outbound, hf.outbound);
                break;
            }
        }
        assert!(link.h.is_bound_to(&original.0));
        assert!(link.c.is_bound_to(&original.1));
        hc.retire_clipboard(&mut link.h, &cx).unwrap();
        assert!(hf.completed_on(&link.h).is_ok());
        assert!(!link.h.is_closed());
    });
}

#[test]
fn unknown_binding_ack_is_not_hidden_by_optional_role_multiplexing() {
    run_test!(cx, {
        let mut link = Link::new(&cx).await;
        enable(&mut link);
        enable_files(&mut link);
        let _input = attach(&mut link, &cx, 8, MediaRole::Input).await;
        let mut clipboard = offer(&mut link, &cx, 9, MediaRole::Clipboard).unwrap();
        let _viewer = link.viewer(&cx, &mut clipboard).await;
        let bytes = encoded(Message::Accepted(99), D::ViewerToHost);
        link.c
            .send(
                &cx,
                Route::Stream(link.cr.outbound),
                &bytes,
                clock(&cx) + 500_000,
                || true,
            )
            .unwrap();
        let until = clock(&cx) + 500_000;
        loop {
            assert!(clock(&cx) < until);
            link.drive(&cx).await;
            if let Err(error) = clipboard.dispatch(&mut link.h, &cx, || true) {
                assert_eq!(error, Error::WrongRoute);
                break;
            }
        }
        assert!(link.h.is_closed());
    });
}

#[test]
fn files_reserved_first_are_offered_first_even_when_clipboard_runs_first() {
    run_test!(cx, {
        let mut link = Link::new(&cx).await;
        enable(&mut link);
        enable_files(&mut link);
        let _input = attach(&mut link, &cx, 8, MediaRole::Input).await;
        let mut files = offer(&mut link, &cx, 9, MediaRole::Files).unwrap();
        let mut clipboard = offer(&mut link, &cx, 10, MediaRole::Clipboard).unwrap();
        assert!(
            !clipboard.transmit(&mut link.h, &cx, || true).unwrap(),
            "a later stream pair cannot be advertised before its predecessor"
        );
        let mut vf = link.viewer(&cx, &mut files).await;
        let mut vc = link.viewer(&cx, &mut clipboard).await;
        // The first pair may still be unfinished: only the Binding's queue
        // order is constrained, not the independent ticket exchanges.
        assert!(!files.is_complete());
        assert!(!clipboard.is_complete());
        link.attached(&cx, &mut files, &mut vf).await;
        link.attached(&cx, &mut clipboard, &mut vc).await;
        assert!(files.completed_on(&link.h).is_ok());
        assert!(clipboard.completed_on(&link.h).is_ok());
    });
}
