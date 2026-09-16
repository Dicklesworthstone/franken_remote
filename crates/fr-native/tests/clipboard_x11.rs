//! Real X11 selection transfers, not a recording clipboard implementation.
//! Run under Xvfb with `-noreset` to avoid last-client-reset connection races.
//! Set `FR_NATIVE_CLIPBOARD_REQUIRED=1`; missing display is then a hard failure.
#![cfg(all(target_os = "linux", feature = "linux-clipboard"))]
#![forbid(unsafe_code)]
use fr_core::{
    clipboard::{ClipboardSink, Endpoint, Publication, Stamp},
    limits::ProtocolLimits,
};
use fr_native::clipboard::{ReadError, ReadText, X11Clipboard};
use std::{
    sync::Mutex,
    time::{Duration, Instant},
};

static SERIAL: Mutex<()> = Mutex::new(());
fn display() -> Option<String> {
    if let Ok(display) = std::env::var("DISPLAY") {
        Some(display)
    } else {
        assert!(
            std::env::var_os("FR_NATIVE_CLIPBOARD_REQUIRED").is_none(),
            "required native clipboard tests need an X11 display"
        );
        eprintln!("BLOCKED: no X11 display; not native clipboard qualification");
        None
    }
}
fn open(display: &str) -> X11Clipboard {
    X11Clipboard::open(display, &ProtocolLimits::ABSOLUTE, true).unwrap()
}
fn stamp(sequence: u64) -> Stamp {
    Stamp {
        id: u128::from(sequence),
        source: Endpoint::Host,
        sequence,
    }
}
fn publish(clipboard: &mut X11Clipboard, text: &str, sequence: u64) {
    clipboard.prepare(text, stamp(sequence)).unwrap();
    assert_eq!(
        clipboard.publish(text, stamp(sequence)),
        Publication::SubmittedToOs
    );
}
fn finish(source: &mut X11Clipboard, reader: &mut X11Clipboard) -> Result<ReadText, ReadError> {
    let until = Instant::now() + Duration::from_secs(4);
    while Instant::now() < until {
        source.pump().unwrap();
        if let Some(text) = reader.poll_read()? {
            return Ok(text);
        }
        std::thread::sleep(Duration::from_micros(100));
    }
    panic!("native read exceeded its own bounded lifetime");
}

#[test]
fn empty_unicode_and_full_one_mib_cross_real_x11() {
    let _guard = SERIAL.lock().unwrap();
    let Some(display) = display() else { return };
    for text in [
        String::new(),
        "private 🦀 café\n\0tail".to_owned(),
        "🦀".repeat(262_144),
    ] {
        let mut source = open(&display);
        let mut reader = open(&display);
        publish(&mut source, &text, 1);
        reader.begin_read().unwrap();
        let observed = finish(&mut source, &mut reader).unwrap();
        assert_eq!(observed.as_str(), text);
        assert_eq!(observed.origin(), None);
        assert_eq!(format!("{observed:?}"), "ClipboardReadText([redacted])");
        source.pump().unwrap();
        assert_eq!(source.active_readers(), 0);
        assert!(matches!(reader.poll_read(), Err(ReadError::NotReading)));
    }
}
#[test]
fn own_publication_preserves_provenance_instead_of_echoing_equal_bytes() {
    let _guard = SERIAL.lock().unwrap();
    let Some(display) = display() else { return };
    let mut clipboard = open(&display);
    publish(&mut clipboard, "remote selection", 7);
    clipboard.begin_read().unwrap();
    let until = Instant::now() + Duration::from_secs(4);
    loop {
        if let Some(text) = clipboard.poll_read().unwrap() {
            assert_eq!(text.as_str(), "remote selection");
            assert_eq!(text.origin(), Some(stamp(7)));
            break;
        }
        assert!(Instant::now() < until);
        std::thread::sleep(Duration::from_micros(100));
    }
}
#[test]
fn cancellation_fences_late_replies_and_next_read_is_fresh() {
    let _guard = SERIAL.lock().unwrap();
    let Some(display) = display() else { return };
    let mut source = open(&display);
    let mut reader = open(&display);
    publish(&mut source, &"x".repeat(131_072), 1);
    reader.begin_read().unwrap();
    assert_eq!(reader.begin_read(), Err(ReadError::Busy));
    for _ in 0..20 {
        source.pump().unwrap();
        assert!(reader.poll_read().unwrap().is_none());
        if source.active_readers() != 0 {
            break;
        }
    }
    assert_eq!(source.active_readers(), 1);
    reader.cancel_read();
    assert!(matches!(reader.poll_read(), Err(ReadError::NotReading)));
    source.pump().unwrap();
    assert_eq!(source.active_readers(), 0);
    publish(&mut source, "new local copy", 2);
    reader.begin_read().unwrap();
    assert_eq!(
        finish(&mut source, &mut reader).unwrap().as_str(),
        "new local copy"
    );
}
#[test]
fn ownership_replacement_discards_the_old_read_without_overwriting_local_copy() {
    let _guard = SERIAL.lock().unwrap();
    let Some(display) = display() else { return };
    let mut source = open(&display);
    let mut reader = open(&display);
    let mut newer = open(&display);
    publish(&mut source, "old", 1);
    reader.begin_read().unwrap();
    publish(&mut newer, "new", 2);
    assert!(matches!(reader.poll_read(), Err(ReadError::LocalChanged)));
    reader.close();
    assert_eq!(newer.current_origin().unwrap(), Some(stamp(2)));
}
#[test]
fn same_owner_new_timestamp_during_incr_refuses_old_text() {
    let _guard = SERIAL.lock().unwrap();
    let Some(display) = display() else { return };
    let mut source = open(&display);
    let mut reader = open(&display);
    publish(&mut source, &"x".repeat(131_072), 1);
    reader.begin_read().unwrap();
    for _ in 0..30 {
        source.pump().unwrap();
        assert!(reader.poll_read().unwrap().is_none());
        if source.active_readers() != 0 {
            break;
        }
    }
    assert_eq!(source.active_readers(), 1);
    std::thread::sleep(Duration::from_millis(2));
    publish(&mut source, "new selection, same owner", 2);
    assert!(matches!(
        finish(&mut source, &mut reader),
        Err(ReadError::LocalChanged)
    ));
    reader.begin_read().unwrap();
    assert_eq!(
        finish(&mut source, &mut reader).unwrap().as_str(),
        "new selection, same owner"
    );
}
#[test]
fn silent_owner_expires_and_can_never_resume_old_read() {
    let _guard = SERIAL.lock().unwrap();
    let Some(display) = display() else { return };
    let mut source = open(&display);
    let mut reader = open(&display);
    publish(&mut source, "not delivered", 1);
    reader.begin_read().unwrap();
    // Do not pump the owner. Its lack of response cannot extend our lifetime.
    std::thread::sleep(Duration::from_millis(3_020));
    assert!(matches!(reader.poll_read(), Err(ReadError::Expired)));
    source.pump().unwrap();
    assert!(matches!(reader.poll_read(), Err(ReadError::NotReading)));
}
#[test]
fn permission_is_required_before_open_and_nonlocal_displays_refuse() {
    use fr_core::clipboard::PlatformError;
    assert!(matches!(
        X11Clipboard::open(":0", &ProtocolLimits::ABSOLUTE, false),
        Err(PlatformError::Permission)
    ));
    assert!(matches!(
        X11Clipboard::open("tcp/host:0", &ProtocolLimits::ABSOLUTE, true),
        Err(PlatformError::Unsupported)
    ));
}

#[path = "clipboard_x11/authorized.rs"]
mod authorized;
