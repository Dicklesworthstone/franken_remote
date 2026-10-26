use super::*;
use fr_core::clipboard::PlatformError;
use fr_native::clipboard::WatchError;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

fn guarded(display: &str) -> (X11Clipboard, Arc<AtomicBool>) {
    let enabled = Arc::new(AtomicBool::new(true));
    let original = enabled.clone();
    let native = open(display).with_privacy_gate(move || original.load(Ordering::Acquire));
    (native, enabled)
}

#[test]
fn privacy_off_after_preparation_fences_publication_and_cannot_rearm() {
    let _lock = SERIAL.lock().unwrap();
    let Some(display) = display() else { return };
    let mut local = open(&display);
    publish(&mut local, "newer local copy remains", 31);
    let (mut peer, enabled) = guarded(&display);
    peer.prepare("private pending transfer", stamp(32)).unwrap();
    enabled.store(false, Ordering::Release);
    assert_eq!(
        peer.publish("private pending transfer", stamp(32)),
        Publication::NotSubmitted(PlatformError::Unavailable)
    );
    assert_eq!(
        peer.start_watching(),
        Err(WatchError::Platform(PlatformError::Unavailable))
    );
    assert_eq!(
        peer.begin_read(),
        Err(ReadError::Platform(PlatformError::Unavailable))
    );
    enabled.store(true, Ordering::Release);
    assert_eq!(
        peer.begin_read(),
        Err(ReadError::Platform(PlatformError::Unavailable)),
        "a late callback cannot revive this native owner"
    );
    assert_eq!(
        peer.prepare("must stay private", stamp(33)),
        Err(PlatformError::Unavailable)
    );
    peer.close();
    assert_eq!(local.current_origin().unwrap(), Some(stamp(31)));
}

#[test]
fn privacy_off_during_an_incremental_native_read_erases_the_partial_item() {
    let _lock = SERIAL.lock().unwrap();
    let Some(display) = display() else { return };
    let mut source = open(&display);
    publish(&mut source, &"s".repeat(128 * 1024), 41);
    let (mut reader, enabled) = guarded(&display);
    reader.begin_read().unwrap();
    let until = Instant::now() + Duration::from_secs(2);
    while source.active_readers() == 0 {
        assert!(Instant::now() < until, "native INCR transfer did not begin");
        source.pump().unwrap();
        assert!(reader.poll_read().unwrap().is_none());
        std::thread::sleep(Duration::from_millis(1));
    }
    enabled.store(false, Ordering::Release);
    assert!(matches!(
        reader.poll_read(),
        Err(ReadError::Platform(PlatformError::Unavailable))
    ));
    enabled.store(true, Ordering::Release);
    assert!(matches!(
        reader.poll_read(),
        Err(ReadError::Platform(PlatformError::Unavailable))
    ));
    assert_eq!(
        reader.begin_read(),
        Err(ReadError::Platform(PlatformError::Unavailable))
    );
    reader.close();
    assert_eq!(source.current_origin().unwrap(), Some(stamp(41)));
}
