//! Deterministic application-order tests. Scripted endpoints are NOT network,
//! disk, authority or publication qualification; real sender fixtures cover those.
use super::Sending as Lane;
use super::*;
use fr_files::sender::batch::Selection;
use std::{fs::File, time::Duration};

struct Peer {
    expect_result: Result<(), Error>,
    closed: bool,
    negotiating: bool,
    stage: Option<Stage>,
    progress: Option<Progress>,
    cleanup: Option<Result<(), Error>>,
    starts: usize,
    expectations: usize,
    cancels: usize,
    polls: usize,
    cancel_on_ready: Option<SendControl>,
}
impl Default for Peer {
    fn default() -> Self {
        Self {
            expect_result: Ok(()),
            closed: false,
            negotiating: false,
            stage: None,
            progress: None,
            cleanup: None,
            starts: 0,
            expectations: 0,
            cancels: 0,
            polls: 0,
            cancel_on_ready: None,
        }
    }
}
impl Endpoint for Peer {
    fn expect(&mut self, _: Policy) -> Result<(), Error> {
        self.expectations += 1;
        if self.expect_result.is_ok() {
            self.negotiating = true;
        }
        self.expect_result
    }
    fn closed(&self) -> bool {
        self.closed
    }
    fn negotiating(&self) -> bool {
        self.negotiating
    }
    fn stage(&self) -> Option<Stage> {
        self.stage
    }
    fn failure(&self) -> Option<Error> {
        None
    }
    fn start(&mut self, request: SendRequest) -> Result<(), Error> {
        assert!(!self.negotiating);
        assert_eq!(request.selection.len(), 2);
        assert_eq!(request.lifetime, Duration::from_secs(13));
        self.starts += 1;
        self.stage = Some(Stage::Queued);
        Ok(())
    }
    fn can_start(&mut self) -> bool {
        if let Some(control) = self.cancel_on_ready.take() {
            assert!(control.cancel(), "the setup owner has not handed off yet");
        }
        !self.closed && self.stage == Some(Stage::Idle)
    }
    fn cancel(&mut self) -> Result<(), Error> {
        assert!(
            !self.negotiating,
            "must not fence a half-acknowledged parent"
        );
        self.cancels += 1;
        self.stage = Some(Stage::Closed);
        Ok(())
    }
    fn cleanup(&mut self) -> Option<Result<(), Error>> {
        self.polls += 1;
        self.cleanup
    }
    fn report(&self) -> Option<Report> {
        None
    }
    fn progress(&self) -> Option<Progress> {
        self.progress
    }
    fn set_paused(&mut self, _: bool) -> Result<(), Error> {
        Ok(())
    }
    fn paused(&self) -> Option<bool> {
        None
    }
}
fn lane() -> (Lane, SendControl) {
    let mut selection = Selection::new();
    for name in ["one", "two"] {
        selection
            .push(
                File::open(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml")).unwrap(),
                name,
            )
            .unwrap();
    }
    let control = SendControl::new();
    let lane = Lane::new(
        SendRequest {
            selection,
            policy: Policy::default(),
            lifetime: Duration::from_secs(13),
        },
        control.clone(),
    );
    (lane, control)
}
fn established(lane: &mut Lane, peer: &mut Peer) {
    lane.viewer(peer);
    assert!(peer.negotiating);
    peer.negotiating = false;
    peer.stage = Some(Stage::Idle);
}

#[test]
fn early_cancel_keeps_the_host_offer_expected_without_starting_sources() {
    let (mut lane, control) = lane();
    let mut peer = Peer::default();
    assert!(control.cancel());
    lane.viewer(&mut peer);
    assert!(lane.request.is_none());
    assert_eq!(peer.expectations, 1);
    assert_eq!(peer.cancels, 0);
    assert_eq!(control.status().phase, SendPhase::Negotiating);
    for _ in 0..3 {
        lane.viewer(&mut peer);
    }
    assert_eq!(
        peer.expectations, 1,
        "must not renew the original handshake"
    );
    assert_eq!(peer.starts, 0);
    assert_eq!(peer.cancels, 0);
    assert_eq!(peer.polls, 0);
    peer.negotiating = false;
    peer.stage = Some(Stage::Idle);
    peer.cleanup = Some(Ok(()));
    lane.viewer(&mut peer);
    assert_eq!(peer.cancels, 1);
    assert_eq!(peer.starts, 0);
    assert!(!peer.closed);
    assert_eq!(
        control.status().absence,
        Some(Absence::Refused(Error::Cancelled))
    );
    assert_eq!(control.status().phase, SendPhase::Ended);
    assert_eq!(control.snapshot().cleanup, Some(Ok(())));
    assert!(control.status().report.is_none());
}

#[test]
fn cancel_during_negotiation_defers_only_retirement_not_source_release() {
    let (mut lane, control) = lane();
    let mut peer = Peer::default();
    lane.viewer(&mut peer);
    control.cancel();
    lane.viewer(&mut peer);
    assert!(lane.request.is_none());
    assert_eq!(peer.starts, 0);
    assert_eq!(peer.cancels, 0);
    peer.negotiating = false;
    peer.stage = Some(Stage::Idle);
    lane.viewer(&mut peer);
    assert_eq!(peer.cancels, 1);
    assert_eq!(peer.starts, 0);
    assert!(!peer.closed);
}

#[test]
fn stale_view_registration_retry_does_not_revive_a_cancelled_selection() {
    let (mut lane, control) = lane();
    let mut peer = Peer {
        expect_result: Err(Error::Cancelled),
        ..Peer::default()
    };
    control.cancel();
    lane.viewer(&mut peer);
    assert!(!lane.expecting);
    assert!(lane.request.is_none());
    peer.expect_result = Ok(());
    lane.viewer(&mut peer);
    assert!(lane.expecting);
    assert_eq!(peer.expectations, 2);
    lane.viewer(&mut peer);
    assert_eq!(peer.expectations, 2);
    assert_eq!(peer.starts, 0);
}

#[test]
fn closed_registration_is_not_retried_as_a_visibility_gap() {
    let (mut lane, control) = lane();
    let mut peer = Peer {
        expect_result: Err(Error::Cancelled),
        closed: true,
        ..Peer::default()
    };
    lane.viewer(&mut peer);
    lane.viewer(&mut peer);
    assert_eq!(peer.expectations, 1);
    assert_eq!(peer.starts, 0);
    assert_eq!(control.status().phase, SendPhase::Ended);
}

#[test]
fn in_flight_cancel_retires_once_and_keeps_polling_without_closing_desktop() {
    let (mut lane, control) = lane();
    let mut peer = Peer::default();
    established(&mut lane, &mut peer);
    lane.viewer(&mut peer);
    assert_eq!(peer.starts, 1);
    peer.stage = Some(Stage::Streaming);
    peer.progress = Some(Progress {
        id: 1,
        queued_bytes: 3,
        total_bytes: Some(100),
    });
    lane.viewer(&mut peer);
    assert_eq!(control.snapshot().current, peer.progress);
    control.cancel();
    for _ in 0..3 {
        lane.viewer(&mut peer);
    }
    assert_eq!(peer.cancels, 1);
    assert_eq!(peer.starts, 1);
    assert!(control.snapshot().cleanup.is_none());
    assert_ne!(control.status().phase, SendPhase::Finished);
    peer.cleanup = Some(Ok(()));
    lane.viewer(&mut peer);
    assert_eq!(control.snapshot().cleanup, Some(Ok(())));
    assert!(!peer.closed);
}

#[test]
fn progress_is_observed_without_starting_again_or_claiming_a_receipt() {
    let (mut lane, control) = lane();
    let mut peer = Peer::default();
    established(&mut lane, &mut peer);
    lane.viewer(&mut peer);
    for (stage, total, queued) in [
        (Stage::Preparing, None, 0),
        (Stage::Streaming, Some(9), 4),
        (Stage::AwaitingProof, Some(9), 9),
    ] {
        peer.stage = Some(stage);
        peer.progress = Some(Progress {
            id: 1,
            queued_bytes: queued,
            total_bytes: total,
        });
        lane.viewer(&mut peer);
        let state = control.snapshot();
        assert_eq!(state.current, peer.progress);
        assert_eq!(state.stage, Some(stage));
        assert_eq!(state.status.phase, SendPhase::Sending);
        assert!(state.status.report.is_none());
    }
    assert_eq!(peer.starts, 1);
    assert_eq!(peer.cancels, 0);
}

#[test]
fn sender_failure_cleanup_is_polled_even_without_a_user_cancel() {
    let (mut lane, control) = lane();
    let mut peer = Peer::default();
    established(&mut lane, &mut peer);
    lane.viewer(&mut peer);
    peer.stage = Some(Stage::Closed);
    peer.cleanup = Some(Err(Error::Worker));
    let before = peer.polls;
    lane.viewer(&mut peer);
    assert!(peer.polls > before);
    assert_eq!(control.snapshot().cleanup, Some(Err(Error::Worker)));
    assert_ne!(control.status().phase, SendPhase::Finished);
    assert!(!peer.closed);
}

#[test]
fn deferred_selection_settles_one_attachment_without_starting_an_empty_batch() {
    let control = SendControl::new();
    let mut lane = Lane::deferred(Policy::default(), control.clone());
    let mut peer = Peer::default();
    assert!(lane.request.is_none());
    assert!(!control.pause() && !control.resume());
    lane.viewer(&mut peer);
    for _ in 0..4 {
        lane.viewer(&mut peer);
    }
    assert_eq!(peer.expectations, 1);
    assert_eq!(peer.starts, 0);
    assert_eq!(peer.polls, 0, "a pending attachment is not a disk owner");
    peer.negotiating = false;
    peer.stage = Some(Stage::Idle);
    for _ in 0..4 {
        lane.viewer(&mut peer);
    }
    assert_eq!(peer.expectations, 1);
    assert_eq!(peer.starts, 0);
    assert_eq!(peer.cancels, 0);
    assert_eq!(control.status().phase, SendPhase::WaitingForSelection);
    assert!(control.status().report.is_none());
    assert!(control.snapshot().current.is_none());
    assert!(!peer.closed);
}

#[test]
fn cancelling_an_empty_selection_settles_the_original_attachment_then_retires_it() {
    for cancel_before_offer in [true, false] {
        let control = SendControl::new();
        let mut lane = Lane::deferred(Policy::default(), control.clone());
        let mut peer = Peer::default();
        if cancel_before_offer {
            assert!(control.cancel());
        }
        lane.viewer(&mut peer);
        if !cancel_before_offer {
            assert!(control.cancel());
        }
        lane.viewer(&mut peer);
        assert_eq!(peer.expectations, 1);
        assert_eq!(peer.starts, 0);
        assert_eq!(peer.cancels, 0, "do not abandon a half-attached lane");
        peer.negotiating = false;
        peer.stage = Some(Stage::Idle);
        peer.cleanup = Some(Ok(()));
        for _ in 0..4 {
            lane.viewer(&mut peer);
        }
        assert_eq!(peer.expectations, 1);
        assert_eq!(peer.cancels, 1);
        assert_eq!(peer.starts, 0);
        assert_eq!(control.status().phase, SendPhase::Ended);
        assert_eq!(
            control.status().absence,
            Some(Absence::Refused(Error::Cancelled))
        );
        assert_eq!(control.snapshot().cleanup, Some(Ok(())));
        assert!(control.status().report.is_none());
        assert!(
            !peer.closed,
            "file cancellation leaves the desktop owner live"
        );
    }
}

#[test]
fn failed_deferred_attachment_does_not_wait_forever_for_a_selection() {
    for stage in [None, Some(Stage::Closed)] {
        let control = SendControl::new();
        let mut lane = Lane::deferred(Policy::default(), control.clone());
        let mut peer = Peer::default();
        lane.viewer(&mut peer);
        peer.negotiating = false;
        peer.stage = stage;
        peer.cleanup = Some(Ok(()));
        for _ in 0..4 {
            lane.viewer(&mut peer);
        }
        assert_eq!(peer.expectations, 1);
        assert_eq!(peer.starts, 0);
        assert_eq!(control.status().phase, SendPhase::Ended);
        assert_eq!(
            control.status().absence,
            Some(Absence::SetupFailed(Error::Closed))
        );
        assert!(control.status().report.is_none());
        assert!(!control.cancel() && !control.pause() && !control.resume());
        assert!(!peer.closed);
    }
}

#[test]
fn cancelling_between_readiness_check_and_selection_handoff_cannot_start_a_source() {
    use asupersync::{runtime::RuntimeBuilder, types::Budget};
    let runtime = RuntimeBuilder::current_thread().build().unwrap();
    let cx = runtime.request_cx_with_budget(Budget::INFINITE);
    let control = SendControl::new();
    let intake = SendIntake::new(cx, Policy::default());
    let mut deferred = Lane::deferred(Policy::default(), control.clone());
    deferred.intake = Some(intake.clone());
    let mut peer = Peer::default();
    established(&mut deferred, &mut peer);
    deferred.viewer(&mut peer);
    assert!(intake.ready());
    let (mut selection, _) = lane();
    let pending = intake.submit(selection.request.take().unwrap()).unwrap();
    // Interpose after the lane's first cancel check but before the actual
    // selection handoff, using the real setup and per-selection controls.
    peer.cancel_on_ready = Some(control.clone());
    peer.cleanup = Some(Ok(()));
    deferred.viewer(&mut peer);
    assert_eq!(peer.starts, 0);
    assert_eq!(peer.cancels, 1);
    assert!(intake.is_closed());
    assert!(!intake.ready());
    assert!(deferred.request.is_none());
    assert_eq!(pending.status().phase, SendPhase::Ended);
    assert_eq!(
        pending.status().absence,
        Some(Absence::Refused(Error::Cancelled))
    );
    assert_eq!(pending.snapshot().cleanup, Some(Ok(())));
    assert!(pending.status().report.is_none());
    assert_eq!(control.status().phase, SendPhase::Ended);
    assert!(
        !peer.closed,
        "the cancelled optional upload cannot stop the desktop"
    );
    deferred.viewer(&mut peer);
    assert_eq!(peer.starts, 0);
    assert_eq!(peer.cancels, 1);
}
