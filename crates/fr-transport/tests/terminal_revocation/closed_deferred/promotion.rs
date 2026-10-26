//! Promotion uses actual TLS/UDP custody. The lease and synchronous fences are
//! explicit local fixtures, not native-input or consent qualification.
use super::*;

fn promote(p: &mut Pair, previous: &ClosedRegistration) -> (RevocationReport, Arc<AtomicBool>) {
    let reporting = RevocationReport::default();
    let fenced = Arc::new(AtomicBool::new(false));
    let flag = fenced.clone();
    p.server
        .promote_closed_report(
            &p.server.binding(),
            p.control,
            binding(),
            report().lease,
            previous,
            reporting.registration(),
            move || {
                assert!(!flag.swap(true, Ordering::AcqRel), "one lease fence");
                Reason::LocalRevoke
            },
        )
        .unwrap();
    (reporting, fenced)
}

#[test]
fn actual_lease_custody_supersedes_closed_without_revoking_observation_or_sending_twice() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut p = pair(&cx).await;
        let (closed, observation) = arm(&mut p, &cx);
        let (revocation, lease) = promote(&mut p, &closed.registration());
        assert!(!observation.load(Ordering::Acquire));
        assert!(!lease.load(Ordering::Acquire));
        assert_eq!(p.server.tick(&cx, || true), Ok(()));
        assert_eq!(closed.finish(outcome()).await, None);
        p.server.close();
        assert!(lease.load(Ordering::Acquire));
        assert!(!observation.load(Ordering::Acquire));
        super::super::deferred::delivered(&mut p, &cx, revocation).await;
    });
}

#[test]
fn foreign_connection_slot_route_and_session_cannot_retire_the_original_fence() {
    for wrong in 0..4 {
        runtime().block_on(async {
            let cx = Cx::current().unwrap();
            let mut p = pair(&cx).await;
            let mut other = pair(&cx).await;
            let (closed, fenced) = arm(&mut p, &cx);
            let (foreign, foreign_fence) = arm(&mut other, &cx);
            let replacement = RevocationReport::default();
            let original = if wrong == 0 {
                other.server.binding()
            } else {
                p.server.binding()
            };
            let previous = if wrong == 1 {
                foreign.registration()
            } else {
                closed.registration()
            };
            let route = if wrong == 2 { p.bulk } else { p.control };
            let session = Binding {
                session: RemoteSessionId::from_raw(if wrong == 3 { 999 } else { 1 }),
                ..binding()
            };
            assert_eq!(
                p.server.promote_closed_report(
                    &original,
                    route,
                    session,
                    report().lease,
                    &previous,
                    replacement.registration(),
                    || panic!("a refused replacement must not run"),
                ),
                Err(Error::WrongRoute)
            );
            assert_eq!(replacement.finish().await, None);
            assert!(!fenced.load(Ordering::Acquire));
            assert!(!foreign_fence.load(Ordering::Acquire));
            assert_eq!(p.server.tick(&cx, || true), Ok(()));
            assert_eq!(other.server.tick(&cx, || true), Ok(()));
            p.server.close();
            delivered(&mut p, &cx, closed).await;
            assert!(fenced.load(Ordering::Acquire));
            assert!(!foreign_fence.load(Ordering::Acquire));
            other.server.close();
            assert!(foreign_fence.load(Ordering::Acquire));
        });
    }
}

#[test]
fn already_used_replacement_cannot_disarm_the_original_report() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut p = pair(&cx).await;
        let mut other = pair(&cx).await;
        let (closed, fenced) = arm(&mut p, &cx);
        let (other_closed, _) = arm(&mut other, &cx);
        let (replacement, other_fence) = promote(&mut other, &other_closed.registration());
        assert_eq!(other_closed.finish(outcome()).await, None);
        assert_eq!(
            p.server.promote_closed_report(
                &p.server.binding(),
                p.control,
                binding(),
                report().lease,
                &closed.registration(),
                replacement.registration(),
                || panic!("duplicate registration"),
            ),
            Err(Error::InvalidPolicy)
        );
        assert!(!fenced.load(Ordering::Acquire));
        assert!(!other_fence.load(Ordering::Acquire));
        p.server.close();
        delivered(&mut p, &cx, closed).await;
        other.server.close();
        super::super::deferred::delivered(&mut other, &cx, replacement).await;
    });
}

#[test]
fn finished_or_abandoned_consumers_cannot_be_resurrected_by_promotion() {
    for finish_previous in [false, true] {
        runtime().block_on(async {
            let cx = Cx::current().unwrap();
            let mut p = pair(&cx).await;
            let (closed, fenced) = arm(&mut p, &cx);
            let previous = closed.registration();
            let replacement = RevocationReport::default();
            let next = replacement.registration();
            if finish_previous {
                assert_eq!(closed.finish(outcome()).await, Some(Err(Error::Closed)));
                assert_eq!(
                    p.server.promote_closed_report(
                        &p.server.binding(),
                        p.control,
                        binding(),
                        report().lease,
                        &previous,
                        next,
                        || panic!("finished original consumer"),
                    ),
                    Err(Error::Closed)
                );
                assert_eq!(replacement.finish().await, None);
            } else {
                drop(replacement);
                assert_eq!(
                    p.server.promote_closed_report(
                        &p.server.binding(),
                        p.control,
                        binding(),
                        report().lease,
                        &previous,
                        next,
                        || panic!("abandoned replacement consumer"),
                    ),
                    Err(Error::Closed)
                );
                p.server.close();
                delivered(&mut p, &cx, closed).await;
            }
            p.server.close();
            assert!(fenced.load(Ordering::Acquire));
        });
    }
}

#[test]
fn promotion_preserves_the_original_cleanup_cancellation_and_terminal_security() {
    for cancel in [false, true] {
        let rt = runtime();
        let session = rt.request_cx_with_budget(asupersync::types::Budget::INFINITE);
        let cleanup = rt.request_cx_with_budget(asupersync::types::Budget::INFINITE);
        rt.block_on(async {
            let mut p = pair(&session).await;
            let allowed = Arc::new(AtomicBool::new(true));
            let guard = allowed.clone();
            p.server
                .retain_lifetime_check(&session, Arc::new(move || guard.load(Ordering::Acquire)))
                .unwrap();
            let (closed, observation) = arm(&mut p, &cleanup);
            let (revocation, lease) = promote(&mut p, &closed.registration());
            assert_eq!(closed.finish(outcome()).await, None);
            p.server.close();
            if cancel {
                cleanup.cancel_fast(CancelKind::User);
            } else {
                allowed.store(false, Ordering::Release);
            }
            assert_eq!(
                revocation.finish().await,
                Some(Err(if cancel {
                    Error::Cancelled
                } else {
                    Error::Unauthorized
                }))
            );
            assert!(lease.load(Ordering::Acquire));
            assert!(!observation.load(Ordering::Acquire));
            assert!(!session.is_cancel_requested());
        });
    }
}

#[test]
fn abandoning_a_promoted_owner_fences_only_the_actual_lease() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut p = pair(&cx).await;
        let (closed, observation) = arm(&mut p, &cx);
        let (revocation, lease) = promote(&mut p, &closed.registration());
        drop(p.server);
        assert!(lease.load(Ordering::Acquire));
        assert!(!observation.load(Ordering::Acquire));
        assert_eq!(closed.finish(outcome()).await, None);
        assert_eq!(revocation.finish().await, Some(Err(Error::Closed)));
    });
}

#[test]
fn promotion_cannot_replace_a_captured_socket_or_restart_its_original_deadline() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut p = pair(&cx).await;
        let (closed, observation) = arm(&mut p, &cx);
        let replacement = RevocationReport::default();
        let original = p.server.binding();
        p.server.close();
        assert!(observation.load(Ordering::Acquire));
        assert_eq!(
            p.server.promote_closed_report(
                &original,
                p.control,
                binding(),
                report().lease,
                &closed.registration(),
                replacement.registration(),
                || panic!("captured socket cannot be promoted"),
            ),
            Err(Error::InvalidPolicy)
        );
        assert_eq!(replacement.finish().await, None);
        asupersync::time::sleep(cx.now(), Duration::from_millis(260)).await;
        assert_eq!(closed.finish(outcome()).await, Some(Err(Error::Expired)));
    });
}

#[test]
fn confirmation_requires_the_exact_promoted_slot_and_never_replaces_its_fence() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut p = pair(&cx).await;
        let (closed, observation) = arm(&mut p, &cx);
        let previous = closed.registration();
        let (revocation, lease) = promote(&mut p, &previous);
        assert_eq!(closed.finish(outcome()).await, None);
        let other = ClosedReport::default();
        let foreign = other.registration();
        for wrong in 0..3 {
            assert_eq!(
                p.server.confirm_revocation_report(
                    &p.server.binding(),
                    p.control,
                    binding(),
                    InputLeaseId::from_raw(if wrong == 0 { 999 } else { 2 }),
                    if wrong == 1 {
                        None
                    } else {
                        Some(if wrong == 2 { &foreign } else { &previous })
                    },
                    &revocation.registration(),
                ),
                Err(Error::WrongRoute)
            );
        }
        assert_eq!(
            p.server.confirm_revocation_report(
                &p.server.binding(),
                p.control,
                binding(),
                report().lease,
                Some(&previous),
                &revocation.registration(),
            ),
            Ok(())
        );
        assert_eq!(
            p.server.promote_closed_report(
                &p.server.binding(),
                p.control,
                binding(),
                report().lease,
                &previous,
                revocation.registration(),
                || panic!("confirmation cannot be a replacement"),
            ),
            Err(Error::InvalidPolicy)
        );
        assert!(!observation.load(Ordering::Acquire));
        assert!(!lease.load(Ordering::Acquire));
        p.server.close();
        assert!(lease.load(Ordering::Acquire));
        super::super::deferred::delivered(&mut p, &cx, revocation).await;
    });
}
