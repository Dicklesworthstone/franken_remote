//! Application flights exclude original setup custody without releasing it.
//! Actual UDP/TLS and the existing attachment fixture; no publication claim.
use super::*;

fn enable_download(link: &mut Link, role: MediaRole) {
    enable(link);
    link.selection.capabilities.push(Capability {
        name: wire::DOWNLOAD_CAPABILITY.into(),
        version: wire::DOWNLOAD_VERSION,
        required: true,
    });
    if role == MediaRole::FilesDownload {
        for (name, version) in [
            (wire::CHANNEL_SCOPE_CAPABILITY, wire::CHANNEL_SCOPE_VERSION),
            (wire::DUPLEX_CAPABILITY, wire::DUPLEX_VERSION),
        ] {
            link.selection.capabilities.push(Capability {
                name: name.into(),
                version,
                required: true,
            });
        }
    }
    link.selection
        .capabilities
        .sort_by(|a, b| a.name.cmp(&b.name));
}

async fn promote_before_peer_ack(
    link: &mut Link,
    cx: &Cx,
    role: MediaRole,
) -> (MediaChannel, MediaChannel) {
    let mut host = offer(link, cx, 9, role).unwrap();
    let mut viewer = link.viewer(cx, &mut host).await;
    let until = clock(cx) + 1_000_000;
    loop {
        assert!(clock(cx) < until, "host attachment did not complete");
        host.transmit(&mut link.h, cx, || true).unwrap();
        viewer.transmit(&mut link.c, cx, || true).unwrap();
        // Stage and send the host's records, then promote before the viewer
        // can receive or acknowledge the final Attached. Transmit alone only
        // admits a pending write and cannot establish this native boundary.
        link.h.drive(cx, WAIT, || true).await.unwrap();
        host.dispatch(&mut link.h, cx, || true).unwrap();
        if host.finish(&mut link.h, cx, || true).unwrap().is_some() {
            assert!(!viewer.is_complete());
            return (host, viewer);
        }
        link.c.drive(cx, WAIT, || true).await.unwrap();
        viewer.dispatch(&mut link.c, cx, || true).unwrap();
        assert!(viewer.finish(&mut link.c, cx, || true).unwrap().is_none());
    }
}

#[test]
fn host_file_flight_excludes_retained_attached_but_counts_each_application_record() {
    run_test!(cx, {
        for role in [MediaRole::Files, MediaRole::FilesDownload] {
            let mut link = Link::new(&cx).await;
            enable_download(&mut link, role);
            let _input = attach(&mut link, &cx, 8, MediaRole::Input).await;
            let (host, mut viewer) = promote_before_peer_ack(&mut link, &cx, role).await;
            let host = FilesChannel::new(&link.h, host, InputLeaseId::from_raw(123), 456).unwrap();
            assert_eq!(host.outgoing().direction, wire::Direction::ToController);
            assert!(!host.send_drained(&link.h).unwrap());
            assert_eq!(host.retained_send_records(&link.h), Ok(0));
            let setup = link.h.usage();
            assert!(setup.retained_send_records > 0);
            assert!(setup.retained_send_upper_bound > 0);

            let bytes = encoded_file(&host);
            let until = clock(&cx) + 1_000_000;
            for count in 1..=2 {
                host.send(&mut link.h, &cx, &bytes, until, || true).unwrap();
                assert_eq!(host.retained_send_records(&link.h), Ok(count));
                assert_eq!(
                    link.h.usage().retained_send_records,
                    setup.retained_send_records + count
                );
                assert_eq!(
                    link.h.usage().retained_send_upper_bound,
                    setup.retained_send_upper_bound + count * bytes.len()
                );
            }
            // The viewer must authenticate Attached before interpreting the
            // following records, even when both arrive in the same native read.
            loop {
                assert!(clock(&cx) < until, "viewer attachment did not complete");
                link.drive(&cx).await;
                viewer.dispatch(&mut link.c, &cx, || true).unwrap();
                if viewer.finish(&mut link.c, &cx, || true).unwrap().is_some() {
                    break;
                }
            }
            let viewer =
                FilesChannel::new(&link.c, viewer, InputLeaseId::from_raw(123), 456).unwrap();
            assert_eq!(viewer.retained_send_records(&link.c), Ok(0));
            let mut received = 0;
            while received != 2 || host.retained_send_records(&link.h).unwrap() != 0 {
                assert!(clock(&cx) < until, "file records did not deliver and ACK");
                link.drive(&cx).await;
                viewer
                    .dispatch(
                        &mut link.c,
                        &cx,
                        || true,
                        |record| {
                            assert_eq!(record, bytes);
                            received += 1;
                            assert!(received <= 2);
                            Ok(Disposition::Consumed)
                        },
                    )
                    .unwrap();
            }
            assert!(host.send_drained(&link.h).unwrap());
            assert_eq!(host.retained_send_records(&link.h), Ok(0));
            assert!(!link.h.is_closed());
            assert!(!link.c.is_closed());
        }
    });
}
