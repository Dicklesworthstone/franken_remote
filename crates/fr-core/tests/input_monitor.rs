use fr_core::{
    authority::{AuthorityPolicy, SessionAuthority},
    ids::*,
    input::*,
    input_sequence::InputOutcome,
    input_submission::*,
    time::HostInstant,
};
use std::{sync::mpsc, thread, time::Duration};
fn setup() -> (InputSession, InputCredentials) {
    let c = InputCredentials {
        session: RemoteSessionId::from_raw(1),
        lease: InputLeaseId::from_raw(2),
        ticket: InputTicketId::from_raw(3),
        view: InputView {
            geometry: DisplayGeometryGeneration::INITIAL,
            viewport: ViewportMappingGeneration::INITIAL,
            configuration: CodecConfigurationGeneration::INITIAL,
            recovery: RecoveryGeneration::INITIAL,
        },
    };
    let mut a = SessionAuthority::new(c.session, AuthorityPolicy::plan_defaults());
    a.mark_capabilities_checked().unwrap();
    a.authorize_observation(HostInstant::ORIGIN).unwrap();
    a.mark_view_ready(HostInstant::ORIGIN).unwrap();
    a.grant_lease(c.lease, HostInstant::ORIGIN).unwrap();
    a.issue_input_ticket(c.lease, c.ticket, HostInstant::ORIGIN)
        .unwrap();
    let owner = InputSession::new(
        a,
        c,
        InputBounds::new(DesktopPoint { x: 0, y: 0 }, 320, 240).unwrap(),
        Capabilities::default().with(Capability::Keys),
        HostInstant::ORIGIN,
    )
    .unwrap();
    (owner, c)
}
fn at(us: u64) -> HostInstant {
    HostInstant::from_micros(us)
}
#[test]
fn monitor_tracks_both_renewals_but_never_renews_on_traffic_or_ticket_expiry() {
    let (mut s, _) = setup();
    let m = s.monitor();
    assert_eq!(m.deadline(at(1_500_000)), Ok(at(3_000_000)));
    s.issue_observation_challenge(17, at(1_000_000)).unwrap();
    s.renew_observation(17, at(1_500_000)).unwrap();
    assert_eq!(m.deadline(at(2_000_000)), Ok(at(3_000_000)));
    s.issue_control_challenge(18, at(2_000_000)).unwrap();
    s.renew_control(18, at(2_500_000)).unwrap();
    assert_eq!(m.deadline(at(3_000_000)), Ok(at(4_000_000)));
    assert!(m.deadline(at(4_000_000)).is_err());
    assert!(m.is_revoked());
    assert!(
        s.issue_ticket(InputTicketId::from_raw(4), at(3_000_000))
            .is_err()
    );
}
#[test]
fn concurrent_observer_sample_cannot_regress_the_authority_writer_clock() {
    let (mut s, _) = setup();
    let m = s.monitor();
    s.issue_control_challenge(4, at(100)).unwrap();
    assert_eq!(m.deadline(at(50)), Ok(at(3_000_000)));
    assert!(s.renew_control(4, at(150)).is_ok());
    assert!(!m.is_revoked());
}
#[test]
fn monitor_is_terminal_after_owner_drop_and_cannot_rebind_a_new_owner() {
    let (s, _) = setup();
    let m = s.monitor();
    drop(s);
    let (other, _) = setup();
    assert!(m.is_revoked());
    assert!(!other.monitor().is_revoked());
    m.revoke();
    assert!(!other.monitor().is_revoked());
}
struct BlockingSink {
    entered: mpsc::SyncSender<()>,
    resume: mpsc::Receiver<()>,
    in_submit: bool,
    effects: usize,
}
impl InputSink for BlockingSink {
    fn prepare(&mut self, _: Operation) -> Result<(), PlatformError> {
        if !self.in_submit {
            self.entered.send(()).unwrap();
            self.resume.recv().unwrap();
        }
        Ok(())
    }
    fn submit(&mut self, _: Operation) -> Submission {
        if self.in_submit {
            self.entered.send(()).unwrap();
            self.resume.recv().unwrap();
        }
        self.effects += 1;
        Submission::Submitted
    }
}
fn blocked(in_submit: bool) {
    let (mut owner, c) = setup();
    let m = owner.monitor();
    let (entered_tx, entered_rx) = mpsc::sync_channel(1);
    let (resume_tx, resume_rx) = mpsc::sync_channel(1);
    let native = thread::spawn(move || {
        let mut sink = BlockingSink {
            entered: entered_tx,
            resume: resume_rx,
            in_submit,
            effects: 0,
        };
        let r = owner
            .dispatch(
                InputRequest {
                    credentials: c,
                    sequence: 0,
                    event: InputEvent::Key {
                        key: PhysicalKey::new(4).unwrap(),
                        transition: KeyTransition::Press,
                    },
                },
                &mut sink,
                || at(1),
            )
            .unwrap();
        (r, owner.held_count(), sink.effects)
    });
    entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    // A timeout on this independent check catches accidentally retaining the
    // policy lock across native preflight or submission without hanging a test.
    let (done_tx, done_rx) = mpsc::sync_channel(1);
    let watchdog = thread::spawn(move || {
        done_tx.send(m.deadline(at(3_000_000))).unwrap();
    });
    let result = done_rx.recv_timeout(Duration::from_secs(2));
    resume_tx.send(()).unwrap();
    let (r, held, effects) = native.join().unwrap();
    watchdog.join().unwrap();
    assert!(result.unwrap().is_err());
    let Dispatch::Completed(r) = r else {
        panic!("receipt required")
    };
    if in_submit {
        assert_eq!(r.outcome, InputOutcome::SubmittedToOs);
        assert_eq!(effects, 1);
        assert_eq!(held, 1);
    } else {
        assert_eq!(r.outcome, InputOutcome::CancelledBeforeSubmission);
        assert_eq!(effects, 0);
        assert_eq!(held, 0);
    }
}
#[test]
fn expiry_progresses_while_native_preparation_is_blocked() {
    blocked(false);
}
#[test]
fn expiry_progresses_during_irreversible_native_submission_without_fabricating_rollback() {
    blocked(true);
}
