//! Actual UDP/TLS and canonical startup. Admission metadata remains a fixture.
use super::*;
use crate::media::clock;
use fr_media::freshness::ClockPolicy;
use fr_wire::negotiation::Capability;

fn capabilities() -> Vec<Capability> {
    vec![Capability {
        name: fr_wire::clock::CAPABILITY.into(),
        version: fr_wire::clock::VERSION,
        required: true,
    }]
}
async fn enabled(c: &Cx, h: &Cx) -> (HostSession, ViewerSession) {
    let (mut host, mut viewer) = pair_with_capabilities(c, h, capabilities()).await;
    host.enable_clock_sync().unwrap();
    viewer.enable_clock_sync(ClockPolicy::default()).unwrap();
    (host, viewer)
}
#[test]
fn session_clock_requires_negotiation_and_refuses_duplicate_ownership() {
    run(|c, h| async move {
        let (mut host, mut viewer) = pair(&c, &h).await;
        assert_eq!(
            host.enable_clock_sync(),
            Err(clock::Error::CapabilityMissing)
        );
        assert_eq!(
            viewer.enable_clock_sync(ClockPolicy::default()),
            Err(clock::Error::CapabilityMissing)
        );
        assert!(host.check().is_ok() && viewer.check().is_ok());
        assert_eq!(viewer.clock_correlation().unwrap(), None);
        let (mut host, mut viewer) = enabled(&c, &h).await;
        assert_eq!(host.enable_clock_sync(), Err(clock::Error::Configuration));
        assert_eq!(
            viewer.enable_clock_sync(ClockPolicy::default()),
            Err(clock::Error::Configuration)
        );
        assert!(host.check().is_ok() && viewer.check().is_ok());
    });
}
#[test]
fn session_clock_is_measured_and_renewed_by_ordinary_idle_drives() {
    run(|c, h| async move {
        let (mut host, mut viewer) = enabled(&c, &h).await;
        assert_eq!(viewer.clock_correlation().unwrap(), None);
        let control = host.observation().unwrap();
        let identity = host.io().unwrap().0.binding();
        let until = now(&h).unwrap() + 3_200_000;
        let mut n = 0;
        let mut first = None;
        while now(&h).unwrap() < until {
            let (a, b) = Box::pin(support::both(
                host.drive(Duration::from_millis(2), || nonce(&mut n), no_other),
                viewer.drive(Duration::from_millis(2), no_other),
            ))
            .await;
            a.unwrap();
            b.unwrap();
            if first.is_none() {
                first = viewer.clock_correlation().unwrap();
            }
        }
        let first = first.expect("no actual sample received");
        let last = viewer.clock_correlation().unwrap().unwrap();
        assert!(last.received_at_us() > first.received_at_us());
        assert_eq!(last.host_boot(), host.binding().host_boot);
        assert!(control.check().is_ok());
        assert!(host.renewed_until().is_some());
        assert!(host.io().unwrap().0.is_bound_to(&identity));
    });
}
#[test]
fn session_clock_progresses_while_admission_lookup_is_pending() {
    run(|c, h| async move {
        let (mut host, mut viewer) = enabled(&c, &h).await;
        let control = host.observation().unwrap();
        let done = Arc::new(AtomicBool::new(false));
        let refresh_done = done.clone();
        let refresh = async {
            asupersync::time::sleep(h.now(), Duration::from_millis(200)).await;
            refresh_done.store(true, Ordering::Release);
            Ok(())
        };
        let mut other = no_other;
        let mut timed = TimedServices {
            clock: host.clock.as_mut(),
            other: &mut other,
        };
        let mut n = 0;
        let mut measured_before_refresh = false;
        let (result, ()) = Box::pin(support::both(
            pump_refresh(
                &mut host.renewal,
                &mut host.opened.transport,
                refresh,
                RefreshTurn {
                    cx: &h,
                    control: &control,
                    until: now(&h).unwrap() + 500_000,
                    wait: Duration::from_millis(2),
                },
                &mut || nonce(&mut n),
                &mut timed,
            ),
            async {
                while !done.load(Ordering::Acquire) {
                    viewer
                        .drive(Duration::from_millis(2), no_other)
                        .await
                        .unwrap();
                    measured_before_refresh |= !done.load(Ordering::Acquire)
                        && viewer.clock_correlation().unwrap().is_some();
                }
            },
        ))
        .await;
        result.unwrap();
        assert!(
            measured_before_refresh,
            "clock exchange stalled behind identity lookup"
        );
        assert!(control.check().is_ok());
    });
}
#[test]
fn session_clock_failure_and_unpolled_abandonment_fence_the_original_owner() {
    run(|c, h| async move {
        let (mut host, mut viewer) = enabled(&c, &h).await;
        let control = host.observation().unwrap();
        drop(host.drive(Duration::from_millis(2), || Ok(1), no_other));
        assert!(control.check().is_err());
        assert!(host.check().is_err());
        drop(viewer.drive(Duration::from_millis(2), no_other));
        assert!(viewer.clock_correlation().is_err());
        assert!(viewer.is_closed());
    });
}
#[test]
fn session_clock_invalid_policy_does_not_claim_the_connection() {
    run(|c, h| async move {
        let (mut host, mut viewer) = pair_with_capabilities(&c, &h, capabilities()).await;
        let invalid = ClockPolicy {
            valid_for_us: 399_999,
            ..ClockPolicy::default()
        };
        assert_eq!(
            viewer.enable_clock_sync(invalid),
            Err(clock::Error::Configuration)
        );
        assert!(viewer.check().is_ok());
        host.enable_clock_sync().unwrap();
        viewer.enable_clock_sync(ClockPolicy::default()).unwrap();
        let mut n = 0;
        let until = now(&c).unwrap() + 500_000;
        while viewer.clock_correlation().unwrap().is_none() {
            assert!(now(&c).unwrap() < until);
            let (a, b) = Box::pin(support::both(
                host.drive(Duration::from_millis(2), || nonce(&mut n), no_other),
                viewer.drive(Duration::from_millis(2), no_other),
            ))
            .await;
            a.unwrap();
            b.unwrap();
        }
    });
}

#[test]
fn session_clock_wrapper_forwards_input_submission_with_and_without_an_endpoint() {
    struct InputEvents(Vec<u64>);
    impl Services for InputEvents {
        fn input_submitted(&mut self, at_us: u64) {
            self.0.push(at_us);
        }
        fn receive(&mut self, _: Route, _: &[u8]) -> Result<Disposition, ()> {
            Ok(Disposition::Blocked)
        }
    }
    run(|c, h| async move {
        let (mut host, _viewer) = enabled(&c, &h).await;
        let mut events = InputEvents(Vec::new());
        for clock in [None, host.clock.as_mut()] {
            let mut services = TimedServices {
                clock,
                other: &mut events,
            };
            services.input_submitted(17);
            services.input_submitted(23);
        }
        assert_eq!(events.0, [17, 23, 17, 23]);
        assert!(host.observation().unwrap().check().is_ok());
    });
}
