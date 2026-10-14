//! Independent application selections through automatic sync and native publication.
//! Real X11 on both desktops; the bounded record handoff remains a test fixture.
use super::super::timestamp_optional::Peer;
use super::*;

#[test]
fn timestamp_less_apps_sync_both_ways_without_echo_and_keep_control_alive() {
    let _guard = SERIAL.lock().unwrap();
    if display().is_none() {
        return;
    }
    let mut pair = Pair::new();
    let mut left = Peer::new(&pair.desktops[0].display, "text");
    let mut right = Peer::new(&pair.desktops[1].display, "unknown");
    for (index, text) in [
        String::new(),
        "copied by a real application 🦀\n\0tail".to_owned(),
        "🦀".repeat(262_144),
    ]
    .iter()
    .enumerate()
    {
        left.copy(text, false);
        pair.until(|p| p.receipts[1].len() == index + 1);
        assert_eq!(pair.text(1), *text);
        // The other app independently copies equal text: it is not an echo.
        right.copy(text, false);
        pair.until(|p| p.receipts[0].len() == index + 1);
        assert_eq!(pair.text(0), *text);
    }
    for _ in 0..100 {
        pair.step();
    }
    assert_eq!(pair.ids, [3, 3]);
    assert_eq!(left.requests(), 3);
    assert_eq!(right.requests(), 3);
    assert_eq!(pair.left.retained_channel_bytes(), 0);
    assert_eq!(pair.right.retained_channel_bytes(), 0);
    assert!(pair.left_input.monitor().deadline(at(0)).is_ok());
    assert!(pair.right_input.monitor().deadline(at(0)).is_ok());
}
