//! Deterministic races use real X11 copies from the clock callback, not a fake
//! native publisher. The callback points straddle actual preparation/submission.
use super::*;
use fr_core::clipboard::{Error, PlatformError};
use fr_wire::clipboard::session::SessionError;

fn remote_commit(peer: &mut ChannelSession, target: &mut ClipboardSynchronizer) -> Vec<u8> {
    peer.offer(400, "older remote transfer", None, at(0))
        .unwrap();
    let mut record = Record::default();
    let mut scratch = vec![0; 65_536];
    loop {
        let step = peer.pump(&mut scratch, &mut record, || at(0)).unwrap();
        if matches!(step, Pump::ItemAccepted(_)) {
            return record.bytes.take().unwrap();
        }
        assert_eq!(
            target
                .receive(record.bytes.as_deref().unwrap(), || at(0))
                .unwrap(),
            Received::Consumed(None)
        );
        record.clear();
    }
}

#[test]
fn a_local_copy_during_remote_preparation_is_preserved_and_propagated() {
    let _guard = SERIAL.lock().unwrap();
    let Some(display) = display() else { return };
    let input = owner();
    let peer_input = owner();
    let mut peer = channel(&peer_input, Role::Controller);
    let mut target = ClipboardSynchronizer::new(channel(&input, Role::Host), open(&display));
    let mut app = open(&display);
    let commit = remote_commit(&mut peer, &mut target);
    let mut calls = 0;
    let result = target
        .receive(&commit, || {
            calls += 1;
            if calls == 3 {
                publish(&mut app, "local copy during preparation", 7);
            }
            at(0)
        })
        .unwrap();
    assert_eq!(
        result,
        Received::Refused(SessionError::Clipboard(Error::Platform(
            PlatformError::LocalChanged
        )))
    );
    assert_eq!(calls, 3, "the rejected preparation must not publish");
    assert_eq!(app.current_origin().unwrap(), Some(stamp(7)));
    assert!(!target.is_closed());
    assert!(input.monitor().deadline(at(0)).is_ok());

    let mut record = Record::default();
    let mut scratch = vec![0; 65_536];
    let mut queued = false;
    for _ in 0..1000 {
        app.pump().unwrap();
        let progress = target
            .poll(&mut scratch, &mut record, || at(0), || Ok(700))
            .unwrap();
        if matches!(progress.read, ReadProgress::Queued(_)) {
            queued = true;
            break;
        }
    }
    assert!(
        queued,
        "preparation must not swallow the local change notification"
    );
}

#[test]
fn a_local_copy_after_preparation_is_fenced_by_the_os_timestamp() {
    let _guard = SERIAL.lock().unwrap();
    let Some(display) = display() else { return };
    let input = owner();
    let peer_input = owner();
    let mut peer = channel(&peer_input, Role::Controller);
    let mut target = ClipboardSynchronizer::new(channel(&input, Role::Host), open(&display));
    let mut app = open(&display);
    let commit = remote_commit(&mut peer, &mut target);
    let mut calls = 0;
    let Received::Consumed(Some(receipt)) = target
        .receive(&commit, || {
            calls += 1;
            if calls == 4 {
                // No artificial delay: the guarded preparation must establish
                // a strictly later server-clock barrier itself.
                publish(&mut app, "new copy after remote preparation", 8);
            }
            at(0)
        })
        .unwrap()
    else {
        panic!("the attempted OS effect must retain its receipt")
    };
    assert_eq!(calls, 4);
    assert_eq!(receipt.publication, Publication::UnknownEffect);
    assert_eq!(app.current_origin().unwrap(), Some(stamp(8)));
    assert!(input.monitor().deadline(at(0)).is_ok());
    // Duplicate receipt lookup must not publish a second time, even though a
    // genuine local native revision has overtaken the first attempt.
    assert_eq!(
        target.receive(&commit, || at(0)).unwrap(),
        Received::Consumed(Some(receipt))
    );
    assert_eq!(app.current_origin().unwrap(), Some(stamp(8)));
}

#[test]
fn native_preparation_does_not_consume_new_copy_metadata_on_refusal() {
    use fr_wire::clipboard::session::synchronize::NativeClipboard;
    let _guard = SERIAL.lock().unwrap();
    let Some(display) = display() else { return };
    let mut native = open(&display);
    let mut app = open(&display);
    native.start_watching().unwrap();
    let baseline = native.poll_change().unwrap().unwrap().revision();
    publish(&mut app, "genuine new local copy", 9);
    assert_eq!(
        native.prepare_for_revision("stale remote clipboard", stamp(10), baseline),
        Err(PlatformError::LocalChanged)
    );
    native.cancel_prepared();
    let current = native
        .poll_change()
        .unwrap()
        .expect("retain the new revision");
    assert!(current.revision() > baseline);
    assert!(current.origin().is_none());
    assert_eq!(app.current_origin().unwrap(), Some(stamp(9)));
}
