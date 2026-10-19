//! Application ordering only; real sender transport tests are separate qualification.
use super::Sending as Lane;
use super::*;
use fr_files::sender::batch::Selection;
use std::{fs::File, time::Duration};

#[derive(Default)]
struct Peer {
    negotiating: bool,
    stage: Option<Stage>,
    observed: Option<bool>,
    intent: bool,
    pacing_error: Option<Error>,
    cleanup: Option<Result<(), Error>>,
    calls: Vec<&'static str>,
    starts: usize,
    lifetime: Option<Duration>,
}
impl Endpoint for Peer {
    fn expect(&mut self, _: Policy) -> Result<(), Error> {
        self.calls.push("expect");
        self.negotiating = true;
        Ok(())
    }
    fn closed(&self) -> bool {
        false
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
        self.calls.push("start");
        self.starts += 1;
        self.lifetime = Some(request.lifetime);
        assert_eq!(request.selection.len(), 2);
        self.stage = Some(Stage::Queued);
        self.observed = Some(false);
        Ok(())
    }
    fn cancel(&mut self) -> Result<(), Error> {
        self.calls.push("cancel");
        self.stage = Some(Stage::Closed);
        self.observed = Some(false);
        Ok(())
    }
    fn cleanup(&mut self) -> Option<Result<(), Error>> {
        self.calls.push("cleanup");
        self.cleanup
    }
    fn report(&self) -> Option<Report> {
        None
    }
    fn progress(&self) -> Option<Progress> {
        None
    }
    fn set_paused(&mut self, paused: bool) -> Result<(), Error> {
        self.calls.push(if paused { "pause" } else { "resume" });
        if let Some(error) = self.pacing_error {
            return Err(error);
        }
        self.intent = paused;
        // Deliberately do not invent an observation until an owner turn supplies it.
        Ok(())
    }
    fn paused(&self) -> Option<bool> {
        self.observed
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
    (
        Lane::new(
            SendRequest {
                selection,
                policy: Policy::default(),
                lifetime: Duration::from_secs(13),
            },
            control.clone(),
        ),
        control,
    )
}
fn established(lane: &mut Lane, peer: &mut Peer) {
    lane.viewer(peer);
    peer.negotiating = false;
    peer.stage = Some(Stage::Idle);
}

#[test]
fn early_pause_settles_original_handshake_and_reaches_queue_before_source_service() {
    let (mut lane, control) = lane();
    let mut peer = Peer::default();
    assert!(control.pause());
    lane.viewer(&mut peer);
    for _ in 0..3 {
        lane.viewer(&mut peer);
    }
    assert_eq!(peer.calls, ["expect"]);
    assert!(lane.request.is_some());
    assert_eq!(control.snapshot().paused, None);
    peer.negotiating = false;
    peer.stage = Some(Stage::Idle);
    lane.viewer(&mut peer);
    assert_eq!(peer.calls, ["expect", "start", "pause", "cleanup"]);
    assert!(peer.intent);
    assert_eq!(peer.starts, 1);
    assert_eq!(peer.lifetime, Some(Duration::from_secs(13)));
    for _ in 0..4 {
        lane.viewer(&mut peer);
    }
    assert_eq!(peer.starts, 1);
    assert_eq!(peer.calls.iter().filter(|&&c| c == "expect").count(), 1);
    assert_eq!(peer.lifetime, Some(Duration::from_secs(13)));
}

#[test]
fn resume_updates_same_owner_without_starting_again_or_fabricating_ack() {
    let (mut lane, control) = lane();
    let mut peer = Peer::default();
    established(&mut lane, &mut peer);
    control.pause();
    lane.viewer(&mut peer);
    assert_eq!(control.snapshot().paused, Some(false));
    peer.observed = Some(true);
    lane.viewer(&mut peer);
    assert_eq!(control.snapshot().paused, Some(true));
    assert!(control.resume());
    lane.viewer(&mut peer);
    assert!(!peer.intent);
    assert_eq!(peer.starts, 1);
    assert_eq!(control.snapshot().paused, Some(true));
    peer.observed = Some(false);
    lane.viewer(&mut peer);
    assert_eq!(control.snapshot().paused, Some(false));
    assert!(control.snapshot().status.report.is_none());
}

#[test]
fn cancel_while_paused_never_resumes_and_cleanup_keeps_running() {
    let (mut lane, control) = lane();
    let mut peer = Peer::default();
    established(&mut lane, &mut peer);
    control.pause();
    lane.viewer(&mut peer);
    peer.calls.clear();
    control.cancel();
    assert!(!control.resume());
    for _ in 0..3 {
        lane.viewer(&mut peer);
    }
    assert_eq!(peer.calls, ["cancel", "cleanup", "cleanup", "cleanup"]);
    assert_eq!(peer.starts, 1);
    assert!(control.snapshot().cleanup.is_none());
    peer.cleanup = Some(Ok(()));
    lane.viewer(&mut peer);
    assert_eq!(control.snapshot().cleanup, Some(Ok(())));
    assert_eq!(control.status().phase, SendPhase::Ended);
    assert!(!control.resume());
}

#[test]
fn pause_then_resume_before_admission_uses_latest_intent_only() {
    let (mut lane, control) = lane();
    let mut peer = Peer::default();
    control.pause();
    established(&mut lane, &mut peer);
    control.resume();
    lane.viewer(&mut peer);
    assert_eq!(peer.calls, ["expect", "start", "resume", "cleanup"]);
    assert!(!peer.intent);
    assert_eq!(peer.starts, 1);
}

#[test]
fn pacing_refusal_is_terminal_once_and_does_not_restart_selection() {
    let (mut lane, control) = lane();
    let mut peer = Peer::default();
    established(&mut lane, &mut peer);
    peer.pacing_error = Some(Error::Expired);
    control.pause();
    lane.viewer(&mut peer);
    assert_eq!(
        peer.calls,
        ["expect", "start", "pause", "cancel", "cleanup"]
    );
    assert_eq!(control.status().phase, SendPhase::Ended);
    assert_eq!(
        control.status().absence,
        Some(Absence::Refused(Error::Expired))
    );
    assert!(!control.resume());
    peer.pacing_error = None;
    for _ in 0..3 {
        lane.viewer(&mut peer);
    }
    assert_eq!(peer.starts, 1);
    assert_eq!(peer.calls.iter().filter(|&&c| c == "cancel").count(), 1);
    assert_eq!(peer.calls.iter().filter(|&&c| c == "pause").count(), 1);
}
