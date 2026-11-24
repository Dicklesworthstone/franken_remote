//! Observe authority from the native future's destructor, not after its caller
//! returned. Real TLS admission and `SessionAuthority`; no codec/hardware claim.
use super::*;
use crate::session_startup::running::tests::{pair_initialized, run};
use fr_core::ids::{
    CodecConfigurationGeneration, DisplayGeometryGeneration, RecoveryGeneration,
    ViewportMappingGeneration,
};
use std::{cell::Cell, rc::Rc, task::Context};

struct PendingNative {
    control: ObservationControl,
    dropped_fenced: Rc<Cell<Option<bool>>>,
    polls: Rc<Cell<usize>>,
}
impl Future for PendingNative {
    type Output = Result<CaptureUpdate, crate::media::Error>;
    fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Self::Output> {
        self.polls.set(self.polls.get() + 1);
        Poll::Pending
    }
}
impl Drop for PendingNative {
    fn drop(&mut self) {
        self.dropped_fenced.set(Some(self.control.check().is_err()));
    }
}
struct Permission(bool);
impl Services for Permission {
    fn permitted(&mut self) -> bool {
        self.0
    }
    fn receive(&mut self, _: Route, _: &[u8]) -> Result<Disposition, ()> {
        Ok(Disposition::Blocked)
    }
}
fn obsolete(session: &HostSession) -> ObsoleteMetadata {
    let parent = session.opened.binding;
    ObsoleteMetadata::new(
        Binding {
            parent,
            display: 1,
            geometry: DisplayGeometryGeneration::INITIAL,
            configuration: CodecConfigurationGeneration::INITIAL,
            recovery: RecoveryGeneration::INITIAL,
            viewport: ViewportMappingGeneration::INITIAL,
        },
        parent,
        fr_wire::MediaLimits::new(
            session.opened.selected.limits,
            1150,
            fr_wire::MAX_FRAGMENTS,
            fr_wire::MAX_REPAIR_RANGES,
        )
        .unwrap(),
        false,
        false,
    )
    .unwrap()
}

#[derive(Clone, Copy)]
enum Failure {
    Deadline,
    Permission,
    Network,
}
fn early_failure(failure: Failure) {
    run(move |c, h| async move {
        let (mut session, mut viewer) = pair_initialized(&c, &h, vec![], |_| {}).await;
        let control = session.observation().unwrap();
        control.check().unwrap();
        let routes = session.opened.routes;
        let mut permission = Permission(!matches!(failure, Failure::Permission));
        let mut path = crate::media::shared_publisher::path_queue::PathQueue::default();
        let mut waiting = Waiting {
            control: &control,
            until: now(&h).unwrap()
                + if matches!(failure, Failure::Deadline) {
                    0
                } else {
                    2_000_000
                },
            routes,
            obsolete: obsolete(&session),
            repair: Route::Stream(routes.inbound),
            other: &mut permission,
            path: &mut path,
            bitrate: None,
        };
        let dropped = Rc::new(Cell::new(None));
        let polls = Rc::new(Cell::new(0));
        let native = PendingNative {
            control: control.clone(),
            dropped_fenced: dropped.clone(),
            polls: polls.clone(),
        };
        // The admitted authority is live. The absent host owner supplies a
        // deterministic network error after the native future has been polled.
        let mut host = Host::Closed;
        let error = during(
            &mut host,
            &mut None,
            &mut |_| Ok(None),
            Policy::default(),
            &mut waiting,
            &mut || Ok(100),
            &mut || None,
            native,
        )
        .await
        .expect_err("the recovery cannot complete");
        assert_eq!(
            error,
            match failure {
                Failure::Deadline => Error::Expired,
                Failure::Permission => Error::Authority,
                Failure::Network => Error::Closed,
            }
        );
        assert_eq!(
            polls.get(),
            usize::from(matches!(failure, Failure::Network))
        );
        assert_eq!(
            dropped.get(),
            Some(true),
            "authority must end BEFORE native cleanup, not in the outer caller"
        );
        session.close();
        viewer.close();
    });
}
#[test]
fn an_already_expired_recovery_fences_before_dropping_unpolled_native_work() {
    early_failure(Failure::Deadline);
}
#[test]
fn permission_refusal_fences_before_dropping_unpolled_native_work() {
    early_failure(Failure::Permission);
}
#[test]
fn network_failure_fences_before_dropping_pending_native_work() {
    early_failure(Failure::Network);
}

#[test]
fn cancelling_an_in_flight_handoff_fences_before_the_native_future_is_dropped() {
    run(|c, h| async move {
        let (mut session, mut viewer) = pair_initialized(&c, &h, vec![], |_| {}).await;
        let control = session.observation().unwrap();
        let routes = session.opened.routes;
        let mut permission = Permission(true);
        let mut path = crate::media::shared_publisher::path_queue::PathQueue::default();
        let mut waiting = Waiting {
            control: &control,
            until: now(&h).unwrap() + 2_000_000,
            routes,
            obsolete: obsolete(&session),
            repair: Route::Stream(routes.inbound),
            other: &mut permission,
            path: &mut path,
            bitrate: None,
        };
        let dropped = Rc::new(Cell::new(None));
        let polls = Rc::new(Cell::new(0));
        let native = PendingNative {
            control: control.clone(),
            dropped_fenced: dropped.clone(),
            polls: polls.clone(),
        };
        let mut host = Host::Observe(session);
        let mut entropy = || Ok(101);
        let mut ticket = || None;
        let mut acquisition = None;
        let mut local = no_local_control;
        let mut operation = Box::pin(during(
            &mut host,
            &mut acquisition,
            &mut local,
            Policy::default(),
            &mut waiting,
            &mut entropy,
            &mut ticket,
            native,
        ));
        poll_fn(|task| match operation.as_mut().poll(task) {
            Poll::Pending => Poll::Ready(()),
            Poll::Ready(value) => panic!("native operation must remain pending: {value:?}"),
        })
        .await;
        assert!(polls.get() > 0);
        assert_eq!(dropped.get(), None);
        control.check().unwrap();
        drop(operation);
        assert_eq!(dropped.get(), Some(true));
        assert!(control.check().is_err());
        host.close();
        viewer.close();
    });
}

fn recovery_path_service(allowed: bool, expired: bool) {
    run(move |c, h| async move {
        let (mut session, mut viewer) = pair_initialized(&c, &h, vec![], |_| {}).await;
        let control = session.observation().unwrap();
        let current = now(&h).unwrap();
        let routes = session.opened.routes;
        let mut permission = Permission(allowed);
        let mut path = crate::media::shared_publisher::path_queue::PathQueue::default();
        let mut bitrate = fr_media::bitrate::Controller::new(2_000_000).unwrap();
        // Seed the old path's congested observation, then service the actual
        // unchanged TLS/UDP owner's current snapshot. No I/O occurs between
        // reading the low RTT here and Waiting's ordinary sampling below.
        let low = session
            .opened
            .transport
            .path_load()
            .map_or(5_000, |load| load.latest_rtt_us);
        let mut old = fr_transport::quic::PathLoad {
            latest_rtt_us: low,
            smoothed_rtt_us: low,
            rtt_variation_us: 1,
            bytes_in_flight: 24_000,
            packets_acked: 0,
            packets_lost: 0,
            busy_packets_acked: 0,
            busy_acked_at_us: None,
        };
        path.observe(Some(old), current);
        old.latest_rtt_us += 200_000;
        old.smoothed_rtt_us = old.latest_rtt_us;
        old.packets_acked = 1;
        path.observe(Some(old), current);
        assert!(path.queued());
        let mut waiting = Waiting {
            control: &control,
            until: if expired {
                current
            } else {
                current + 2_000_000
            },
            routes,
            obsolete: obsolete(&session),
            repair: Route::Stream(routes.inbound),
            other: &mut permission,
            path: &mut path,
            bitrate: Some(&mut bitrate),
        };
        let result = waiting.maintain(&mut session.opened.transport, &mut || Ok(100));
        assert_eq!(
            result,
            if expired {
                Err(Error::Expired)
            } else if allowed {
                Ok(())
            } else {
                Err(Error::Authority)
            }
        );
        assert_eq!(waiting.path.queued(), expired || !allowed);
        assert!(waiting.bitrate.as_ref().unwrap().pending().is_none());
        session.close();
        viewer.close();
    });
}

#[test]
fn an_authorized_recovery_wait_samples_its_original_live_path() {
    recovery_path_service(true, false);
}

#[test]
fn recovery_path_sampling_stops_at_permission_refusal() {
    recovery_path_service(false, false);
}

#[test]
fn recovery_path_sampling_stops_at_the_original_deadline() {
    recovery_path_service(true, true);
}

/// An observation session's recovery: no control acquisition to service.
/// The `Result` is the callback's required signature.
#[allow(clippy::unnecessary_wraps)]
fn no_local_control(
    _: acquisition::LocalControl<'_>,
) -> Result<Option<fr_wire::control::Target>, crate::input_quic::grant::Error> {
    Ok(None)
}
