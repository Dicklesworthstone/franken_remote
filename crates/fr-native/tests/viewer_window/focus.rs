//! Real focus changes and independent Xlib events against the original XCB
//! window owner. No input grant, physical visibility or host effect is inferred.
use super::*;

#[test]
fn automation_requires_an_exact_focus_reply_and_synthetic_focus_cannot_grant_it() {
    let desktop = Desktop::start();
    let runtime = support::runtime();
    let session = viewer(&runtime);
    let original = session.control();
    let mut window = ViewerWindow::start(&desktop.display, 320, 240, original.clone()).unwrap();
    let (control, id) = mapped(&window);
    assert!(!control.automation_focus_ready());
    control.enable_automation_focus().unwrap();
    assert_eq!(control.enable_automation_focus(), Err(Error::NotReady));
    desktop.peer(id, "focus_in_synthetic");
    thread::sleep(Duration::from_millis(125));
    assert!(!control.automation_focus_ready());
    assert!(!original.is_stopped());
    desktop.peer(id, "focus");
    wait(|| control.automation_focus_ready());
    assert!(!original.is_stopped());
    control.stop();
    assert!(!control.automation_focus_ready());
    finish(&mut window, StopReason::User);
}

#[test]
fn established_focus_loss_fences_only_its_original_automation_owner() {
    let desktop = Desktop::start();
    let runtime = support::runtime();
    let session = viewer(&runtime);
    let original = session.control();
    let foreign = viewer(&runtime);
    let mut window = ViewerWindow::start(&desktop.display, 320, 240, original.clone()).unwrap();
    let mut other = ViewerWindow::start(&desktop.display, 320, 240, foreign.control()).unwrap();
    let (control, id) = mapped(&window);
    let (other_control, other_id) = mapped(&other);
    control.enable_automation_focus().unwrap();
    desktop.peer(id, "focus");
    wait(|| control.automation_focus_ready());
    desktop.peer(other_id, "focus");
    wait(|| original.is_stopped());
    finish(&mut window, StopReason::FocusLost);
    assert!(!control.automation_focus_ready());
    assert_eq!(control.enable_automation_focus(), Err(Error::NotReady));
    assert!(!foreign.control().is_stopped());
    assert_eq!(other_control.status(), Status::Mapped);
    other_control.stop();
    finish(&mut other, StopReason::User);
}

#[test]
fn losing_then_regaining_focus_in_one_batch_cannot_resume_automation() {
    let desktop = Desktop::start();
    let runtime = support::runtime();
    let session = viewer(&runtime);
    let original = session.control();
    let mut window = ViewerWindow::start(&desktop.display, 320, 240, original.clone()).unwrap();
    let (control, id) = mapped(&window);
    control.enable_automation_focus().unwrap();
    desktop.peer(id, "focus");
    wait(|| control.automation_focus_ready());
    desktop.peer(id, "focus_bounce");
    wait(|| original.is_stopped());
    finish(&mut window, StopReason::FocusLost);
    assert!(!control.automation_focus_ready());
}

#[test]
fn native_focus_reply_stall_expires_instead_of_renewing_cached_focus() {
    let desktop = Desktop::start();
    let runtime = support::runtime();
    let session = viewer(&runtime);
    let original = session.control();
    let mut window = ViewerWindow::start(&desktop.display, 320, 240, original.clone()).unwrap();
    let (control, id) = mapped(&window);
    control.enable_automation_focus().unwrap();
    desktop.peer(id, "focus");
    wait(|| control.automation_focus_ready());
    // This is the private Xvfb child from THIS test. No shared desktop or
    // protocol response is replaced. SIGKILL in Desktop::drop still works if
    // an assertion unwinds while that child is stopped.
    let signal = |name: &str| {
        assert!(
            Command::new("kill")
                .args([name, &desktop.process.id().to_string()])
                .status()
                .unwrap()
                .success()
        );
    };
    signal("-STOP");
    wait(|| original.is_stopped());
    signal("-CONT");
    finish(&mut window, StopReason::FocusUnavailable);
    assert!(!control.automation_focus_ready());
}

#[test]
fn ordinary_viewing_does_not_acquire_an_automation_focus_requirement() {
    let desktop = Desktop::start();
    let runtime = support::runtime();
    let session = viewer(&runtime);
    let original = session.control();
    let mut window = ViewerWindow::start(&desktop.display, 320, 240, original.clone()).unwrap();
    let (control, id) = mapped(&window);
    desktop.peer(id, "focus");
    desktop.peer(id, "blur");
    thread::sleep(Duration::from_millis(125));
    assert!(!control.automation_focus_ready());
    assert!(!original.is_stopped());
    assert_eq!(control.status(), Status::Mapped);
    control.stop();
    finish(&mut window, StopReason::User);
}
