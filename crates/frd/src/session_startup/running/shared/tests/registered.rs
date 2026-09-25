//! Actual publisher/session authorities and TLS/UDP, not a registry-state mock.
use super::*;
use crate::broker::session_registry::{PublicationError, SessionRegistry};

fn registry() -> SessionRegistry {
    SessionRegistry::new(HostBootId::from_raw(11), OsSessionId::from_raw(12), 8)
}
#[test]
fn registered_source_admits_a_live_session_and_os_switch_fences_all_original_owners() {
    let rt = support::runtime();
    let cx = rt.request_cx_with_budget(Budget::INFINITE);
    rt.block_on(async {
        let mut g = Box::pin(group(&rt, 1)).await;
        ready(&mut g).await;
        let pid = g.publisher.worker_id();
        let mut registry = registry();
        let registration = registry.register_publisher(&g.publisher).unwrap();
        assert_eq!(registry.publication_count().unwrap(), 1);
        let mut late = Box::pin(peer(&rt, 14, Role::Observe)).await;
        let session = late.session.take().unwrap();
        late.shared = Some(
            session
                .join_registered(
                    &registry,
                    late.host_media.take().unwrap(),
                    SendPolicy::default(),
                    Duration::from_secs(2),
                )
                .unwrap(),
        );
        assert!(!late.complete());
        let source = g.publisher.serve(Duration::from_millis(50), |_| {});
        let network = async {
            let until = now(&late.h).unwrap() + 1_500_000;
            while !late.complete() {
                assert!(now(&late.h).unwrap() < until);
                let (a, b) = Box::pin(support::both(g.peers[0].turn(), late.turn())).await;
                a.unwrap();
                b.unwrap();
            }
            assert!(!late.control.view_ready().unwrap());
            registry.switch_os_session(OsSessionId::from_raw(99));
            assert_eq!(registry.os_session_id(), OsSessionId::from_raw(99));
            assert_eq!(
                registration.check(),
                Err(PublicationError::StaleRegistration)
            );
            assert!(g.owner.check().is_err());
            assert!(g.peers[0].control.check().is_err());
            assert!(late.control.check().is_err());
            assert!(late.turn().await.is_err());
        };
        let (result, ()) = Box::pin(support::both(source, network)).await;
        assert!(result.is_err());
        assert_eq!(
            g.publisher.worker_id(),
            pid,
            "registry must preserve native reap custody"
        );
        assert_eq!(g.publisher.physical_usage(), BudgetUsage::default());
        late.viewer.close();
        cleanup(&mut g, &cx).await;
    });
}
#[test]
fn every_registry_generation_transition_revokes_actual_source_not_just_metadata() {
    let rt = support::runtime();
    let cx = rt.request_cx_with_budget(Budget::INFINITE);
    rt.block_on(async {
        for change in 0..4 {
            let mut g = Box::pin(group(&rt, 1)).await;
            ready(&mut g).await;
            let mut registry = registry();
            let old = registry.register_publisher(&g.publisher).unwrap();
            match change {
                0 => {
                    registry.advance_geometry_generation().unwrap();
                }
                1 => {
                    registry.advance_codec_generation().unwrap();
                }
                2 => {
                    registry.advance_process_generation().unwrap();
                }
                _ => {
                    registry.teardown_all();
                }
            }
            assert_eq!(old.check(), Err(PublicationError::StaleRegistration));
            assert_eq!(registry.publication_count().unwrap(), 0);
            assert!(g.owner.check().is_err());
            assert!(g.peers[0].control.check().is_err());
            assert!(g.peers[0].turn().await.is_err());
            assert_eq!(g.publisher.physical_usage(), BudgetUsage::default());
            cleanup(&mut g, &cx).await;
        }
    });
}
#[test]
fn registry_drop_and_explicit_retirement_end_real_consent_and_never_redirect_old_handles() {
    let rt = support::runtime();
    let cx = rt.request_cx_with_budget(Budget::INFINITE);
    rt.block_on(async {
        let mut g = Box::pin(group(&rt, 1)).await;
        ready(&mut g).await;
        let mut registry = registry();
        let old = registry.register_publisher(&g.publisher).unwrap();
        registry.retire_publisher(&old).unwrap();
        assert!(g.owner.check().is_err());
        cleanup(&mut g, &cx).await;
        let mut fresh = Box::pin(group(&rt, 1)).await;
        ready(&mut fresh).await;
        let successor = registry.register_publisher(&fresh.publisher).unwrap();
        assert_eq!(
            registry.retire_publisher(&old),
            Err(PublicationError::StaleRegistration)
        );
        successor.check().unwrap();
        fresh.owner.check().unwrap();
        assert_eq!(old.check(), Err(PublicationError::StaleRegistration));
        drop(registry);
        assert_eq!(successor.check(), Err(PublicationError::StaleRegistration));
        assert!(fresh.owner.check().is_err());
        assert!(fresh.peers[0].control.check().is_err());
        cleanup(&mut fresh, &cx).await;
    });
}
#[test]
fn identical_numeric_scopes_cannot_replace_sources_or_borrow_another_registrys_ownership() {
    let rt = support::runtime();
    let cx = rt.request_cx_with_budget(Budget::INFINITE);
    rt.block_on(async {
        let mut a = Box::pin(group(&rt, 1)).await;
        let mut b = Box::pin(group(&rt, 1)).await;
        ready(&mut a).await;
        ready(&mut b).await;
        let mut registry = registry();
        let first = registry.register_publisher(&a.publisher).unwrap();
        registry
            .register_publisher(&a.publisher)
            .unwrap()
            .check()
            .unwrap();
        assert_eq!(registry.publication_count().unwrap(), 1);
        assert!(matches!(
            registry.register_publisher(&b.publisher),
            Err(PublicationError::AlreadyPublished)
        ));
        let mut other = self::registry();
        assert_eq!(
            other.retire_publisher(&first),
            Err(PublicationError::StaleRegistration)
        );
        assert!(matches!(
            other.register_publisher(&a.publisher),
            Err(PublicationError::Source(_))
        ));
        let second = other.register_publisher(&b.publisher).unwrap();
        registry.retire_publisher(&first).unwrap();
        assert!(a.owner.check().is_err());
        second.check().unwrap();
        b.owner.check().unwrap();
        b.peers[0].turn().await.unwrap();
        drop(other);
        assert!(b.owner.check().is_err());
        cleanup(&mut a, &cx).await;
        cleanup(&mut b, &cx).await;
    });
}
#[test]
fn wrong_scope_and_missing_publication_refuse_before_affecting_any_live_source() {
    let rt = support::runtime();
    let cx = rt.request_cx_with_budget(Budget::INFINITE);
    rt.block_on(async {
        let mut g = Box::pin(group(&rt, 1)).await;
        ready(&mut g).await;
        let mut wrong =
            SessionRegistry::new(HostBootId::from_raw(11), OsSessionId::from_raw(90), 8);
        assert!(matches!(
            wrong.register_publisher(&g.publisher),
            Err(PublicationError::WrongScope)
        ));
        let mut registry = registry();
        let binding = g.peers[0].media.binding();
        assert!(matches!(
            registry.publisher(binding),
            Err(PublicationError::NotFound)
        ));
        let registration = registry.register_publisher(&g.publisher).unwrap();
        for dimension in 0..5 {
            let mut wrong = binding;
            match dimension {
                0 => wrong.parent.host_boot = HostBootId::from_raw(99),
                1 => wrong.parent.os_session = OsSessionId::from_raw(99),
                2 => wrong.geometry = wrong.geometry.next().unwrap(),
                3 => wrong.configuration = wrong.configuration.next().unwrap(),
                _ => wrong.display += 1,
            }
            assert!(registry.publisher(wrong).is_err());
        }
        // Recovery/viewports and individual remote sessions are independent,
        // never a reason to start another source or borrow another peer's grant.
        let mut same_source = binding;
        same_source.parent.remote_session = RemoteSessionId::from_raw(99);
        same_source.viewport = same_source.viewport.next().unwrap();
        same_source.recovery = same_source.recovery.next().unwrap();
        registry.publisher(same_source).unwrap().check().unwrap();
        registration.check().unwrap();
        g.owner.check().unwrap();
        g.peers[0].turn().await.unwrap();
        cleanup(&mut g, &cx).await;
    });
}
#[test]
fn registered_join_rejects_control_intent_and_unregistered_scope_without_new_members() {
    let rt = support::runtime();
    let cx = rt.request_cx_with_budget(Budget::INFINITE);
    rt.block_on(async {
        let mut g = Box::pin(group(&rt, 1)).await;
        ready(&mut g).await;
        let mut registry = registry();
        registry.register_publisher(&g.publisher).unwrap();
        let mut request = Box::pin(peer(&rt, 14, Role::RequestControl)).await;
        assert!(matches!(
            request.session.take().unwrap().join_registered(
                &registry,
                request.host_media.take().unwrap(),
                SendPolicy::default(),
                Duration::from_secs(2)
            ),
            Err(Error::Order)
        ));
        assert!(request.control.check().is_err());
        let mut late = Box::pin(peer(&rt, 15, Role::Observe)).await;
        let empty = self::registry();
        assert!(matches!(
            late.session.take().unwrap().join_registered(
                &empty,
                late.host_media.take().unwrap(),
                SendPolicy::default(),
                Duration::from_secs(2)
            ),
            Err(Error::PublicationRegistry(PublicationError::NotFound))
        ));
        assert!(late.control.check().is_err());
        assert_eq!(g.publisher.tick().unwrap(), 1);
        g.owner.check().unwrap();
        g.peers[0].turn().await.unwrap();
        request.viewer.close();
        late.viewer.close();
        cleanup(&mut g, &cx).await;
    });
}

#[test]
fn explicit_registry_removal_stops_one_real_session_and_keeps_healthy_renewal_running() {
    let rt = support::runtime();
    let cx = rt.request_cx_with_budget(Budget::INFINITE);
    rt.block_on(async {
        let mut g = Box::pin(group(&rt, 2)).await;
        ready(&mut g).await;
        let pid = g.publisher.worker_id();
        let mut registry = registry();
        let root = registry.register_publisher(&g.publisher).unwrap();
        let first = g.peers[0].media.binding().parent.remote_session;
        let second = g.peers[1].media.binding().parent.remote_session;
        assert_eq!(registry.session_count(), 0, "legacy rows are not authority");
        assert!(
            registry.remove_session(first),
            "actual removal needs no synthetic row"
        );
        assert!(!registry.remove_session(first));
        assert!(g.peers[0].control.check().is_err());
        assert!(g.peers[0].turn().await.is_err());
        root.check().unwrap();
        g.owner.check().unwrap();
        g.peers[1].control.check().unwrap();
        assert_eq!(g.publisher.tick().unwrap(), 1);
        let until = now(&g.peers[1].h).unwrap() + 3_200_000;
        let source = g.publisher.serve(Duration::from_millis(50), |_| {});
        let network = async {
            while now(&g.peers[1].h).unwrap() < until {
                g.peers[1].turn().await.unwrap();
            }
            assert!(
                g.peers[1]
                    .shared
                    .as_ref()
                    .unwrap()
                    .renewed_until()
                    .unwrap()
                    .as_micros()
                    > until
            );
            assert!(registry.remove_session(second));
            assert!(g.peers[1].control.check().is_err());
            assert!(g.owner.check().is_err());
            assert!(root.check().is_err());
        };
        let (result, ()) = Box::pin(support::both(source, network)).await;
        assert!(result.is_err());
        assert_eq!(g.publisher.worker_id(), pid);
        cleanup(&mut g, &cx).await;
    });
}

#[test]
fn removing_a_registered_pending_join_releases_it_without_forcing_or_resetting_capture() {
    let rt = support::runtime();
    let cx = rt.request_cx_with_budget(Budget::INFINITE);
    rt.block_on(async {
        let mut g = Box::pin(group(&rt, 1)).await;
        ready(&mut g).await;
        let mut registry = registry();
        registry.register_publisher(&g.publisher).unwrap();
        let mut late = Box::pin(peer(&rt, 14, Role::Observe)).await;
        let id = late.media.binding().parent.remote_session;
        late.shared = Some(
            late.session
                .take()
                .unwrap()
                .join_registered(
                    &registry,
                    late.host_media.take().unwrap(),
                    SendPolicy::default(),
                    Duration::from_secs(2),
                )
                .unwrap(),
        );
        let usage = g.publisher.physical_usage();
        assert!(registry.remove_session(id));
        assert!(late.control.check().is_err());
        assert!(late.turn().await.is_err());
        let observation = g.publisher.capture_next().await.unwrap();
        assert!(
            observation.unchanged,
            "a removed waiting join cannot spend an IDR"
        );
        assert_eq!((observation.frame, observation.delivered), (0, 1));
        assert!(g.publisher.physical_usage().bytes <= usage.bytes);
        assert!(!registry.remove_session(RemoteSessionId::from_raw(999)));
        g.owner.check().unwrap();
        g.peers[0].turn().await.unwrap();
        assert_eq!(late.frames, [] as [u64; 0]);
        late.viewer.close();
        cleanup(&mut g, &cx).await;
    });
}

#[test]
fn registry_removal_is_scoped_even_when_an_unrelated_registry_reuses_remote_ids() {
    let rt = support::runtime();
    let cx = rt.request_cx_with_budget(Budget::INFINITE);
    rt.block_on(async {
        let mut a = Box::pin(group(&rt, 1)).await;
        let mut b = Box::pin(group(&rt, 1)).await;
        ready(&mut a).await;
        ready(&mut b).await;
        let mut ar = registry();
        let mut br = registry();
        ar.register_publisher(&a.publisher).unwrap();
        let bh = br.register_publisher(&b.publisher).unwrap();
        let id = a.peers[0].media.binding().parent.remote_session;
        assert_eq!(id, b.peers[0].media.binding().parent.remote_session);
        assert!(ar.remove_session(id));
        assert!(a.owner.check().is_err());
        bh.check().unwrap();
        b.owner.check().unwrap();
        b.peers[0].turn().await.unwrap();
        assert!(!ar.remove_session(id));
        cleanup(&mut a, &cx).await;
        cleanup(&mut b, &cx).await;
    });
}
