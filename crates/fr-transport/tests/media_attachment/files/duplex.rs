//! Real native UDP/TLS for independent file directions on the same controller.
//! File envelopes are routing fixtures, not ATP publication or tailnet evidence.
use super::*;

fn add_duplex(link: &mut Link) {
    for (name, version) in [
        (wire::CHANNEL_SCOPE_CAPABILITY, wire::CHANNEL_SCOPE_VERSION),
        (wire::DOWNLOAD_CAPABILITY, wire::DOWNLOAD_VERSION),
        (wire::DUPLEX_CAPABILITY, wire::DUPLEX_VERSION),
    ] {
        if !link
            .selection
            .capabilities
            .iter()
            .any(|cap| cap.name == name)
        {
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
fn enable_duplex(link: &mut Link) {
    enable(link);
    add_duplex(link);
    assert!(wire::duplex_selected(&link.selection));
}

struct Pair {
    host: FilesChannel,
    viewer: FilesChannel,
    host_routes: AttachedChannel,
    viewer_routes: AttachedChannel,
}
impl Pair {
    fn new(
        link: &Link,
        host: MediaChannel,
        viewer: MediaChannel,
        host_routes: AttachedChannel,
        viewer_routes: AttachedChannel,
    ) -> Self {
        let handle = u128::from(host_routes.descriptor.binding.parent.id);
        Self {
            host: FilesChannel::new(&link.h, host, InputLeaseId::from_raw(123), handle).unwrap(),
            viewer: FilesChannel::new(&link.c, viewer, InputLeaseId::from_raw(123), handle)
                .unwrap(),
            host_routes,
            viewer_routes,
        }
    }
}

/// All three offers exist before any optional owner completes. Reverse ACK
/// service forces each dispatcher to preserve TWO other owners' exact records.
async fn attach_three(
    link: &mut Link,
    cx: &Cx,
) -> (Pair, Pair, [AttachedChannel; 2], [AttachedChannel; 2]) {
    enable_duplex(link);
    for (name, version) in [
        (
            attachment::CLIPBOARD_CAPABILITY,
            attachment::CLIPBOARD_VERSION,
        ),
        (fr_wire::clipboard::CAPABILITY, fr_wire::clipboard::VERSION),
    ] {
        link.selection.capabilities.push(Capability {
            name: name.into(),
            version,
            required: true,
        });
    }
    link.selection
        .capabilities
        .sort_by(|a, b| a.name.cmp(&b.name));
    let (_, _, hi, vi) = attach(link, cx, 8, MediaRole::Input).await;
    let mut hosts = [
        offer(link, cx, 9, MediaRole::Clipboard).unwrap(),
        offer(link, cx, 10, MediaRole::Files).unwrap(),
        offer(link, cx, 11, MediaRole::FilesDownload).unwrap(),
    ];
    assert!(!hosts[2].transmit(&mut link.h, cx, || true).unwrap());
    let before = link.h.usage();
    assert!(matches!(
        offer(link, cx, 12, MediaRole::Configuration),
        Err(Error::Backpressure)
    ));
    assert_eq!(link.h.usage(), before);
    let mut viewers = [
        link.viewer(cx, &mut hosts[0]).await,
        link.viewer(cx, &mut hosts[1]).await,
        link.viewer(cx, &mut hosts[2]).await,
    ];
    let until = clock(cx) + 1_000_000;
    let (host_routes, viewer_routes) = loop {
        assert!(clock(cx) < until, "three optional owners must finish");
        for viewer in viewers.iter_mut().rev() {
            viewer.transmit(&mut link.c, cx, || true).unwrap();
        }
        for host in &mut hosts {
            host.transmit(&mut link.h, cx, || true).unwrap();
        }
        link.drive(cx).await;
        let mut host_routes = [None; 3];
        let mut viewer_routes = [None; 3];
        for index in 0..3 {
            hosts[index].dispatch(&mut link.h, cx, || true).unwrap();
            host_routes[index] = hosts[index].finish(&mut link.h, cx, || true).unwrap();
        }
        for index in (0..3).rev() {
            viewers[index].dispatch(&mut link.c, cx, || true).unwrap();
            viewer_routes[index] = viewers[index].finish(&mut link.c, cx, || true).unwrap();
        }
        if host_routes.iter().all(Option::is_some) && viewer_routes.iter().all(Option::is_some) {
            break (
                host_routes.map(Option::unwrap),
                viewer_routes.map(Option::unwrap),
            );
        }
    };
    for index in 0..3 {
        assert_eq!(
            host_routes[index].descriptor,
            viewer_routes[index].descriptor
        );
        for sibling in index + 1..3 {
            assert_ne!(host_routes[index].inbound, host_routes[sibling].inbound);
            assert_ne!(host_routes[index].outbound, host_routes[sibling].outbound);
        }
    }
    let [_clipboard, upload, download] = hosts;
    let [_clipboard, vu, vd] = viewers;
    (
        Pair::new(link, upload, vu, host_routes[1], viewer_routes[1]),
        Pair::new(link, download, vd, host_routes[2], viewer_routes[2]),
        [hi, vi],
        [host_routes[0], viewer_routes[0]],
    )
}

async fn exchange(link: &mut Link, cx: &Cx, pair: &Pair, download: bool) {
    let (source, destination) = if download {
        (&pair.host, &pair.viewer)
    } else {
        (&pair.viewer, &pair.host)
    };
    let bytes = encoded_file(source);
    let until = clock(cx) + 1_000_000;
    source
        .send(
            if download { &mut link.h } else { &mut link.c },
            cx,
            &bytes,
            until,
            || true,
        )
        .unwrap();
    let mut got = false;
    while !got {
        assert!(clock(cx) < until, "original file direction must progress");
        link.drive(cx).await;
        destination
            .dispatch(
                if download { &mut link.c } else { &mut link.h },
                cx,
                || true,
                |record| {
                    assert_eq!(record, bytes);
                    got = true;
                    Ok(Disposition::Consumed)
                },
            )
            .unwrap();
    }
}

#[test]
fn download_role_requires_complete_selection_and_this_connections_completed_input() {
    run_test!(cx, {
        let mut link = Link::new(&cx).await;
        enable_duplex(&mut link);
        let before = link.h.usage();
        assert!(matches!(
            offer(&mut link, &cx, 8, MediaRole::FilesDownload),
            Err(Error::WrongRoute)
        ));
        assert_eq!(link.h.usage(), before);
        let _input = attach(&mut link, &cx, 8, MediaRole::Input).await;
        let complete = link.selection.clone();
        let before = link.h.usage();
        for cap in &complete.capabilities {
            for unknown_version in [false, true] {
                link.selection = complete.clone();
                if unknown_version {
                    link.selection
                        .capabilities
                        .iter_mut()
                        .find(|c| c.name == cap.name)
                        .unwrap()
                        .version += 1;
                } else {
                    link.selection.capabilities.retain(|c| c.name != cap.name);
                }
                assert!(
                    matches!(
                        offer(&mut link, &cx, 9, MediaRole::FilesDownload),
                        Err(Error::WrongRoute)
                    ),
                    "missing exact {}",
                    cap.name
                );
                assert_eq!(link.h.usage(), before);
            }
        }
        link.selection = complete.clone();
        link.selection.role = Role::Observe;
        assert!(matches!(
            offer(&mut link, &cx, 9, MediaRole::FilesDownload),
            Err(Error::WrongRoute)
        ));
        assert_eq!(link.h.usage(), before);
        link.selection = complete;
        let (_, _, download, _) = attach(&mut link, &cx, 9, MediaRole::FilesDownload).await;
        let (_, _, upload, _) = attach(&mut link, &cx, 10, MediaRole::Files).await;
        assert_eq!(download.descriptor.role, MediaRole::FilesDownload);
        assert_eq!(upload.descriptor.role, MediaRole::Files);
        let before = link.h.usage();
        for role in [MediaRole::Files, MediaRole::FilesDownload] {
            assert!(matches!(
                offer(&mut link, &cx, 11, role),
                Err(Error::WrongRoute)
            ));
        }
        assert_eq!(link.h.usage(), before);
        assert!(!link.h.is_closed() && !link.c.is_closed());
    });
}

#[test]
fn both_file_roles_survive_three_overlapping_exchanges_and_a_blocked_sibling() {
    run_test!(cx, {
        let mut link = Link::new(&cx).await;
        let original = (link.h.binding(), link.c.binding());
        let (upload, download, _, _) = attach_three(&mut link, &cx).await;
        assert_eq!(upload.host.incoming(), upload.viewer.outgoing());
        assert_eq!(download.host.outgoing(), download.viewer.incoming());
        assert_eq!(upload.viewer.outgoing().direction, wire::Direction::ToHost);
        assert_eq!(
            download.host.outgoing().direction,
            wire::Direction::ToController
        );
        assert!(
            !upload
                .host
                .owns_inbound(Route::Stream(download.host_routes.inbound))
        );
        assert!(
            !download
                .viewer
                .owns_inbound(Route::Stream(upload.viewer_routes.inbound))
        );
        let up = encoded_file(&upload.viewer);
        let down = encoded_file(&download.host);
        let before = link.h.usage();
        assert_eq!(
            download
                .host
                .send(&mut link.h, &cx, &up, clock(&cx) + 1_000_000, || true),
            Err(Error::Malformed)
        );
        assert_eq!(link.h.usage(), before);
        let mut foreign = Link::new(&cx).await;
        assert_eq!(
            download
                .host
                .send(&mut foreign.h, &cx, &down, clock(&cx) + 1_000_000, || true),
            Err(Error::WrongRoute)
        );
        assert!(!foreign.h.is_closed());
        let until = clock(&cx) + 1_000_000;
        upload
            .viewer
            .send(&mut link.c, &cx, &up, until, || true)
            .unwrap();
        download
            .host
            .send(&mut link.h, &cx, &down, until, || true)
            .unwrap();
        let (mut blocked, mut received) = (false, false);
        while !blocked || !received {
            assert!(clock(&cx) < until);
            link.drive(&cx).await;
            upload
                .host
                .dispatch(
                    &mut link.h,
                    &cx,
                    || true,
                    |bytes| {
                        assert_eq!(bytes, up);
                        blocked = true;
                        Ok(Disposition::Blocked)
                    },
                )
                .unwrap();
            download
                .viewer
                .dispatch(
                    &mut link.c,
                    &cx,
                    || true,
                    |bytes| {
                        assert_eq!(bytes, down);
                        received = true;
                        Ok(Disposition::Consumed)
                    },
                )
                .unwrap();
        }
        let mut retained = false;
        upload
            .host
            .dispatch(
                &mut link.h,
                &cx,
                || true,
                |bytes| {
                    assert_eq!(bytes, up);
                    retained = true;
                    Ok(Disposition::Consumed)
                },
            )
            .unwrap();
        assert!(retained);
        assert!(link.h.is_bound_to(&original.0) && link.c.is_bound_to(&original.1));
    });
}

#[test]
fn retiring_either_direction_preserves_its_sibling_clipboard_input_and_control() {
    for cancel_download in [false, true] {
        run_test!(cx, {
            let mut link = Link::new(&cx).await;
            let (mut upload, mut download, input, clipboard) = attach_three(&mut link, &cx).await;
            exchange(&mut link, &cx, &upload, false).await;
            exchange(&mut link, &cx, &download, true).await;
            let until = clock(&cx) + 1_000_000;
            if cancel_download {
                download.host.retire(&mut link.h, &cx).unwrap();
                while download.viewer.check(&link.c).is_ok() {
                    assert!(clock(&cx) < until);
                    link.drive(&cx).await;
                }
                download.viewer.retire(&mut link.c, &cx).unwrap();
            } else {
                upload.viewer.retire(&mut link.c, &cx).unwrap();
                while upload.host.check(&link.h).is_ok() {
                    assert!(clock(&cx) < until);
                    link.drive(&cx).await;
                }
                upload.host.retire(&mut link.h, &cx).unwrap();
            }
            exchange(
                &mut link,
                &cx,
                if cancel_download { &upload } else { &download },
                !cancel_download,
            )
            .await;
            let before = link.h.usage();
            for role in [MediaRole::Files, MediaRole::FilesDownload] {
                assert!(matches!(
                    offer(&mut link, &cx, 12, role),
                    Err(Error::WrongRoute)
                ));
            }
            assert_eq!(link.h.usage(), before, "retired roles remain spent");
            assert!(!link.h.is_closed() && !link.c.is_closed());
            input::transfer(
                &mut link,
                &cx,
                false,
                Route::Stream(input[1].outbound),
                Route::Stream(input[0].inbound),
                &record(0x40, 8, 128),
            )
            .await;
            input::transfer(
                &mut link,
                &cx,
                true,
                Route::Stream(clipboard[0].outbound),
                Route::Stream(clipboard[1].inbound),
                &record(0x51, 9, 128),
            )
            .await;
            let (outgoing, incoming) = (link.hr.outbound, link.cr.inbound);
            input::transfer(
                &mut link,
                &cx,
                true,
                Route::Stream(outgoing),
                Route::Stream(incoming),
                &record(0x12, 7, 128),
            )
            .await;
        });
    }
}

#[test]
fn a_legacy_file_attachment_cannot_be_upgraded_or_reinterpreted_after_selection() {
    for legacy_download in [false, true] {
        run_test!(cx, {
            let mut link = Link::new(&cx).await;
            enable(&mut link);
            if legacy_download {
                link.selection.capabilities.push(Capability {
                    name: wire::DOWNLOAD_CAPABILITY.into(),
                    version: wire::DOWNLOAD_VERSION,
                    required: true,
                });
                link.selection
                    .capabilities
                    .sort_by(|a, b| a.name.cmp(&b.name));
            }
            let _input = attach(&mut link, &cx, 8, MediaRole::Input).await;
            let (host, viewer, hr, vr) = attach(&mut link, &cx, 9, MediaRole::Files).await;
            let pair = Pair::new(&link, host, viewer, hr, vr);
            let original = pair.host.outgoing().direction;
            assert_eq!(
                original,
                if legacy_download {
                    wire::Direction::ToController
                } else {
                    wire::Direction::ToHost
                }
            );
            add_duplex(&mut link);
            assert!(wire::duplex_selected(&link.selection));
            let before = link.h.usage();
            assert!(matches!(
                offer(&mut link, &cx, 10, MediaRole::FilesDownload),
                Err(Error::WrongRoute)
            ));
            assert_eq!(link.h.usage(), before);
            assert_eq!(pair.host.outgoing().direction, original);
            assert_eq!(pair.viewer.incoming().direction, original);
            exchange(&mut link, &cx, &pair, legacy_download).await;
            assert!(!link.h.is_closed() && !link.c.is_closed());
        });
    }
}
