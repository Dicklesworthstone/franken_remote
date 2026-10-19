//! A selected download starts at the host's final authenticated handoff.
use super::*;

fn download(l: &mut Link) {
    enable(l);
    l.selection.capabilities.push(Capability {
        name: wire::DOWNLOAD_CAPABILITY.into(),
        version: wire::DOWNLOAD_VERSION,
        required: true,
    });
    l.selection.capabilities.sort_by(|a, b| a.name.cmp(&b.name));
}

async fn host_finished_first(
    l: &mut Link,
    cx: &Cx,
) -> (MediaChannel, MediaChannel, AttachedChannel) {
    let mut host = offer(l, cx, 9, MediaRole::Files).unwrap();
    let mut viewer = l.viewer(cx, &mut host).await;
    let until = clock(cx) + 1_000_000;
    loop {
        assert!(clock(cx) < until, "host attachment did not complete");
        host.transmit(&mut l.h, cx, || true).unwrap();
        viewer.transmit(&mut l.c, cx, || true).unwrap();
        l.drive(cx).await;
        host.dispatch(&mut l.h, cx, || true).unwrap();
        if let Some(routes) = host.finish(&mut l.h, cx, || true).unwrap() {
            assert!(!viewer.is_complete());
            return (host, viewer, routes);
        }
        viewer.dispatch(&mut l.c, cx, || true).unwrap();
        assert!(viewer.finish(&mut l.c, cx, || true).unwrap().is_none());
    }
}

#[test]
fn selected_download_preserves_native_file_bytes_before_viewer_promotion() {
    run_test!(cx, {
        let mut l = Link::new(&cx).await;
        download(&mut l);
        let (_hi, _ci, hi, ci) = attach(&mut l, &cx, 8, MediaRole::Input).await;
        let original = l.c.binding();
        let (host, mut viewer, routes) = host_finished_first(&mut l, &cx).await;
        let host = FilesChannel::new(&l.h, host, InputLeaseId::from_raw(123), 456).unwrap();
        assert_eq!(host.outgoing().direction, wire::Direction::ToController);
        let bytes = encoded_file(&host);
        let until = clock(&cx) + 1_000_000;
        host.send(&mut l.h, &cx, &bytes, until, || true).unwrap();
        while !host.send_drained(&l.h).unwrap() {
            assert!(clock(&cx) < until, "download prefix did not drain");
            l.drive(&cx).await;
        }
        assert!(!viewer.is_complete());
        // Direction belongs to the original selection, not caller metadata
        // that can change between handshake completion and typed construction.
        l.selection
            .capabilities
            .retain(|c| c.name != wire::DOWNLOAD_CAPABILITY);
        viewer.dispatch(&mut l.c, &cx, || true).unwrap();
        let promoted = viewer.finish(&mut l.c, &cx, || true).unwrap().unwrap();
        assert_eq!(promoted.inbound.stream, routes.outbound.stream);
        let viewer = FilesChannel::new(&l.c, viewer, InputLeaseId::from_raw(123), 456).unwrap();
        assert_eq!(viewer.incoming().direction, wire::Direction::ToController);
        let mut got = false;
        viewer
            .dispatch(
                &mut l.c,
                &cx,
                || true,
                |record| {
                    assert_eq!(record, bytes);
                    got = true;
                    Ok(Disposition::Consumed)
                },
            )
            .unwrap();
        assert!(got);
        assert!(l.c.is_bound_to(&original));
        input::transfer(
            &mut l,
            &cx,
            false,
            Route::Stream(ci.outbound),
            Route::Stream(hi.inbound),
            &record(0x40, 8, 128),
        )
        .await;
    });
}

#[test]
fn host_download_cancel_before_viewer_attached_dispatch_retires_only_the_spent_lane() {
    run_test!(cx, {
        let mut l = Link::new(&cx).await;
        download(&mut l);
        let (_hi, _ci, hi, ci) = attach(&mut l, &cx, 8, MediaRole::Input).await;
        let (host, mut viewer, routes) = host_finished_first(&mut l, &cx).await;
        let incoming = StreamRoute {
            outbound: false,
            maximum: attachment::GRANT_RECORD_BYTES,
            messages: Messages::Exact(0x1a),
            ..routes.outbound
        };
        let mut host = FilesChannel::new(&l.h, host, InputLeaseId::from_raw(123), 456).unwrap();
        host.retire(&mut l.h, &cx).unwrap();
        let until = clock(&cx) + 1_000_000;
        while !l.c.receive_ended(incoming).unwrap() {
            assert!(clock(&cx) < until, "viewer did not receive Files reset");
            l.drive(&cx).await;
        }
        assert_eq!(
            viewer.retire_files_if_peer_cancelled(&mut l.c, &cx, || true),
            Ok(true)
        );
        assert!(!viewer.is_complete());
        assert_eq!(viewer.completed_on(&l.c), Err(Error::WrongRoute));
        drop(viewer);
        l.c.tick(&cx, || true).unwrap();
        l.h.tick(&cx, || true).unwrap();
        assert!(matches!(
            offer(&mut l, &cx, 10, MediaRole::Files),
            Err(Error::WrongRoute)
        ));
        input::transfer(
            &mut l,
            &cx,
            false,
            Route::Stream(ci.outbound),
            Route::Stream(hi.inbound),
            &record(0x40, 8, 128),
        )
        .await;
    });
}
