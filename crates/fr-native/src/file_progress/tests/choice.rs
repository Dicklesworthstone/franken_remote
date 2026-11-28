//! Real C hit testing and the production Rust event loop reserve the same
//! original intake slot as Xdnd. The transfer snapshot remains a fixture.
use super::*;
use crate::file_drop::{ChoiceSnapshot, ChoiceStatus, Inbox};

struct ChoiceTarget {
    probe: Probe,
    inbox: Inbox,
}
impl Target for ChoiceTarget {
    fn stopped(&self) -> bool {
        self.probe.stopped()
    }
    fn direction(&self) -> Direction {
        Direction::Upload
    }
    fn snapshot(&self) -> Snapshot {
        self.probe.snapshot()
    }
    fn choice(&self) -> Option<ChoiceSnapshot> {
        self.inbox.choice()
    }
    fn action(&self, action: Action) {
        assert_eq!(action, Action::ChooseFiles);
        assert!(self.inbox.request_choice());
    }
}

#[test]
fn actual_chooser_button_reserves_one_selection_and_survives_local_decline() {
    let _serial = SERIAL.lock().unwrap();
    let Some((mut peer, display)) = Peer::new() else {
        return;
    };
    let inbox = Inbox::default();
    assert!(inbox.enable_choice());
    let target = Arc::new(ChoiceTarget {
        probe: Probe::new(),
        inbox: inbox.clone(),
    });
    target.probe.sample.lock().unwrap().status.phase = SendPhase::Finished;
    let state = Arc::new(AtomicU8::new(0));
    let (copy, shared, window) = (target.clone(), state.clone(), peer.window);
    let task = thread::spawn(move || run(&display, window, &shared, copy.as_ref()));
    wait(|| state.load(Ordering::Acquire) == 1 || task.is_finished());
    assert!(!task.is_finished());
    wait(|| peer.command("mapped") == "yes");
    assert_eq!(peer.command("choose_box"), "168");
    peer.command("click choose");
    thread::sleep(REFRESH);
    assert_eq!(inbox.choice().unwrap().status, ChoiceStatus::Idle);
    inbox.set_ready(true);
    thread::sleep(REFRESH);
    for invalid in [
        "synthetic choose",
        "release choose",
        "press choose",
        "release outside",
    ] {
        peer.command(invalid);
    }
    thread::sleep(REFRESH);
    assert_eq!(inbox.choice().unwrap().status, ChoiceStatus::Idle);
    assert_eq!(peer.command("focus"), "parent");
    peer.command("click choose");
    wait(|| inbox.choice().unwrap().status == ChoiceStatus::Choosing);
    assert!(inbox.take_choice());
    peer.command("click choose");
    thread::sleep(REFRESH);
    assert!(!inbox.take_choice());
    assert!(inbox.take().is_none());
    assert!(!inbox.ready());
    inbox.finish_choice(ChoiceStatus::Cancelled);
    inbox.set_ready(true);
    thread::sleep(REFRESH);
    assert_eq!(state.load(Ordering::Acquire), 1);
    assert!(!target.stopped());
    assert_eq!(*target.probe.commands.lock().unwrap(), []);
    peer.command("click choose");
    wait(|| inbox.choice().unwrap().status == ChoiceStatus::Choosing);
    assert!(inbox.take_choice());
    assert_eq!(peer.command("focus"), "parent");
    finish(&target.probe, &state, task);
}
