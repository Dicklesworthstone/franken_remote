//! Both real ATP directions on one original TLS/UDP controller. Each has its
//! own finite selection, destination, attachment, receipts and cancellation.
use super::*;
use crate::{
    native_files::{Directory, Limits, ReceiveControl, ReceiveRequest, SendControl, SendRequest},
    session_startup::{
        running::streaming::files::Lane as HostLane, viewer::streaming::files::Lane as ViewerLane,
    },
};
use fr_files::sender::batch::Selection;
use fr_transport::quic::ChannelRequest;
use fr_wire::{attachment::Ticket, decoder::Binding, negotiation::ControlBinding};

async fn setup_duplex(c: &Cx, h: &Cx) -> Fixture {
    Box::pin(fixture_with_clipboard(
        c,
        h,
        caps(),
        false,
        false,
        false,
        ClipboardMode::FileDuplex,
    ))
    .await
}
fn request(state: &Fixture, id: u32) -> ChannelRequest {
    ChannelRequest {
        binding: Binding {
            parent: ControlBinding {
                id,
                ..state.viewer.media.as_ref().unwrap().binding().parent
            },
            ..state.viewer.media.as_ref().unwrap().binding()
        },
        ticket: Ticket(800 + u128::from(id)),
        timeout: Duration::from_secs(2),
    }
}
fn selected(source: &Disk, bytes: &[u8], name: &str) -> Selection {
    let mut selected = Selection::new();
    selected.push(source.source(bytes), name).unwrap();
    selected
}
fn start(
    state: &mut Fixture,
    host_dest: &Disk,
    viewer_dest: &Disk,
    host_source: &Disk,
    download: &[u8],
) -> Permission {
    let permission = Permission::new(true);
    state
        .viewer
        .expect_file_drop(
            permission.clone(),
            fr_files::sender::Policy::default(),
            Duration::from_secs(2),
        )
        .unwrap();
    state
        .viewer
        .expect_file_download(viewer_dest.config(), Duration::from_secs(2))
        .unwrap();
    let upload_request = request(state, 14);
    let download_request = request(state, 15);
    state
        .host
        .offer_file_drop(upload_request, host_dest.config())
        .unwrap();
    state
        .host
        .offer_file_download(
            download_request,
            selected(host_source, download, "download"),
            Permission::new(true),
            fr_files::sender::Policy::default(),
            Duration::from_secs(5),
        )
        .unwrap();
    assert!(state.viewer.file_send_negotiating() && state.viewer.file_receive_negotiating());
    assert!(state.host.file_receive_negotiating() && state.host.file_send_negotiating());
    assert!(state.viewer.file_stage().is_none() && state.viewer.file_receive_state().is_none());
    permission
}
async fn attached(state: &mut Fixture, c: &Cx, h: &Cx, n: &mut u128, t: &mut u128, until: u64) {
    while state.viewer.file_send_negotiating()
        || state.viewer.file_receive_negotiating()
        || state.host.file_receive_negotiating()
        || state.host.file_send_negotiating()
    {
        assert!(now(c).unwrap() < until, "bounded duplex attachment");
        drive(state, c, h, n, t).await;
    }
    assert_eq!(state.viewer.file_stage(), Some(Stage::Idle));
    assert!(state.viewer.file_receive_state().is_some());
}

#[test]
fn both_directions_publish_and_keep_input_on_the_original_controller() {
    run(|c, h| async move {
        let cleanup = Cx::current().unwrap();
        let mut state = Box::pin(setup_duplex(&c, &h)).await;
        let host_source = Disk::new();
        let viewer_source = Disk::new();
        let host_dest = Disk::new();
        let viewer_dest = Disk::new();
        let upload = vec![0x36; 1_000_007];
        let download = vec![0x87; 1_000_019];
        let mut host_lane = HostLane::default();
        host_lane.add_upload(&Directory::open(&host_dest.0, Limits::default()).unwrap());
        host_lane.add_download(SendRequest {
            selection: selected(&host_source, &download, "download"),
            policy: fr_files::sender::Policy::default(),
            lifetime: Duration::from_secs(5),
        });
        let send = SendControl::new();
        let receive = ReceiveControl::new();
        let mut viewer_lane = ViewerLane::default();
        viewer_lane
            .add_send(
                SendRequest {
                    selection: selected(&viewer_source, &upload, "upload"),
                    policy: fr_files::sender::Policy::default(),
                    lifetime: Duration::from_secs(5),
                },
                send.clone(),
            )
            .unwrap();
        viewer_lane
            .add_receive(
                ReceiveRequest::new(&Directory::open(&viewer_dest.0, Limits::default()).unwrap()),
                receive.clone(),
            )
            .unwrap();
        let view = state.host_media.binding();
        let mut tickets = 700_000_u128;
        let mut ticket = || nonce(&mut tickets);
        host_lane.host(&mut state.host, view, &mut ticket);
        assert!(!state.host.file_receive_negotiating() && !state.host.file_send_negotiating());
        viewer_lane.viewer(&mut state.viewer);
        assert!(state.viewer.file_send_negotiating() && state.viewer.file_receive_negotiating());
        let lease = state.viewer.input.binding().lease;
        let driver = state.driver.take().unwrap();
        let ((), done) = Box::pin(support::both(
            async {
                let (mut n, mut t) = (80_000, 90_000);
                let until = now(&c).unwrap() + 5_000_000;
                let _ = state.viewer.action(key(true)).unwrap();
                let mut released = false;
                loop {
                    assert!(now(&c).unwrap() < until, "bounded duplex publication");
                    drive(&mut state, &c, &h, &mut n, &mut t).await;
                    host_lane.host(&mut state.host, view, &mut ticket);
                    viewer_lane.viewer(&mut state.viewer);
                    if !released && state.viewer.pending_actions() == 0 {
                        let _ = state.viewer.action(key(false)).unwrap();
                        released = true;
                    }
                    if state.viewer.file_batch_report().is_some_and(|r| r.complete)
                        && state.host.file_batch_report().is_some_and(|r| r.complete)
                        && released
                        && state.viewer.pending_actions() == 0
                    {
                        break;
                    }
                }
                let sent = state.viewer.file_batch_report().unwrap();
                let received = state.host.file_batch_report().unwrap();
                assert_eq!(send.status().absence, None);
                assert_eq!(send.status().report, Some(sent));
                assert_eq!(receive.status().absence, None);
                assert_eq!(receive.status().results.len(), 1);
                assert_eq!((sent.started, sent.not_started(), sent.stop), (1, 0, None));
                assert_eq!(
                    (received.started, received.not_started(), received.stop),
                    (1, 0, None)
                );
                assert!(sent.receipts().all(|r| r.outcome
                    == Outcome::HostPublished {
                        bytes: upload.len() as u64,
                        publication: Publication::Durable,
                    }));
                assert!(received.receipts().all(|r| r.outcome
                    == Outcome::ControllerPublished {
                        bytes: download.len() as u64,
                        publication: Publication::Durable,
                    }));
                assert_eq!(fs::read(host_dest.0.join("upload")).unwrap(), upload);
                assert_eq!(fs::read(viewer_dest.0.join("download")).unwrap(), download);
                assert_eq!(state.viewer.input.binding().lease, lease);
                assert_eq!(state.effects.lock().unwrap().keys, [true, false]);
                assert!(!state.viewer.is_closed() && !state.host.control().is_stopped());
                // Either spent direction refuses replacement without spending a
                // third attachment or changing the other direction's receipts.
                assert_eq!(
                    state.viewer.expect_file_drop(
                        Permission::new(true),
                        fr_files::sender::Policy::default(),
                        Duration::from_secs(2),
                    ),
                    Err(SendError::Busy)
                );
                assert_eq!(
                    state
                        .viewer
                        .expect_file_download(viewer_dest.config(), Duration::from_secs(2)),
                    Err(SendError::Busy)
                );
                download::retire(&mut state, &cleanup).await;
                assert!(state.viewer.file_cleanup_finished());
                assert_eq!(state.viewer.try_finish_file_cleanup(), Some(Ok(())));
                assert_eq!(state.viewer.file_batch_report(), Some(sent));
                assert_eq!(state.host.file_batch_report(), Some(received));
                assert_eq!(
                    state.viewer.file_receive_status().unwrap().cleanup,
                    Some(Ok(()))
                );
            },
            driver,
        ))
        .await;
        assert!(done.handoff_safe());
        assert!(!state.seat.is_occupied());
    });
}

#[test]
fn cancelling_either_partial_direction_preserves_the_other_and_input() {
    for cancel_download in [false, true] {
        run(move |c, h| async move {
            let cleanup = Cx::current().unwrap();
            let mut state = Box::pin(setup_duplex(&c, &h)).await;
            let host_source = Disk::new();
            let viewer_source = Disk::new();
            let host_dest = Disk::new();
            let viewer_dest = Disk::new();
            let upload = vec![0x36; 1_000_007];
            let download = vec![0x87; 1_000_019];
            start(
                &mut state,
                &host_dest,
                &viewer_dest,
                &host_source,
                &download,
            );
            let lease = state.viewer.input.binding().lease;
            let driver = state.driver.take().unwrap();
            let ((), done) = Box::pin(support::both(
                async {
                    let (mut n, mut t) = (80_000, 90_000);
                    let until = now(&c).unwrap() + 5_000_000;
                    attached(&mut state, &c, &h, &mut n, &mut t, until).await;
                    state
                        .viewer
                        .send_files(
                            selected(&viewer_source, &upload, "upload"),
                            Duration::from_secs(5),
                        )
                        .unwrap();
                    loop {
                        let partial = if cancel_download {
                            state.viewer.file_receive_progress().is_some_and(|p| {
                                p.staged_bytes > 0 && p.staged_bytes < download.len() as u64
                            })
                        } else {
                            state.viewer.file_progress().is_some_and(|p| {
                                p.queued_bytes > 0 && p.queued_bytes < upload.len() as u64
                            })
                        };
                        if partial {
                            break;
                        }
                        assert!(now(&c).unwrap() < until, "bounded partial transfer");
                        drive(&mut state, &c, &h, &mut n, &mut t).await;
                    }
                    if cancel_download {
                        // The last disk receipt records staging progress too;
                        // only the separate terminal result list certifies an end.
                        let progress = state.viewer.file_receive_progress().unwrap();
                        assert!(matches!(
                            state.viewer.file_receive_result(),
                            Some(fr_files::wire::ResultReceipt {
                                id,
                                outcome: Ok(
                                    fr_files::worker::Completion::Begun(actual)
                                        | fr_files::worker::Completion::Written(actual)
                                ),
                            }) if id == progress.id && actual == progress
                        ));
                        assert!(
                            state
                                .viewer
                                .file_receive_status()
                                .unwrap()
                                .results
                                .is_empty()
                        );
                        state.viewer.cancel_file_receive().unwrap();
                    } else {
                        assert!(state.viewer.file_result().is_none());
                        assert!(
                            state
                                .viewer
                                .file_batch_report()
                                .unwrap()
                                .receipts()
                                .next()
                                .is_none()
                        );
                        state.viewer.cancel_file_send().unwrap();
                    }
                    let _ = state.viewer.action(key(true)).unwrap();
                    loop {
                        assert!(
                            now(&c).unwrap() < until,
                            "independent direction must finish"
                        );
                        drive(&mut state, &c, &h, &mut n, &mut t).await;
                        let (complete, cleaned) = if cancel_download {
                            (
                                state.viewer.file_batch_report().is_some_and(|r| r.complete),
                                state.viewer.try_finish_file_receive_cleanup() == Some(Ok(())),
                            )
                        } else {
                            (
                                state.host.file_batch_report().is_some_and(|r| r.complete),
                                state.viewer.try_finish_file_send_cleanup() == Some(Ok(()))
                                    && matches!(
                                        state.host.file_receive_cleanup(),
                                        crate::session_startup::FileReceiveCleanup::Finished(
                                            Ok(())
                                        )
                                    ),
                            )
                        };
                        if complete && cleaned && state.viewer.pending_actions() == 0 {
                            break;
                        }
                    }
                    if cancel_download {
                        let sent = state.viewer.file_batch_report().unwrap();
                        assert_eq!(sent.stop, None);
                        assert!(
                            sent.receipts()
                                .all(|r| matches!(r.outcome, Outcome::HostPublished { .. }))
                        );
                        assert_eq!(fs::read(host_dest.0.join("upload")).unwrap(), upload);
                        assert_eq!(fs::read_dir(&viewer_dest.0).unwrap().count(), 0);
                        assert!(state.viewer.file_send_failure().is_none());
                    } else {
                        let received = state.host.file_batch_report().unwrap();
                        assert_eq!(received.stop, None);
                        assert!(
                            received
                                .receipts()
                                .all(|r| matches!(r.outcome, Outcome::ControllerPublished { .. }))
                        );
                        assert_eq!(fs::read(viewer_dest.0.join("download")).unwrap(), download);
                        assert_eq!(fs::read_dir(&host_dest.0).unwrap().count(), 0);
                        assert!(state.viewer.file_receive_setup_failure().is_none());
                    }
                    assert_eq!(state.viewer.input.binding().lease, lease);
                    assert_eq!(state.effects.lock().unwrap().keys, [true]);
                    assert!(!state.viewer.is_closed() && !state.host.control().is_stopped());
                    download::retire(&mut state, &cleanup).await;
                    assert!(state.viewer.file_cleanup_finished());
                },
                driver,
            ))
            .await;
            assert!(done.handoff_safe());
            assert!(!state.seat.is_occupied());
        });
    }
}

mod permission;
