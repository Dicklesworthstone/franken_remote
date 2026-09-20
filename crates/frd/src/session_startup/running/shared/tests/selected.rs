//! Source-native selection, real TLS/UDP, canonical host joins and independent viewers.
//! Discovery, encoded pictures and decode receipts are explicit protocol fixtures.
use super::*;
use crate::session_startup::running::publisher::Error as PublishError;
use crate::{
    display_selection::SelectedDisplay,
    media::{SharedCaptureUpdate, discovery::DiscoveredSource},
};
use fr_transport::quic::MediaChannel;
use std::{
    future::{Future, poll_fn},
    pin::pin,
    task::Poll,
};

fn is_send<T: Send>(_: &T) {}

async fn selected_publisher(rt: &Runtime) -> (Publisher, ObservationControl, SharedCaptureUpdate) {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let mut payload = String::new();
    for nal in [
        "40010c01ffff01600000030090000003000003003cba0240",
        "42010101600000030090000003000003003ca00a080f165ba4a4c2f016a020202080000003008000000f04",
        "4401c0718112",
        "2801ade06702f86753c11ead2f1f6a69",
    ] {
        write!(payload, "{:08x}{nal}", nal.len() / 2).unwrap();
    }
    let script = include_str!("../../../../../tests/support/local_source_fixture.py")
        .replace("@MODE@", "normal")
        .replace(
            "b\"synthetic-monitor-unit\"",
            &format!("bytes.fromhex('{payload}')"),
        );
    let path = std::env::temp_dir().join(format!(
        "fr-selected-shared-{}-{}.py",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::write(&path, script).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
    let owner = source_control(rt);
    let discovery = DiscoveredSource::start(
        &owner,
        Launch::new(&path, ":0", None, WorkerRole::Capture, 90).unwrap(),
    )
    .await
    .unwrap();
    let catalog = discovery.catalog().unwrap();
    let mut source = discovery
        .configure_local(
            catalog.selection(catalog.displays()[0].handle).unwrap(),
            codec(),
        )
        .unwrap()
        .await
        .unwrap();
    let pool = SharedFramePool::new(ProtocolLimits::ABSOLUTE, 32 * 1024 * 1024, 8).unwrap();
    let initial = source
        .prepare_shared_capture(&owner, &pool)
        .unwrap()
        .capture_if_changed(true)
        .await
        .unwrap();
    (
        Publisher::new(source, owner.clone(), pool, &initial).unwrap(),
        owner,
        initial,
    )
}
async fn select(viewer: &mut ViewerSession) -> SelectedDisplay {
    let mut choice = viewer.select_display(Duration::from_secs(2)).unwrap();
    let mut chosen = false;
    while !choice.is_complete() {
        choice.dispatch(viewer.io().unwrap().0).unwrap();
        if !chosen && let Some(catalog) = choice.catalog(viewer.io().unwrap().0).unwrap() {
            assert_eq!(
                catalog.displays().len(),
                1,
                "never disclose the neighboring monitor"
            );
            assert_eq!(catalog.displays()[0].x, -320);
            let handle = catalog.displays()[0].handle;
            choice.choose(viewer.io().unwrap().0, handle).unwrap();
            chosen = true;
        }
        choice.transmit(viewer.io().unwrap().0).unwrap();
        viewer.drive(Duration::from_millis(1), block).await.unwrap();
    }
    choice.finish(viewer.io().unwrap().0).unwrap()
}
async fn channel(viewer: &mut ViewerSession, c: &Cx, expected: MediaRole) -> MediaChannel {
    let mut channel: Option<MediaChannel> = None;
    let until = now(c).unwrap() + 1_500_000;
    loop {
        assert!(now(c).unwrap() < until, "shared attachment deadline");
        if let Some(channel) = &mut channel {
            channel
                .transmit(viewer.io().unwrap().0, c, || true)
                .unwrap();
            channel
                .dispatch(viewer.io().unwrap().0, c, || true)
                .unwrap();
            if channel
                .finish(viewer.io().unwrap().0, c, || true)
                .unwrap()
                .is_some()
            {
                assert_eq!(channel.descriptor().role, expected);
                break;
            }
        }
        let mut offer = None;
        viewer
            .drive(Duration::from_millis(1), |_, bytes| {
                if channel.is_none()
                    && bytes.get(6..8) == Some(&(fr_wire::Kind::StreamBinding as u16).to_be_bytes())
                {
                    offer = Some(bytes.to_vec());
                    Ok(Disposition::Consumed)
                } else {
                    Ok(Disposition::Blocked)
                }
            })
            .await
            .unwrap();
        if let Some(offer) = offer {
            channel = Some(
                viewer
                    .accept_media_channel(&offer, Duration::from_secs(2))
                    .unwrap(),
            );
        }
    }
    channel.unwrap()
}
struct SelectedPeer {
    peer: Member,
    _selected: SelectedDisplay,
}
async fn accept(
    mut viewer: ViewerSession,
    c: Cx,
    h: Cx,
    control: ObservationControl,
    selection: fr_wire::negotiation::Selection,
) -> SelectedPeer {
    let selected = select(&mut viewer).await;
    Box::pin(accept_selected(viewer, selected, c, h, control, selection)).await
}
async fn accept_selected(
    mut viewer: ViewerSession,
    selected: SelectedDisplay,
    c: Cx,
    h: Cx,
    control: ObservationControl,
    selection: fr_wire::negotiation::Selection,
) -> SelectedPeer {
    let cfg = channel(&mut viewer, &c, MediaRole::Configuration).await;
    let recovery = channel(&mut viewer, &c, MediaRole::Recovery).await;
    let video = channel(&mut viewer, &c, MediaRole::Video).await;
    let q = viewer.io().unwrap().0;
    let media = NegotiatedMedia::new(q, &selection, &cfg, &recovery, &video).unwrap();
    let reply = cfg.completed_on(q).unwrap().outbound;
    let config = media.receiver_config(q, ReceivePolicy::default()).unwrap();
    let receiver =
        ReceivePipeline::new(config, MediaBudget::new(config.limits.protocol()).unwrap()).unwrap();
    SelectedPeer {
        _selected: selected,
        peer: Member {
            h,
            c,
            session: None,
            shared: None,
            viewer,
            control,
            host_media: None,
            media,
            reply,
            receiver,
            configuration: None,
            configured: false,
            first: None,
            acknowledged: false,
            frames: vec![],
            nonce: 10000,
        },
    }
}
async fn first(
    rt: &Runtime,
    publisher: &mut Publisher,
    initial: &SharedCaptureUpdate,
    id: u128,
) -> SelectedPeer {
    let (c, h, host, viewer) =
        sessions(rt, id, Role::Observe, &[fr_wire::display::CAPABILITY]).await;
    let selection = host.selection().clone();
    let control = host.original_observation();
    let mut n = 40000;
    let (host, mut peer) = Box::pin(support::both(
        host.start_shared_display(
            publisher,
            initial,
            Duration::from_secs(2),
            SendPolicy::default(),
            || nonce(&mut n),
        ),
        accept(viewer, c, h, control, selection),
    ))
    .await;
    peer.peer.shared = Some(host.unwrap());
    assert!(!peer.peer.complete());
    while !peer.peer.complete() {
        peer.peer.turn().await.unwrap();
    }
    peer
}
async fn close(publisher: &mut Publisher, peers: Vec<SelectedPeer>) {
    for mut p in peers {
        p.peer.shared.as_mut().unwrap().close();
        p.peer.viewer.close();
    }
    let cx = Cx::current().unwrap();
    publisher
        .reap(&cx, Deadline::after(&cx, Duration::from_secs(1)).unwrap())
        .await
        .unwrap();
}
#[test]
fn normal_selected_source_bootstrap_and_late_join_retain_one_worker_after_first_viewer_leaves() {
    let rt = support::runtime();
    rt.block_on(async {
        let (mut publisher, owner, initial) = selected_publisher(&rt).await;
        let pid = publisher.worker_id();
        let mut first_peer = Box::pin(first(&rt, &mut publisher, &initial, 13)).await;
        let original_view = first_peer.peer.media.binding();
        assert!(!first_peer.peer.control.view_ready().unwrap());
        let (c, h, host, viewer) =
            sessions(&rt, 14, Role::Observe, &[fr_wire::display::CAPABILITY]).await;
        let selection = host.selection().clone();
        let control = host.original_observation();
        let mut next_nonce = 80000;
        let (host, mut second_peer) = Box::pin(support::both(
            host.join_shared_display(
                publisher.join_queue(),
                Duration::from_secs(2),
                SendPolicy::default(),
                || nonce(&mut next_nonce),
            ),
            accept(viewer, c, h, control, selection),
        ))
        .await;
        second_peer.peer.shared = Some(host.unwrap());
        assert_eq!(publisher.worker_id(), pid);
        assert!(!second_peer.peer.complete());
        publisher.capture_next().await.unwrap();
        while !second_peer.peer.complete() {
            first_peer.peer.turn().await.unwrap();
            second_peer.peer.turn().await.unwrap();
        }
        assert_eq!(first_peer.peer.media.binding(), original_view);
        assert_eq!(
            second_peer.peer.media.binding().display,
            original_view.display
        );
        assert!(second_peer.peer.first.unwrap() > initial.frame().as_raw());
        assert!(!second_peer.peer.control.view_ready().unwrap());
        first_peer.peer.shared.as_mut().unwrap().close();
        assert!(owner.check().is_ok());
        assert_eq!(publisher.tick().unwrap(), 1);
        second_peer.peer.turn().await.unwrap();
        assert_eq!(publisher.worker_id(), pid);
        close(&mut publisher, vec![first_peer, second_peer]).await;
        assert!(owner.check().is_err());
    });
}
#[test]
fn unpolled_shared_display_join_retires_only_the_new_viewer() {
    let rt = support::runtime();
    rt.block_on(async {
        let (mut publisher, owner, initial) = selected_publisher(&rt).await;
        let a = Box::pin(first(&rt, &mut publisher, &initial, 13)).await;
        let (_, _, host, _viewer) =
            sessions(&rt, 14, Role::Observe, &[fr_wire::display::CAPABILITY]).await;
        let control = host.original_observation();
        let future = host.join_shared_display(
            publisher.join_queue(),
            Duration::from_secs(2),
            SendPolicy::default(),
            || panic!("no nonce before polling"),
        );
        is_send(&future);
        drop(future);
        assert!(control.check().is_err());
        assert!(owner.check().is_ok());
        assert_eq!(publisher.tick().unwrap(), 1);
        close(&mut publisher, vec![a]).await;
    });
}
#[test]
fn shared_display_budget_is_fixed_before_poll_and_control_intent_refuses() {
    let rt = support::runtime();
    rt.block_on(async {
        let (mut publisher, owner, initial) = selected_publisher(&rt).await;
        let a = Box::pin(first(&rt, &mut publisher, &initial, 13)).await;
        for intent in [Role::Observe, Role::RequestControl] {
            let (_, _, host, _viewer) =
                sessions(&rt, 14, intent, &[fr_wire::display::CAPABILITY]).await;
            let control = host.original_observation();
            let future = host.join_shared_display(
                publisher.join_queue(),
                Duration::from_millis(1),
                SendPolicy::default(),
                || panic!("refuse before entropy"),
            );
            if intent == Role::Observe {
                std::thread::sleep(Duration::from_millis(5));
            }
            let result = future.await;
            assert!(matches!(
                result,
                Err(PublishError::Expired | PublishError::InvalidConfiguration)
            ));
            assert!(control.check().is_err());
            assert!(owner.check().is_ok());
        }
        close(&mut publisher, vec![a]).await;
    });
}
// Poll the failing host first so no test-only peer callback can run after source
// revocation. The observer is abandoned only after the actual host result exists.
async fn refusal<T>(
    work: impl Future<Output = Result<T, PublishError>>,
    peer: impl Future<Output = SelectedPeer>,
) -> PublishError {
    let mut work = pin!(work);
    let mut peer = pin!(peer);
    poll_fn(|cx| {
        if let Poll::Ready(result) = work.as_mut().poll(cx) {
            return Poll::Ready(result.err().expect("host must refuse"));
        }
        assert!(peer.as_mut().poll(cx).is_pending());
        Poll::Pending
    })
    .await
}
#[test]
fn source_revocation_during_shared_bootstrap_prevents_new_attachment() {
    let rt = support::runtime();
    rt.block_on(async {
        let (mut publisher, owner, initial) = selected_publisher(&rt).await;
        let a = Box::pin(first(&rt, &mut publisher, &initial, 13)).await;
        let (c, h, host, viewer) =
            sessions(&rt, 14, Role::Observe, &[fr_wire::display::CAPABILITY]).await;
        let selection = host.selection().clone();
        let control = host.original_observation();
        let result = Box::pin(refusal(
            host.join_shared_display(
                publisher.join_queue(),
                Duration::from_secs(2),
                SendPolicy::default(),
                || {
                    // Revoke from the original local owner during the first nonce callback.
                    owner.revoke();
                    Ok(91000)
                },
            ),
            accept(viewer, c, h, control.clone(), selection),
        ))
        .await;
        assert!(matches!(
            result,
            PublishError::Shared(_) | PublishError::Session(_)
        ));
        assert!(control.check().is_err());
        assert!(owner.check().is_err());
        close(&mut publisher, vec![a]).await;
    });
}

#[test]
fn initial_selected_source_refuses_media_bound_to_another_display() {
    let rt = support::runtime();
    rt.block_on(async {
        let mut peer = Box::pin(peer(&rt, 13, Role::Observe)).await;
        let (mut publisher, owner, initial) = selected_publisher(&rt).await;
        let mut session = peer.session.take().unwrap();
        let q = session.io().unwrap().0;
        let media = peer.host_media.take().unwrap();
        assert_ne!(
            media.binding().display,
            publisher
                .join_queue()
                .selected_catalog()
                .unwrap()
                .displays()[0]
                .handle
        );
        let startup = decoder_startup::Host::new_shared(
            peer.control.clone(),
            q,
            media.decoder_setup(q, Duration::from_secs(2)).unwrap(),
            codec(),
            initial,
        )
        .unwrap();
        let sender = media
            .sender(q, peer.control.clone(), SendPolicy::default())
            .unwrap();
        assert!(matches!(
            publisher.admit_pending(startup, sender, media, q),
            Err(crate::media::shared_publisher::Error::WrongSource)
        ));
        assert!(owner.check().is_ok());
        assert_eq!(publisher.tick().unwrap(), 0);
        publisher.close();
        close(&mut publisher, vec![]).await;
    });
}

#[test]
fn abandoned_first_shared_display_keeps_the_original_source_available_within_its_budget() {
    let rt = support::runtime();
    rt.block_on(async {
        let (mut publisher, owner, initial) = selected_publisher(&rt).await;
        let pid = publisher.worker_id();
        let (_, _, host, _viewer) =
            sessions(&rt, 13, Role::Observe, &[fr_wire::display::CAPABILITY]).await;
        let control = host.original_observation();
        let future = host.start_shared_display(
            &mut publisher,
            &initial,
            Duration::from_secs(2),
            SendPolicy::default(),
            || panic!("unpolled attempt cannot request entropy"),
        );
        is_send(&future);
        drop(future);
        assert!(control.check().is_err());
        assert!(owner.check().is_ok());
        assert_eq!(publisher.worker_id(), pid);
        let peer = Box::pin(first(&rt, &mut publisher, &initial, 14)).await;
        assert_eq!(publisher.worker_id(), pid);
        close(&mut publisher, vec![peer]).await;
    });
}

#[test]
fn ticket_callback_failure_is_terminal_without_borrowing_another_viewers_lifetime() {
    let rt = support::runtime();
    rt.block_on(async {
        for revoke_source in [false, true] {
            let (mut publisher, owner, initial) = selected_publisher(&rt).await;
            let first_peer = Box::pin(first(&rt, &mut publisher, &initial, 13)).await;
            let (c, h, host, mut viewer) =
                sessions(&rt, 14, Role::Observe, &[fr_wire::display::CAPABILITY]).await;
            let selection = host.selection().clone();
            let control = host.original_observation();
            let chosen = AtomicBool::new(false);
            let called = AtomicBool::new(false);
            let mut next_nonce = 200_000;
            let host = host.join_shared_display(
                publisher.join_queue(),
                Duration::from_secs(2),
                SendPolicy::default(),
                || {
                    if chosen.load(Ordering::Acquire) {
                        called.store(true, Ordering::Release);
                        if revoke_source {
                            owner.revoke();
                            nonce(&mut next_nonce)
                        } else {
                            Err(())
                        }
                    } else {
                        nonce(&mut next_nonce)
                    }
                },
            );
            let peer = async {
                let selected = select(&mut viewer).await;
                chosen.store(true, Ordering::Release);
                Box::pin(accept_selected(
                    viewer,
                    selected,
                    c,
                    h,
                    control.clone(),
                    selection,
                ))
                .await
            };
            let error = Box::pin(refusal(host, peer)).await;
            assert!(
                called.load(Ordering::Acquire),
                "fail only after actual display choice"
            );
            assert!(control.check().is_err());
            if revoke_source {
                assert!(matches!(error, PublishError::Shared(_)));
                assert!(owner.check().is_err());
            } else {
                assert!(matches!(error, PublishError::Identity));
                assert!(owner.check().is_ok());
                assert!(first_peer.peer.control.check().is_ok());
                assert_eq!(publisher.tick().unwrap(), 1);
            }
            close(&mut publisher, vec![first_peer]).await;
        }
    });
}
