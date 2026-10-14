//! Native INCR interoperability: missing size hints never disable real bounds.
use super::*;

#[test]
fn an_unknown_length_incr_reads_empty_split_unicode_and_full_one_mib() {
    let _guard = SERIAL.lock().unwrap();
    let Some(display) = display() else { return };
    let mut peer = Peer::new(&display, "unknown");
    let mut reader = open(&display);
    reader.start_watching().unwrap();
    settled(&mut reader);
    for text in [String::new(), "🦀".repeat(262_144)] {
        peer.copy(&text, false);
        settled(&mut reader);
        reader.begin_read().unwrap();
        assert_eq!(complete(&mut reader).unwrap().as_str(), text);
        assert!(matches!(reader.poll_read(), Err(ReadError::NotReading)));
    }
    // Odd 65,537-byte native properties split both UTF8 and 16-KiB read turns.
    assert_eq!(peer.requests(), 2);
}

#[test]
fn unknown_length_still_enforces_selected_bytes_and_native_chunk_count() {
    let _guard = SERIAL.lock().unwrap();
    let Some(display) = display() else { return };
    for (mode, length, bound) in [
        ("unknown", 32_768, 20_000),
        ("unknown", 131_072, 100_000),
        ("unknown-tiny", 1_025, 2_048),
    ] {
        let limits = ProtocolLimits::with_overrides(fr_core::limits::LimitOverrides {
            max_clipboard_item_bytes: Some(bound),
            ..Default::default()
        })
        .unwrap();
        let mut peer = Peer::new(&display, mode);
        peer.copy(&"x".repeat(length), false);
        let mut reader = X11Clipboard::open(&display, &limits, true).unwrap();
        reader.start_watching().unwrap();
        settled(&mut reader);
        reader.begin_read().unwrap();
        assert_eq!(complete(&mut reader).unwrap_err(), ReadError::Limit);
        assert!(matches!(reader.poll_read(), Err(ReadError::NotReading)));
        // A bounded fresh copy remains usable on this same native owner.
        peer.copy("valid after refusal", false);
        settled(&mut reader);
        reader.begin_read().unwrap();
        assert_eq!(
            complete(&mut reader).unwrap().as_str(),
            "valid after refusal"
        );
    }
}

#[test]
fn accepting_an_absent_hint_does_not_accept_malformed_incr_headers() {
    let _guard = SERIAL.lock().unwrap();
    let Some(display) = display() else { return };
    for mode in ["unknown-bad-format", "unknown-extra"] {
        let mut peer = Peer::new(&display, mode);
        peer.copy("header must still be bounded and typed", false);
        let mut reader = open(&display);
        reader.start_watching().unwrap();
        settled(&mut reader);
        reader.begin_read().unwrap();
        assert_eq!(complete(&mut reader).unwrap_err(), ReadError::Malformed);
    }
}

#[test]
fn unknown_length_still_requires_a_terminator_before_the_original_deadline() {
    let _guard = SERIAL.lock().unwrap();
    let Some(display) = display() else { return };
    let mut peer = Peer::new(&display, "unknown-unterminated");
    peer.copy("complete bytes without the mandatory terminator", false);
    let mut reader = open(&display);
    reader.start_watching().unwrap();
    settled(&mut reader);
    reader.begin_read().unwrap();
    assert_eq!(complete(&mut reader).unwrap_err(), ReadError::Expired);
    assert!(matches!(reader.poll_read(), Err(ReadError::NotReading)));
}
