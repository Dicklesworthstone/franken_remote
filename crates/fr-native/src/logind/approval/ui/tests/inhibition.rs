//! Actual X11 sink and consent window, original authority and TLS/UDP approval.
//! Identity, logind and device-attributed local clicks remain explicit fixtures.
use super::*;
use crate::input::X11Pointer;
use fr_core::{
    authority::{AuthorityPolicy, SessionAuthority},
    ids::*,
    input::{InputCredentials, InputEvent, InputRequest, InputView, PointerButton},
    input_sequence::InputOutcome,
    input_submission::{
        Capabilities, Capability, Dispatch, InputSession, InputSink, Operation, PlatformError,
        Submission,
    },
    limits::ProtocolLimits,
};
use fr_wire::input::{InputDelivery, InputDirection, MAX_INPUT_RECORD_BYTES, encode_input};
use frd::{
    input_agent::{Agent, Driver, Error as InputError, Reply, Route},
    input_watchdog::{StopReason, host_now},
    session_agent::source::desktop::ControlProfile,
};
use std::{path::Path, sync::mpsc};

fn credentials() -> InputCredentials {
    InputCredentials {
        session: RemoteSessionId::from_raw(71),
        lease: InputLeaseId::from_raw(72),
        ticket: InputTicketId::from_raw(73),
        view: InputView {
            geometry: DisplayGeometryGeneration::INITIAL,
            viewport: ViewportMappingGeneration::INITIAL,
            configuration: CodecConfigurationGeneration::INITIAL,
            recovery: RecoveryGeneration::INITIAL,
        },
    }
}
fn capabilities() -> Capabilities {
    Capabilities::default()
        .with(Capability::Absolute)
        .with(Capability::Buttons)
}
fn input(cx: &Cx) -> InputSession {
    let c = credentials();
    let now = host_now(cx).unwrap();
    let mut authority = SessionAuthority::new(c.session, AuthorityPolicy::plan_defaults());
    authority.mark_capabilities_checked().unwrap();
    authority.authorize_observation(now).unwrap();
    authority.mark_view_ready(now).unwrap();
    authority.grant_lease(c.lease, now).unwrap();
    authority
        .issue_input_ticket(c.lease, c.ticket, now)
        .unwrap();
    InputSession::new(
        authority,
        c,
        InputBounds::new(DesktopPoint { x: 0, y: 0 }, 800, 600).unwrap(),
        capabilities(),
        now,
    )
    .unwrap()
}
fn route() -> Route {
    Route::new(7, ProtocolLimits::ABSOLUTE)
}

// The injected operations are REAL XTest. Only native-destructor timing is
// controlled so the mapping/cleanup race is deterministic, not a sleep guess.
struct HeldNative {
    sink: Option<X11Pointer>,
    entered: mpsc::SyncSender<()>,
    resume: mpsc::Receiver<()>,
    destroyed: Arc<AtomicBool>,
}
impl InputSink for HeldNative {
    fn prepare(&mut self, operation: Operation) -> Result<(), PlatformError> {
        self.sink.as_mut().unwrap().prepare(operation)
    }
    fn submit(&mut self, operation: Operation) -> Submission {
        self.sink.as_mut().unwrap().submit(operation)
    }
    fn cancel_prepared(&mut self) {
        self.sink.as_mut().unwrap().cancel_prepared();
    }
}
impl Drop for HeldNative {
    fn drop(&mut self) {
        self.entered.send(()).unwrap();
        self.resume.recv_timeout(Duration::from_secs(5)).unwrap();
        drop(self.sink.take());
        self.destroyed.store(true, Ordering::Release);
    }
}
struct Native {
    agent: Agent,
    driver: Driver,
    entered: mpsc::Receiver<()>,
    resume: mpsc::SyncSender<()>,
    destroyed: Arc<AtomicBool>,
}
fn native(rt: &Runtime, seat: &Seat, display: String) -> Native {
    let cx = rt.request_cx_with_budget(Budget::INFINITE);
    let (entered, wait) = mpsc::sync_channel(1);
    let (resume, resumed) = mpsc::sync_channel(1);
    let destroyed = Arc::new(AtomicBool::new(false));
    let done = destroyed.clone();
    let (agent, driver) = seat
        .start(
            cx.clone(),
            input(&cx),
            route(),
            move || {
                Ok(HeldNative {
                    sink: Some(X11Pointer::open(&display)?),
                    entered,
                    resume: resumed,
                    destroyed: done,
                })
            },
            |owner| owner.sink.as_mut().unwrap().cleanup_native(),
        )
        .unwrap();
    Native {
        agent,
        driver,
        entered: wait,
        resume,
        destroyed,
    }
}
async fn press(agent: &mut Agent) {
    let mut bytes = [0; MAX_INPUT_RECORD_BYTES];
    let size = encode_input(
        InputRequest {
            credentials: credentials(),
            sequence: 0,
            event: InputEvent::Button {
                button: PointerButton::Primary,
                pressed: true,
                position: DesktopPoint { x: 600, y: 400 },
                barrier: 0,
            },
        },
        &mut bytes,
        &ProtocolLimits::ABSOLUTE,
        7,
        InputDirection::ViewerToHost,
        InputDelivery::Reliable,
    )
    .unwrap();
    agent
        .submit(&bytes[..size], InputDelivery::Reliable)
        .unwrap();
    assert!(matches!(agent.response().await.unwrap(),
        Reply::Input(Ok(Dispatch::Completed(r))) if r.outcome == InputOutcome::SubmittedToOs));
}
fn no_successor(seat: &Seat, cx: &Cx) {
    assert!(matches!(
        seat.start(
            cx.clone(),
            input(cx),
            route(),
            || -> Result<X11Pointer, PlatformError> {
                panic!("consent allowed a new input factory")
            },
            X11Pointer::cleanup_native
        ),
        Err(InputError::SeatInhibited)
    ));
}

#[test]
#[ignore = "isolated network and 800x600 Xvfb; actual XTest sink"]
fn control_profile_seat_is_fenced_before_consent_mapping_and_retained_through_decision() {
    run(async |rt, broker, session, display| {
        let seat = Seat::default();
        let mut old = native(rt, &seat, display.clone());
        press(&mut old.agent).await;
        let profile = ControlProfile::new(
            Path::new("/usr/libexec/fr-input-agent"),
            &display,
            None,
            seat.clone(),
            capabilities(),
            15,
            2_000_000,
            fr_media::worker::Backend::SoftwareExplicit,
        )
        .unwrap();
        let original = agent(2).with_control(profile);
        let mut ui = ApprovalUi::new(session, &original).unwrap();
        let local = attempt(rt, broker, ui.callback(), async |approval| {
            until(broker, || old.entered.try_recv().is_ok()).await;
            assert!(old.agent.control().is_stopped());
            assert_eq!(old.agent.control().reason(), Some(StopReason::Suspended));
            let prompt = ui.current().unwrap();
            assert_eq!(prompt.status(), Status::Opening);
            assert_eq!(
                prompt.window(),
                None,
                "no consent target exists during old input cleanup"
            );
            assert!(!old.destroyed.load(Ordering::Acquire));
            no_successor(&seat, broker);
            old.resume.send(()).unwrap();
            let prompt = mapped(broker, &ui).await;
            assert!(old.destroyed.load(Ordering::Acquire));
            no_successor(&seat, broker);
            assert!(approval.check_pending().is_ok());
            interaction(broker, &display, prompt.window().unwrap(), "device-allow").await;
        });
        let (shutdown, allowed) = Box::pin(network::both(old.driver, local)).await;
        assert!(allowed);
        assert!(shutdown.handoff_safe());
        assert_eq!(
            collect(broker, &mut ui).await,
            Outcome::Allowed(Role::Observe)
        );
        assert!(!seat.is_occupied());
        assert!(
            old.agent.control().is_stopped(),
            "consent never resumes old input"
        );
        assert!(
            !original.is_revoked(),
            "input inhibition does not revoke the whole OS share"
        );
    });
}

#[test]
#[ignore = "isolated network and 800x600 Xvfb; actual XTest sink"]
fn stalled_native_retirement_refuses_consent_without_ever_creating_a_window() {
    run(async |rt, broker, session, display| {
        let seat = Seat::default();
        let mut old = native(rt, &seat, display);
        press(&mut old.agent).await;
        let original = agent(2);
        let mut ui = ApprovalUi::with_input_seat(session, &original, seat.clone()).unwrap();
        let local = attempt(rt, broker, ui.callback(), async |_| {
            until(broker, || old.entered.try_recv().is_ok()).await;
            let prompt = ui.current().unwrap();
            until(broker, || matches!(prompt.status(), Status::Finished(_))).await;
            assert_eq!(
                prompt.status(),
                Status::Finished(Outcome::Refused(Error::InputCleanupExpired))
            );
            assert_eq!(prompt.window(), None);
            assert!(seat.is_occupied());
            assert!(!old.destroyed.load(Ordering::Acquire));
            old.resume.send(()).unwrap();
        });
        let (shutdown, allowed) = Box::pin(network::both(old.driver, local)).await;
        assert!(!allowed);
        assert!(
            !shutdown.handoff_safe(),
            "a timed-out driver cannot retroactively claim native exit"
        );
        assert_eq!(
            collect(broker, &mut ui).await,
            Outcome::Refused(Error::InputCleanupExpired)
        );
        until(broker, || !seat.is_occupied()).await;
        assert!(old.destroyed.load(Ordering::Acquire));
    });
}

#[test]
#[ignore = "isolated network and Xvfb"]
fn stopping_a_mapped_prompt_keeps_input_excluded_until_native_retirement() {
    run(async |rt, broker, session, display| {
        let seat = Seat::default();
        let original = agent(2);
        let mut ui = ApprovalUi::with_input_seat(session, &original, seat.clone()).unwrap();
        assert!(
            !attempt(rt, broker, ui.callback(), async |_| {
                let _prompt = mapped(broker, &ui).await;
                no_successor(&seat, broker);
                ui.stop();
            })
            .await
        );
        assert_eq!(
            collect(broker, &mut ui).await,
            Outcome::Refused(Error::Cancelled)
        );
        // Prompt cleanup did not occupy or release somebody else's native seat.
        assert!(!seat.is_occupied());
        let fresh = native(rt, &seat, display);
        fresh.resume.send(()).unwrap();
        fresh.agent.control().stop(StopReason::LocalRevoke);
        assert!(fresh.driver.await.handoff_safe());
        assert!(fresh.destroyed.load(Ordering::Acquire));
        assert!(!original.is_revoked());
    });
}

#[test]
#[ignore = "isolated network and Xvfb"]
fn an_explicit_seat_cannot_replace_the_control_profiles_canonical_owner() {
    run(async |_, _, session, display| {
        let seat = Seat::default();
        let profile = ControlProfile::new(
            Path::new("/usr/libexec/fr-input-agent"),
            &display,
            None,
            seat.clone(),
            capabilities(),
            15,
            2_000_000,
            fr_media::worker::Backend::SoftwareExplicit,
        )
        .unwrap();
        let original = agent(2).with_control(profile);
        assert!(matches!(
            ApprovalUi::with_input_seat(session.clone(), &original, Seat::default()),
            Err(Error::WrongInputSeat)
        ));
        let ui = ApprovalUi::with_input_seat(session, &original, seat.clone()).unwrap();
        assert!(ui.current().is_none());
        assert!(!WORKER.load(Ordering::Acquire));
        assert!(seat.same_owner(&original.control_profile().unwrap().seat()));
    });
}

#[test]
#[ignore = "isolated network and 800x600 Xvfb; actual XTest sink"]
fn input_fence_can_cancel_pending_approval_without_reentering_a_locked_slot() {
    run(async |rt, broker, session, display| {
        let seat = Seat::default();
        let mut old = native(rt, &seat, display);
        press(&mut old.agent).await;
        // Retirement is not intentionally stalled in this regression.
        old.resume.send(()).unwrap();
        let original = agent(2);
        let mut ui = ApprovalUi::with_input_seat(session, &original, seat).unwrap();
        let state = ui.0.clone();
        let called = Arc::new(AtomicBool::new(false));
        let unlocked = Arc::new(AtomicBool::new(false));
        let invoked = called.clone();
        let available = unlocked.clone();
        assert!(old.agent.control().install_fence(Box::new(move || {
            // Check rather than block on the old implementation, so its failure
            // is a bounded assertion instead of a wedged test/runtime thread.
            if invoked.swap(true, Ordering::AcqRel) {
                return;
            }
            let free = state.slot.try_lock().is_ok();
            available.store(free, Ordering::Release);
            if free {
                // A genuine reentrant local stop, never a second input stop.
                state.stop();
            }
        })));
        assert!(
            !attempt(rt, broker, ui.callback(), async |_| {
                until(broker, || called.load(Ordering::Acquire)).await;
                ui.stop();
            })
            .await
        );
        if ui.current().is_some() {
            let _ = collect(broker, &mut ui).await;
        }
        assert!(old.driver.await.handoff_safe());
        assert!(old.destroyed.load(Ordering::Acquire));
        assert!(
            unlocked.load(Ordering::Acquire),
            "the native input fence ran under the approval slot mutex"
        );
        assert!(ui.current().is_none());
        assert!(!original.is_revoked());
    });
}
