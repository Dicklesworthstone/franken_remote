//! Production startup, attachment, clock and grant broker over real TLS/UDP.
//! Local consent/readiness and the counted OS sink are explicit test fixtures.
use super::*;
use crate::{
    input_agent::{Driver, Seat},
    input_quic::{
        QuicInput,
        grant::{Event, GrantBroker},
    },
    input_watchdog::StopReason,
    media::ObservationControl,
    session_startup::{
        HostSession,
        running::{
            controlled::tests::attach,
            tests::{pair_initialized, run},
        },
        tests::support,
    },
};
use asupersync::cx::Cx;
use fr_client::input::Action;
use fr_core::{
    ids::*,
    input::*,
    input_submission::{Capabilities, Capability, InputSink, Operation, PlatformError, Submission},
    time::HostInstant,
};
use fr_media::freshness::ClockPolicy;
use fr_wire::{
    attachment::{self, MediaRole},
    control::Target,
    negotiation::Capability as WireCapability,
};
use std::{
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    task::Poll,
};

struct Sink(Arc<AtomicUsize>);
impl InputSink for Sink {
    fn prepare(&mut self, _: Operation) -> Result<(), PlatformError> {
        Ok(())
    }
    fn submit(&mut self, _: Operation) -> Submission {
        self.0.fetch_add(1, Ordering::SeqCst);
        Submission::Submitted
    }
}
struct Fixture {
    host: HostSession,
    viewer: ViewerSession,
    host_clock: ClockSync,
    clock: Option<ClockSync>,
    channels: NegotiatedInput,
    broker: GrantBroker,
    observation: ObservationControl,
    seat: Seat,
    request: Request,
}
fn target() -> Target {
    Target {
        display_binding: 8,
        view: InputView {
            geometry: DisplayGeometryGeneration::INITIAL,
            viewport: ViewportMappingGeneration::INITIAL,
            configuration: CodecConfigurationGeneration::INITIAL,
            recovery: RecoveryGeneration::INITIAL,
        },
        bounds: InputBounds::new(DesktopPoint { x: 0, y: 0 }, 320, 240).unwrap(),
        capabilities: Capabilities::default().with(Capability::Keys),
    }
}
async fn fixture(c: &Cx, h: &Cx) -> Fixture {
    Box::pin(fixture_with_clock(c, h, false)).await
}
async fn fixture_with_clock(c: &Cx, h: &Cx, synchronized: bool) -> Fixture {
    let mut capabilities: Vec<_> = [
        fr_wire::clock::CAPABILITY,
        fr_wire::decoder::CAPABILITY,
        attachment::INPUT_CAPABILITY,
        attachment::CAPABILITY,
        attachment::DELIVERY_CAPABILITY,
        crate::input_quic::grant::CAPABILITY,
    ]
    .into_iter()
    .map(|name| WireCapability {
        name: name.into(),
        version: 1,
        required: true,
    })
    .collect();
    capabilities.sort_by(|a, b| a.name.cmp(&b.name));
    let (mut host, mut viewer) = pair_initialized(c, h, capabilities, |host| {
        host.authority
            .as_mut()
            .unwrap()
            .mark_view_ready(HostInstant::from_micros(now(h).unwrap()))
            .unwrap();
    })
    .await;
    let (hc, vc) = attach(&mut host, &mut viewer, c, h, MediaRole::Configuration, 8).await;
    let (hi, vi) = attach(&mut host, &mut viewer, c, h, MediaRole::Input, 10).await;
    let parent = host.binding();
    let selection = host.selection().clone();
    let hn = NegotiatedInput::new(host.io().unwrap().0, &selection, &hc, hi).unwrap();
    let channels = NegotiatedInput::new(viewer.io().unwrap().0, &selection, &vc, vi).unwrap();
    let observation = host.observation().unwrap();
    let (q, routes) = host.io().unwrap();
    let host_clock = ClockSync::host(observation.clone(), q, routes, parent, &selection).unwrap();
    let clock = if synchronized {
        viewer.enable_clock_sync(ClockPolicy::default()).unwrap();
        None
    } else {
        let (q, routes) = viewer.io().unwrap();
        Some(
            ClockSync::viewer(
                c.clone(),
                q,
                routes,
                parent,
                &selection,
                ClockPolicy::default(),
            )
            .unwrap(),
        )
    };
    let seat = Seat::default();
    let broker = host.negotiated_control_broker(seat.clone(), hn).unwrap();
    Fixture {
        host,
        viewer,
        host_clock,
        clock,
        channels,
        broker,
        observation,
        seat,
        request: Request {
            parent,
            sequence: 1,
            target: target(),
        },
    }
}
async fn poll_driver(driver: &mut Option<Driver>) {
    if let Some(driver) = driver {
        std::future::poll_fn(|task| {
            assert!(Pin::new(&mut *driver).poll(task).is_pending());
            Poll::Ready(())
        })
        .await;
    }
}
async fn host_until_granted(
    host: &mut HostSession,
    clock: &mut ClockSync,
    broker: GrantBroker,
    observation: &ObservationControl,
    done: &AtomicBool,
    effects: &Arc<AtomicUsize>,
    delay_us: u64,
) -> (Option<Driver>, Option<QuicInput>) {
    let cx = observation.context();
    let start = now(&cx).unwrap();
    let mut broker = Some(broker);
    let mut driver = None;
    let mut input = None;
    let mut n = 5000_u128;
    while !done.load(Ordering::SeqCst) {
        assert!(now(&cx).unwrap() < start + 3_000_000);
        poll_driver(&mut driver).await;
        clock
            .receive(host.io().unwrap().0, |_, _| Ok(Disposition::Blocked))
            .unwrap();
        clock.service(host.io().unwrap().0).unwrap();
        let granted = if let Some(broker) = &mut broker {
            broker
                .receive(host.io().unwrap().0, |_, _| Ok(Disposition::Blocked))
                .unwrap();
            if driver.is_none()
                && broker.request().is_some()
                && now(&cx).unwrap() >= start + delay_us
            {
                let effects = effects.clone();
                driver = Some(
                    broker
                        .approve(
                            host.io().unwrap().0,
                            target(),
                            || Some((InputLeaseId::from_raw(19), InputTicketId::from_raw(23))),
                            move || Ok(Sink(effects)),
                            |_| true,
                        )
                        .unwrap(),
                );
            }
            broker
                .service(host.io().unwrap().0, Some(target()))
                .unwrap()
                == Event::GrantQueued
        } else {
            false
        };
        if granted {
            input = Some(
                broker
                    .take()
                    .unwrap()
                    .finish(host.io().unwrap().0, Some(target()))
                    .unwrap(),
            );
        }
        host.drive(
            Duration::from_millis(1),
            || {
                n += 1;
                Ok(n)
            },
            |_, _| Ok(Disposition::Blocked),
        )
        .await
        .unwrap();
    }
    (driver, input)
}

async fn accepted(c: Cx, h: Cx, delay: u64, synchronized: bool) {
    let Fixture {
        mut host,
        mut viewer,
        mut host_clock,
        mut clock,
        channels,
        broker,
        observation,
        seat,
        request,
    } = Box::pin(fixture_with_clock(&c, &h, synchronized)).await;
    let effects = Arc::new(AtomicUsize::new(0));
    let done = AtomicBool::new(false);
    let initial = observation.deadline(Duration::from_secs(3)).unwrap().time();
    let (answer, (driver, native)) = Box::pin(support::both(
        async {
            let answer = match &mut clock {
                Some(clock) => {
                    viewer
                        .request_control(&channels, clock, request, Policy::default())
                        .await
                }
                None => {
                    viewer
                        .request_control_synchronized(&channels, request, Policy::default())
                        .await
                }
            };
            done.store(true, Ordering::SeqCst);
            answer
        },
        host_until_granted(
            &mut host,
            &mut host_clock,
            broker,
            &observation,
            &done,
            &effects,
            delay,
        ),
    ))
    .await;
    let (granted, mut input) = answer.unwrap();
    assert_eq!(granted.request, request);
    assert_eq!(granted.input_channel, channels.channel_binding());
    assert_eq!(granted.lease, InputLeaseId::from_raw(19));
    assert_eq!(granted.ticket, InputTicketId::from_raw(23));
    assert!(input.ticket_deadline().is_some());
    assert!(seat.is_occupied());
    assert!(!viewer.is_closed());
    if let Some(clock) = &mut clock {
        assert!(clock.correlation(viewer.io().unwrap().0).unwrap().is_some());
    } else {
        assert!(viewer.clock_correlation().unwrap().is_some());
        assert_eq!(
            viewer.enable_clock_sync(ClockPolicy::default()),
            Err(crate::media::clock::Error::Configuration)
        );
    }
    if delay > 1_000_000 {
        assert!(observation.deadline(Duration::from_secs(3)).unwrap().time() > initial);
    }
    let action = Action::Key {
        key: PhysicalKey::new(4).unwrap(),
        transition: KeyTransition::Press,
    };
    assert_eq!(
        input.action(action, &mut [0; 256], ClientInstant(now(&c).unwrap())),
        Err(fr_client::input::Error::MappingUnconfirmed)
    );
    assert_eq!(effects.load(Ordering::SeqCst), 0);
    let native = native.unwrap();
    native.control().stop(StopReason::LocalRevoke);
    assert!(driver.unwrap().await.handoff_safe());
    assert!(!seat.is_occupied());
    assert_eq!(effects.load(Ordering::SeqCst), 0);
}
#[test]
fn real_broker_grant_is_bound_to_input_without_inventing_presentation() {
    run(|c, h| accepted(c, h, 0, false));
}
#[test]
fn delayed_local_approval_keeps_observation_and_clock_alive() {
    run(|c, h| accepted(c, h, 1_100_000, false));
}
#[test]
fn unpolled_request_drop_closes_original_session_without_reserving_seat() {
    run(|c, h| async move {
        let mut f = Box::pin(fixture(&c, &h)).await;
        let request = f.viewer.request_control(
            &f.channels,
            f.clock.as_mut().unwrap(),
            f.request,
            Policy::default(),
        );
        drop(request);
        assert!(f.viewer.is_closed());
        assert!(!f.seat.is_occupied());
        assert!(f.broker.request().is_none());
        assert!(
            f.clock
                .as_mut()
                .unwrap()
                .correlation(&mut f.viewer.transport)
                .is_err()
        );
    });
}
#[test]
fn wrong_session_request_refuses_before_native_authority() {
    run(|c, h| async move {
        let mut f = Box::pin(fixture(&c, &h)).await;
        f.request.parent.remote_session = RemoteSessionId::from_raw(99);
        let result = f
            .viewer
            .request_control(
                &f.channels,
                f.clock.as_mut().unwrap(),
                f.request,
                Policy::default(),
            )
            .await;
        assert!(matches!(result, Err(Error::WrongBinding)));
        assert!(f.viewer.is_closed());
        assert!(!f.seat.is_occupied());
        assert!(f.broker.request().is_none());
    });
}
#[test]
fn request_deadline_includes_time_before_its_first_poll() {
    run(|c, h| async move {
        let mut f = Box::pin(fixture(&c, &h)).await;
        let request = f.viewer.request_control(
            &f.channels,
            f.clock.as_mut().unwrap(),
            f.request,
            Policy::default(),
        );
        asupersync::time::sleep(c.now(), Duration::from_millis(2050)).await;
        assert!(matches!(request.await, Err(Error::Expired)));
        assert!(f.viewer.is_closed());
        assert!(!f.seat.is_occupied());
        assert!(f.broker.request().is_none());
    });
}

#[test]
fn session_owned_clock_survives_grant_and_rejects_duplicate_ownership() {
    run(|c, h| accepted(c, h, 0, true));
}
#[test]
fn session_owned_clock_and_observation_renew_during_local_approval() {
    run(|c, h| accepted(c, h, 1_100_000, true));
}
#[test]
fn synchronized_request_does_not_create_an_unconfigured_clock() {
    run(|c, h| async move {
        let mut f = Box::pin(fixture(&c, &h)).await;
        let result = f
            .viewer
            .request_control_synchronized(&f.channels, f.request, Policy::default())
            .await;
        assert!(matches!(result, Err(Error::ClockNotReady)));
        assert!(f.viewer.is_closed());
        assert!(!f.seat.is_occupied());
        assert!(f.broker.request().is_none());
    });
}
#[test]
fn dropping_unpolled_synchronized_request_fences_its_owned_clock() {
    run(|c, h| async move {
        let mut f = Box::pin(fixture_with_clock(&c, &h, true)).await;
        assert!(f.viewer.clock.is_some());
        drop(
            f.viewer
                .request_control_synchronized(&f.channels, f.request, Policy::default()),
        );
        assert!(f.viewer.is_closed());
        assert!(f.viewer.clock.is_none());
        assert!(!f.seat.is_occupied());
        assert!(f.broker.request().is_none());
    });
}
#[test]
fn synchronized_request_deadline_also_starts_before_polling() {
    run(|c, h| async move {
        let mut f = Box::pin(fixture_with_clock(&c, &h, true)).await;
        let request =
            f.viewer
                .request_control_synchronized(&f.channels, f.request, Policy::default());
        asupersync::time::sleep(c.now(), Duration::from_millis(2050)).await;
        assert!(matches!(request.await, Err(Error::Expired)));
        assert!(f.viewer.is_closed());
        assert!(!f.seat.is_occupied());
        assert!(f.broker.request().is_none());
    });
}
