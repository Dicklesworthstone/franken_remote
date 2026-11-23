//! Job-state routing on actual native UDP/TLS; these content-free fixtures do
//! not claim filesystem publication, local consent or tailnet qualification.
use super::*;
use fr_wire::sync_job_state::{self, ConflictPolicy, SyncDirection, SyncJobState, SyncPhase};

fn enable_sync(link: &mut Link, duplex: bool) {
    enable(link);
    let mut capabilities = vec![
        (wire::DOWNLOAD_CAPABILITY, wire::DOWNLOAD_VERSION),
        (sync_job_state::CAPABILITY, sync_job_state::VERSION),
    ];
    if duplex {
        capabilities.extend([
            (wire::CHANNEL_SCOPE_CAPABILITY, wire::CHANNEL_SCOPE_VERSION),
            (wire::DUPLEX_CAPABILITY, wire::DUPLEX_VERSION),
        ]);
    }
    for (name, version) in capabilities {
        link.selection.capabilities.push(Capability {
            name: name.into(),
            version,
            required: false,
        });
    }
    link.selection
        .capabilities
        .sort_by(|a, b| a.name.cmp(&b.name));
    assert!(sync_job_state::selected(&link.selection));
    assert_eq!(wire::duplex_selected(&link.selection), duplex);
}

fn advertised() -> SyncJobState {
    SyncJobState {
        job_id: 41,
        sync_direction: SyncDirection::HostToController,
        conflict_policy: ConflictPolicy::KeepBoth,
        phase: SyncPhase::Advertised,
        reason: wire::Reason::None,
        entries_total: 0,
        entries_completed: 0,
        conflicts_kept: 0,
        bytes_total: 0,
        bytes_completed: 0,
        manifest_revision: 0,
    }
}

async fn transfer_state(
    link: &mut Link,
    cx: &Cx,
    source: &FilesChannel,
    receiver: &FilesChannel,
    state: SyncJobState,
) {
    let host = source.outgoing().sender == wire::Role::Host;
    let until = clock(cx) + 1_000_000;
    let q = if host { &mut link.h } else { &mut link.c };
    assert_eq!(source.retained_send_records(q), Ok(0));
    source.send_sync(q, cx, &state, until, || true).unwrap();
    // State is an ordinary counted application record, never setup-prefix
    // credit or an independent uncharged side queue.
    assert_eq!(source.retained_send_records(q), Ok(1));
    let mut received = false;
    while !received
        || !source
            .send_drained(if host { &link.h } else { &link.c })
            .unwrap()
    {
        assert!(clock(cx) < until, "original sync record did not drain");
        link.drive(cx).await;
        receiver
            .dispatch(
                if host { &mut link.c } else { &mut link.h },
                cx,
                || true,
                |bytes| {
                    assert!(!received);
                    assert_eq!(
                        sync_job_state::decode(bytes, receiver.incoming(), receiver.limits()),
                        Ok(state)
                    );
                    received = true;
                    Ok(Disposition::Consumed)
                },
            )
            .unwrap();
    }
    assert_eq!(
        source.retained_send_records(if host { &link.h } else { &link.c }),
        Ok(0)
    );
}

#[test]
fn selected_legacy_and_duplex_downloads_exchange_counted_sync_state_and_atp() {
    run_test!(cx, {
        for duplex in [false, true] {
            let mut link = Link::new(&cx).await;
            enable_sync(&mut link, duplex);
            let _input = attach(&mut link, &cx, 8, MediaRole::Input).await;
            let role = if duplex {
                MediaRole::FilesDownload
            } else {
                MediaRole::Files
            };
            let (host, viewer, hr, vr) = attach(&mut link, &cx, 9, role).await;
            assert_eq!(hr.outbound.messages, Messages::FilesSync);
            assert_eq!(vr.outbound.messages, Messages::FilesSync);
            // Later local selection mutation cannot enable or disable this
            // original completed attachment's negotiated message family.
            link.selection
                .capabilities
                .retain(|cap| cap.name != sync_job_state::CAPABILITY);
            let host = FilesChannel::new(&link.h, host, InputLeaseId::from_raw(123), 9).unwrap();
            let viewer =
                FilesChannel::new(&link.c, viewer, InputLeaseId::from_raw(123), 9).unwrap();
            assert!(host.sync_selected() && viewer.sync_selected());
            transfer_state(&mut link, &cx, &host, &viewer, advertised()).await;
            transfer_state(
                &mut link,
                &cx,
                &viewer,
                &host,
                SyncJobState {
                    phase: SyncPhase::Accepted,
                    ..advertised()
                },
            )
            .await;

            let bytes = encoded_file(&host);
            let until = clock(&cx) + 1_000_000;
            host.send(&mut link.h, &cx, &bytes, until, || true).unwrap();
            let mut got = false;
            while !got || !host.send_drained(&link.h).unwrap() {
                assert!(clock(&cx) < until);
                link.drive(&cx).await;
                viewer
                    .dispatch(
                        &mut link.c,
                        &cx,
                        || true,
                        |record| {
                            assert_eq!(record, bytes);
                            assert!(
                                wire::decode(record, viewer.incoming(), viewer.limits()).is_ok()
                            );
                            got = true;
                            Ok(Disposition::Consumed)
                        },
                    )
                    .unwrap();
            }
            transfer_state(
                &mut link,
                &cx,
                &host,
                &viewer,
                SyncJobState {
                    phase: SyncPhase::Settled,
                    manifest_revision: 1,
                    ..advertised()
                },
            )
            .await;
        }
    });
}

#[test]
fn sync_selection_never_opens_upload_or_unnegotiated_download_records() {
    run_test!(cx, {
        for upload in [false, true] {
            let mut link = Link::new(&cx).await;
            enable_sync(&mut link, upload);
            if !upload {
                link.selection
                    .capabilities
                    .retain(|cap| cap.name != sync_job_state::CAPABILITY);
            }
            let _input = attach(&mut link, &cx, 8, MediaRole::Input).await;
            let (host, viewer, hr, vr) = attach(&mut link, &cx, 9, MediaRole::Files).await;
            assert_eq!(hr.outbound.messages, Messages::Files);
            assert_eq!(vr.outbound.messages, Messages::Files);
            let host = FilesChannel::new(&link.h, host, InputLeaseId::from_raw(123), 9).unwrap();
            let viewer =
                FilesChannel::new(&link.c, viewer, InputLeaseId::from_raw(123), 9).unwrap();
            assert!(!host.sync_selected() && !viewer.sync_selected());
            let before = link.h.usage();
            let until = clock(&cx) + 1_000_000;
            assert_eq!(
                host.send_sync(&mut link.h, &cx, &advertised(), until, || true),
                Err(Error::WrongRoute)
            );
            assert_eq!(
                link.h.send(
                    &cx,
                    Route::Stream(hr.outbound),
                    &record(0x75, 9, 121),
                    until,
                    || true
                ),
                Err(Error::WrongRoute)
            );
            assert_eq!(link.h.usage(), before);
        }
    });
}

#[test]
fn sync_keeps_original_scope_role_blocked_record_and_input_independence() {
    run_test!(cx, {
        let mut link = Link::new(&cx).await;
        enable_sync(&mut link, false);
        let (_hi, _ci, hi, ci) = attach(&mut link, &cx, 8, MediaRole::Input).await;
        let (host, viewer, _, _) = attach(&mut link, &cx, 9, MediaRole::Files).await;
        let mut host = FilesChannel::new(&link.h, host, InputLeaseId::from_raw(123), 9).unwrap();
        let viewer = FilesChannel::new(&link.c, viewer, InputLeaseId::from_raw(123), 9).unwrap();
        let until = clock(&cx) + 1_000_000;
        let before = link.h.usage();
        for wrong in [
            wire::Context {
                session: RemoteSessionId::from_raw(91),
                ..host.outgoing()
            },
            wire::Context {
                lease: InputLeaseId::from_raw(92),
                ..host.outgoing()
            },
            wire::Context {
                handle: 93,
                ..host.outgoing()
            },
            wire::Context {
                channel: 94,
                ..host.outgoing()
            },
        ] {
            let mut bytes = [0; sync_job_state::RECORD_BYTES];
            sync_job_state::encode(&advertised(), wrong, host.limits(), &mut bytes).unwrap();
            assert_eq!(
                host.send(&mut link.h, &cx, &bytes, until, || true),
                Err(Error::Malformed)
            );
        }
        assert_eq!(link.h.usage(), before);
        assert_eq!(
            viewer.send_sync(&mut link.c, &cx, &advertised(), until, || true),
            Err(Error::Malformed)
        );
        let mut foreign = Link::new(&cx).await;
        let foreign_before = foreign.h.usage();
        assert_eq!(
            host.send_sync(&mut foreign.h, &cx, &advertised(), until, || true),
            Err(Error::WrongRoute)
        );
        assert_eq!(foreign.h.usage(), foreign_before);
        assert!(!foreign.h.is_closed());

        host.send_sync(&mut link.h, &cx, &advertised(), until, || true)
            .unwrap();
        let mut blocked = None;
        while blocked.is_none() {
            assert!(clock(&cx) < until);
            link.drive(&cx).await;
            viewer
                .dispatch(
                    &mut link.c,
                    &cx,
                    || true,
                    |bytes| {
                        blocked = Some(bytes.to_vec());
                        Ok(Disposition::Blocked)
                    },
                )
                .unwrap();
        }
        input::transfer(
            &mut link,
            &cx,
            false,
            Route::Stream(ci.outbound),
            Route::Stream(hi.inbound),
            &record(0x40, 8, 128),
        )
        .await;
        let mut consumed = false;
        viewer
            .dispatch(
                &mut link.c,
                &cx,
                || true,
                |bytes| {
                    assert_eq!(Some(bytes), blocked.as_deref());
                    consumed = true;
                    Ok(Disposition::Consumed)
                },
            )
            .unwrap();
        assert!(consumed);
        while !host.send_drained(&link.h).unwrap() {
            assert!(clock(&cx) < until);
            link.drive(&cx).await;
        }
        host.retire(&mut link.h, &cx).unwrap();
        assert!(
            host.send_sync(&mut link.h, &cx, &advertised(), until, || true)
                .is_err()
        );
        assert!(matches!(
            offer(&mut link, &cx, 10, MediaRole::Files),
            Err(Error::WrongRoute)
        ));
        assert!(!link.h.is_closed() && !link.c.is_closed());
    });
}
