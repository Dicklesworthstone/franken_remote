use super::*;
use fr_native::clipboard::{ClipboardChange, WatchError};

fn change(watcher: &mut X11Clipboard) -> ClipboardChange {
    let until = Instant::now() + Duration::from_secs(1);
    while Instant::now() < until {
        if let Some(change) = watcher.poll_change().unwrap() {
            return change;
        }
        std::thread::sleep(Duration::from_micros(100));
    }
    panic!("expected a bounded server-authored clipboard notification");
}

#[test]
fn changes_bootstrap_once_then_stay_idle_without_reading_payloads() {
    let _guard = SERIAL.lock().unwrap();
    let Some(display) = display() else { return };
    let mut source = open(&display);
    let mut watcher = open(&display);
    publish(&mut source, "private text that is not requested", 1);
    watcher.start_watching().unwrap();
    let initial = change(&mut watcher);
    assert!(initial.has_selection());
    assert!(initial.origin().is_none());
    for _ in 0..100 {
        assert!(watcher.poll_change().unwrap().is_none());
    }
    assert_eq!(watcher.change_revision(), initial.revision());
    assert_eq!(watcher.poll_read().unwrap_err(), ReadError::NotReading);
    assert_eq!(source.pump().unwrap(), 0, "watching must not request text");
    assert_eq!(watcher.start_watching(), Err(WatchError::AlreadyWatching));
}

#[test]
fn same_owner_equal_text_reselection_is_a_new_revision_not_an_echo() {
    let _guard = SERIAL.lock().unwrap();
    let Some(display) = display() else { return };
    let mut source = open(&display);
    let mut watcher = open(&display);
    publish(&mut source, "same bytes", 1);
    watcher.start_watching().unwrap();
    let before = change(&mut watcher);
    publish(&mut source, "same bytes", 2);
    let after = change(&mut watcher);
    assert!(after.revision() > before.revision());
    assert!(after.has_selection());
    assert!(after.origin().is_none());
    assert!(watcher.poll_change().unwrap().is_none());
}

#[test]
fn own_publications_keep_exact_origin_and_external_replacements_do_not() {
    let _guard = SERIAL.lock().unwrap();
    let Some(display) = display() else { return };
    let mut source = open(&display);
    let mut watcher = open(&display);
    watcher.start_watching().unwrap();
    change(&mut watcher);
    publish(&mut watcher, "our publication", 10);
    assert_eq!(change(&mut watcher).origin(), Some(stamp(10)));
    publish(&mut watcher, "our newer publication", 11);
    assert_eq!(change(&mut watcher).origin(), Some(stamp(11)));
    publish(&mut source, "our newer publication", 12);
    assert!(change(&mut watcher).origin().is_none());
}

#[test]
fn notification_bursts_are_bounded_and_coalesce_to_the_last_copy() {
    let _guard = SERIAL.lock().unwrap();
    let Some(display) = display() else { return };
    let mut source = open(&display);
    let mut watcher = open(&display);
    watcher.start_watching().unwrap();
    let before = change(&mut watcher).revision();
    for id in 1..=80 {
        publish(&mut source, "burst", id);
    }
    // A bounded turn never returns an intermediate clipboard from a backlog.
    assert!(watcher.poll_change().unwrap().is_none());
    assert_eq!(watcher.change_revision(), before + 32);
    assert!(watcher.poll_change().unwrap().is_none());
    assert_eq!(watcher.change_revision(), before + 64);
    assert_eq!(change(&mut watcher).revision(), before + 80);
    assert!(watcher.poll_change().unwrap().is_none());
    assert_eq!(watcher.poll_read().unwrap_err(), ReadError::NotReading);
}

#[test]
fn stopping_and_restarting_fences_old_notifications_without_resetting_selection() {
    let _guard = SERIAL.lock().unwrap();
    let Some(display) = display() else { return };
    let mut source = open(&display);
    let mut watcher = open(&display);
    watcher.start_watching().unwrap();
    let before = change(&mut watcher).revision();
    for id in 1..=5 {
        publish(&mut source, "old queued event", id);
    }
    watcher.stop_watching().unwrap();
    assert!(matches!(
        watcher.poll_change(),
        Err(WatchError::NotWatching)
    ));
    publish(&mut source, "new current copy", 6);
    watcher.start_watching().unwrap();
    let current = change(&mut watcher);
    assert_eq!(
        current.revision(),
        before + 1,
        "only fresh bootstrap survives"
    );
    assert!(current.has_selection());
    watcher.begin_read().unwrap();
    assert_eq!(
        finish(&mut source, &mut watcher).unwrap().as_str(),
        "new current copy"
    );
}

#[test]
fn owner_disconnection_reports_selection_loss_without_creating_empty_text() {
    let _guard = SERIAL.lock().unwrap();
    let Some(display) = display() else { return };
    let mut source = open(&display);
    let mut watcher = open(&display);
    publish(&mut source, "was owned", 1);
    watcher.start_watching().unwrap();
    let before = change(&mut watcher);
    source.close();
    let lost = change(&mut watcher);
    assert!(lost.revision() > before.revision());
    assert!(!lost.has_selection());
    assert!(lost.origin().is_none());
    assert_eq!(watcher.begin_read(), Err(ReadError::NoSelection));
}
