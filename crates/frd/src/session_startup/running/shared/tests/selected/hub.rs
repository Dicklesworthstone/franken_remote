//! Real original sessions/UDP and source IPC; media and decode remain fixtures.
use super::*;
use crate::session_startup::shared_viewers::{self as service, Admission, Hub, State, Ticket};

fn entropy() -> service::Entropy {
    let counter = AtomicU64::new(400_000);
    Arc::new(move || Ok(u128::from(counter.fetch_add(1, Ordering::Relaxed))))
}
async fn client_turn(peer: &mut Member) {
    peer.prepare_reply();
    let cfg = &mut peer.configuration;
    peer.viewer
        .drive(Duration::from_millis(1), |route, bytes| {
            if matches!(route, Route::Stream(r) if r.messages == Messages::Exact(0x30)) {
                assert!(cfg.is_none());
                *cfg = Some(bytes.to_vec());
                Ok(Disposition::Consumed)
            } else {
                Ok(Disposition::Blocked)
            }
        })
        .await
        .unwrap();
    peer.media
        .receive_ready(
            &peer.c,
            peer.viewer.io().unwrap().0,
            || true,
            |channel, bytes| {
                peer.receiver
                    .receive(channel, bytes, now(&peer.c).unwrap())
                    .unwrap();
                Ok(Disposition::Consumed)
            },
        )
        .unwrap();
    while let Some(picture) = peer.receiver.take_decodable(now(&peer.c).unwrap()).unwrap() {
        let frame = peer
            .receiver
            .complete_decode(&picture, now(&peer.c).unwrap())
            .unwrap()
            .descriptor()
            .frame;
        peer.first.get_or_insert(frame);
        peer.frames.push(frame);
    }
    asupersync::runtime::yield_now().await;
}
async fn until_ready(peer: &mut Member) {
    let until = now(&peer.c).unwrap() + 1_500_000;
    while !peer.acknowledged {
        assert!(now(&peer.c).unwrap() < until);
        client_turn(peer).await;
    }
    for _ in 0..3 {
        client_turn(peer).await;
    }
}
async fn together<T>(hub: &mut Hub, publisher: &mut Publisher, work: impl Future<Output = T>) -> T {
    let mut network = pin!(hub.serve());
    is_send(&network);
    let mut capture = pin!(publisher.serve(Duration::from_millis(50), |_| {}));
    let mut work = Box::pin(work);
    poll_fn(|cx| {
        assert!(
            network.as_mut().poll(cx).is_pending(),
            "unexpected hub exit"
        );
        assert!(
            capture.as_mut().poll(cx).is_pending(),
            "unexpected source exit"
        );
        work.as_mut().poll(cx)
    })
    .await
}
// A healthy remote peer must continue consuming its original connection while
// another peer performs a slower, rate-limited join. Pausing it intentionally
// trips the existing per-viewer reference deadline; that is not a hub failure.
async fn with_client<T>(peer: &mut Member, work: impl Future<Output = T>) -> T {
    let mut service = Box::pin(async {
        loop {
            client_turn(peer).await;
        }
    });
    let mut work = Box::pin(work);
    poll_fn(|cx| {
        assert!(service.as_mut().poll(cx).is_pending());
        work.as_mut().poll(cx)
    })
    .await
}
async fn reap(publisher: &mut Publisher) {
    let cx = Cx::current().unwrap();
    publisher
        .reap(&cx, Deadline::after(&cx, Duration::from_secs(1)).unwrap())
        .await
        .unwrap();
}
async fn fixture(
    rt: &Runtime,
    policy: service::Policy,
    random: service::Entropy,
) -> (
    Publisher,
    ObservationControl,
    SelectedPeer,
    Hub,
    Admission,
    Ticket,
) {
    let (mut publisher, owner, initial) = selected_publisher(rt).await;
    let mut peer = Box::pin(first(rt, &mut publisher, &initial, 13)).await;
    let hub = Hub::new(peer.peer.shared.take().unwrap(), policy, random).unwrap();
    let admissions = hub.admissions();
    let ticket = hub.initial();
    (publisher, owner, peer, hub, admissions, ticket)
}

#[test]
fn hub_drives_late_joins_and_reuses_slots_without_reusing_cancellation_authority() {
    let rt = support::runtime();
    rt.block_on(async {
        let policy = service::Policy {
            viewers: 2,
            ..service::Policy::default()
        };
        let (mut publisher, owner, mut first, mut hub, admission, old) =
            fixture(&rt, policy, entropy()).await;
        let pid = publisher.worker_id();
        Box::pin(together(&mut hub, &mut publisher, async {
            let (c, h, host, viewer) =
                sessions(&rt, 14, Role::Observe, &[fr_wire::display::CAPABILITY]).await;
            let selection = host.selection().clone();
            let control = host.original_observation();
            let second_ticket = admission.admit(host).unwrap();
            assert_eq!(second_ticket.state(), State::Starting);
            let mut second = Box::pin(accept(viewer, c, h, control, selection)).await;
            until_ready(&mut second.peer).await;
            assert_eq!(second_ticket.state(), State::Serving);
            assert!(first.peer.control.check().is_ok());
            old.close();
            first.peer.viewer.close();
            for _ in 0..5 {
                client_turn(&mut second.peer).await;
            }
            assert!(owner.check().is_ok());
            // The original remote-session number is reused only in this fixture;
            // the old local ticket must still point exclusively at the old owner.
            let (mut third, replacement) = Box::pin(with_client(&mut second.peer, async {
                let (c, h, host, viewer) =
                    sessions(&rt, 13, Role::Observe, &[fr_wire::display::CAPABILITY]).await;
                let selection = host.selection().clone();
                let control = host.original_observation();
                let replacement = admission.admit(host).unwrap();
                let mut third = Box::new(Box::pin(accept(viewer, c, h, control, selection)).await);
                until_ready(&mut third.peer).await;
                (third, replacement)
            }))
            .await;
            old.close();
            for _ in 0..5 {
                client_turn(&mut third.peer).await;
                client_turn(&mut second.peer).await;
            }
            assert_eq!(replacement.state(), State::Serving);
            assert!(third.peer.control.check().is_ok());
            assert!(second.peer.control.check().is_ok());
            assert!(!third.peer.control.view_ready().unwrap());
            let statistics = admission.statistics().unwrap();
            assert_eq!(statistics.admitted, 3);
            assert_eq!(statistics.finished, 1);
            second.peer.viewer.close();
            third.peer.viewer.close();
        }))
        .await;
        assert_eq!(publisher.worker_id(), pid);
        reap(&mut publisher).await;
    });
}

#[test]
fn hub_capacity_includes_unpolled_joins_and_duplicate_ids_cannot_alias_an_owner() {
    let rt = support::runtime();
    rt.block_on(async {
        let (mut publisher, owner, _first, mut hub, admission, _) = fixture(
            &rt,
            service::Policy {
                viewers: 2,
                ..service::Policy::default()
            },
            entropy(),
        )
        .await;
        let (_, _, host, _viewer) =
            sessions(&rt, 14, Role::Observe, &[fr_wire::display::CAPABILITY]).await;
        let pending = admission.admit(host).unwrap();
        let (_, _, host, _viewer2) =
            sessions(&rt, 15, Role::Observe, &[fr_wire::display::CAPABILITY]).await;
        let refused = host.original_observation();
        assert!(matches!(admission.admit(host), Err(service::Error::Full)));
        assert!(refused.check().is_err());
        let (_, _, host, _viewer3) =
            sessions(&rt, 13, Role::Observe, &[fr_wire::display::CAPABILITY]).await;
        assert!(matches!(
            admission.admit(host),
            Err(service::Error::DuplicateSession)
        ));
        assert_eq!(pending.state(), State::Starting);
        assert!(owner.check().is_ok());
        hub.close();
        reap(&mut publisher).await;
    });
}

#[test]
fn hub_unpolled_cancellation_fences_active_and_starting_sessions() {
    let rt = support::runtime();
    rt.block_on(async {
        let (mut publisher, owner, first, mut hub, admission, initial) =
            fixture(&rt, service::Policy::default(), entropy()).await;
        let (_, _, host, _viewer) =
            sessions(&rt, 14, Role::Observe, &[fr_wire::display::CAPABILITY]).await;
        let control = host.original_observation();
        let pending = admission.admit(host).unwrap();
        let run = hub.serve();
        is_send(&run);
        drop(run);
        assert!(first.peer.control.check().is_err());
        assert!(control.check().is_err());
        assert!(matches!(initial.state(), State::Finished(Err(_))));
        assert!(matches!(pending.state(), State::Finished(Err(_))));
        assert!(owner.check().is_err());
        reap(&mut publisher).await;
    });
}

#[test]
fn hub_queued_admission_keeps_its_original_deadline_instead_of_starting_a_new_budget() {
    let rt = support::runtime();
    rt.block_on(async {
        let policy = service::Policy {
            join_timeout: Duration::from_millis(60),
            ..service::Policy::default()
        };
        let (mut publisher, owner, mut first, mut hub, admission, _) =
            fixture(&rt, policy, entropy()).await;
        let (_, _, host, _viewer) =
            sessions(&rt, 14, Role::Observe, &[fr_wire::display::CAPABILITY]).await;
        let pending = admission.admit(host).unwrap();
        asupersync::time::sleep(
            asupersync::types::Time::from_nanos(now(&first.peer.c).unwrap() * 1000),
            Duration::from_millis(80),
        )
        .await;
        Box::pin(together(&mut hub, &mut publisher, async {
            for _ in 0..10 {
                client_turn(&mut first.peer).await;
            }
            assert!(matches!(
                pending.state(),
                State::Finished(Err(service::Error::Publication(_)))
            ));
            assert!(owner.check().is_ok());
            assert!(first.peer.control.check().is_ok());
        }))
        .await;
        reap(&mut publisher).await;
    });
}

#[test]
fn hub_silent_newcomer_cannot_stop_a_healthy_viewers_original_session_renewal() {
    let rt = support::runtime();
    rt.block_on(async {
        let policy = service::Policy {
            join_timeout: Duration::from_millis(100),
            ..service::Policy::default()
        };
        let (mut publisher, owner, mut first, mut hub, admission, _) =
            fixture(&rt, policy, entropy()).await;
        let (_, _, host, _viewer) =
            sessions(&rt, 14, Role::Observe, &[fr_wire::display::CAPABILITY]).await;
        let pending = admission.admit(host).unwrap();
        Box::pin(together(&mut hub, &mut publisher, async {
            let until = now(&first.peer.c).unwrap() + 3_200_000;
            while now(&first.peer.c).unwrap() < until {
                client_turn(&mut first.peer).await;
            }
            assert!(matches!(pending.state(), State::Finished(Err(_))));
            assert!(first.peer.control.check().is_ok());
            assert!(owner.check().is_ok());
            assert!(!first.peer.control.view_ready().unwrap());
        }))
        .await;
        reap(&mut publisher).await;
    });
}

#[test]
fn hub_entropy_callbacks_run_outside_the_registry_lock() {
    let rt = support::runtime();
    rt.block_on(async {
        let handle: Arc<std::sync::Mutex<Option<Admission>>> = Arc::default();
        let callbacks = Arc::new(AtomicU64::new(0));
        let (copy, count) = (handle.clone(), callbacks.clone());
        let random: service::Entropy = Arc::new(move || {
            if let Some(admission) = copy.lock().unwrap().as_ref() {
                admission.statistics().unwrap();
            }
            Ok(u128::from(900_000 + count.fetch_add(1, Ordering::Relaxed)))
        });
        let (mut publisher, _owner, _first, mut hub, admission, _) =
            fixture(&rt, service::Policy::default(), random).await;
        *handle.lock().unwrap() = Some(admission.clone());
        Box::pin(together(&mut hub, &mut publisher, async {
            let (c, h, host, viewer) =
                sessions(&rt, 14, Role::Observe, &[fr_wire::display::CAPABILITY]).await;
            let selection = host.selection().clone();
            let control = host.original_observation();
            let ticket = admission.admit(host).unwrap();
            let mut peer = Box::pin(accept(viewer, c, h, control, selection)).await;
            until_ready(&mut peer.peer).await;
            assert_eq!(ticket.state(), State::Serving);
            assert!(callbacks.load(Ordering::Relaxed) >= 3);
        }))
        .await;
        reap(&mut publisher).await;
    });
}

#[test]
fn hub_refuses_control_intent_and_foreign_os_scope_before_starting_attachments() {
    let rt = support::runtime();
    rt.block_on(async {
        let (mut publisher, owner, _first, mut hub, admission, _) =
            fixture(&rt, service::Policy::default(), entropy()).await;
        for foreign in [false, true] {
            let (_, _, mut host, _viewer) =
                sessions(&rt, 14, Role::Observe, &[fr_wire::display::CAPABILITY]).await;
            let control = host.original_observation();
            // Fixture-only variation of metadata: no network bytes are changed.
            if foreign {
                host.opened.binding.os_session = OsSessionId::from_raw(999);
            } else {
                host.opened.selected.role = Role::RequestControl;
            }
            let result = admission.admit(host);
            assert!(matches!(
                result,
                Err(service::Error::ForeignScope | service::Error::WrongRole)
            ));
            assert!(control.check().is_err());
            assert!(owner.check().is_ok());
        }
        hub.close();
        reap(&mut publisher).await;
    });
}

mod desktop;

mod incoming;
