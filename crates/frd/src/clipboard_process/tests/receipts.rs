//! Deterministically interleave the real parent's validated IPC reply with the
//! independent privacy fence. The child is the existing explicit protocol
//! fixture; these tests make no claim about native clipboard publication.
use super::*;
use crate::native_clipboard::{Application, Configuration};

fn application() -> Application {
    Application::new(
        Configuration::new::<RemoteClipboard, _, _>(
            true,
            Duration::from_secs(1),
            || Err(PlatformError::Unavailable),
            || Ok(1),
        )
        .unwrap(),
    )
}

#[test]
fn publication_receipts_survive_a_post_reply_stop_without_reopening_the_owner() {
    for submitted in [false, true] {
        let runtime = runtime();
        let cx = runtime.request_cx_with_budget(Budget::INFINITE);
        let app = application();
        let original = app.control();
        let (image, log) = fixture("normal");
        let mut owner =
            RemoteClipboard::start_inner(&launch(&image), cx.clone(), 4096, Some(original.clone()))
                .unwrap();
        owner.watch().unwrap();
        owner.prepare("fixture text", stamp(1)).unwrap();
        let request = Request::Publish {
            // A wrong stamp gets a definitive native refusal, not uncertainty.
            stamp: stamp(if submitted { 1 } else { 2 }),
            not_after_ns: owner.not_after_ns(later(&cx, 500_000)).unwrap(),
        };
        let reply = owner.round_trip(request, None, REPLY_TIMEOUT).unwrap();
        let publication = if submitted {
            Publication::SubmittedToOs
        } else {
            Publication::NotSubmitted(PlatformError::Unavailable)
        };
        assert_eq!(
            reply.0,
            Reply::Published {
                revision: if submitted { 2 } else { 1 },
                publication,
            }
        );
        assert!(reply.1.is_none());
        // The byte/EOF monitor has its own tests. Stop exactly after receipt
        // validation here, without sleeps or a race against a native deadline.
        owner.privacy.as_ref().unwrap().stop();
        assert!(!original.status().enabled);
        assert_eq!(
            original.set_enabled(true),
            Err(crate::clipboard_quic::Error::Closed)
        );
        let retained = owner.finish_exchange(request, Ok(reply)).unwrap();
        assert_eq!(
            retained.0,
            Reply::Published {
                revision: if submitted { 2 } else { 1 },
                publication,
            }
        );
        assert!(retained.1.is_none());
        assert!(
            owner.is_poisoned(),
            "receipt retention is not authorization"
        );
        let before = transcript(&log);
        assert_eq!(owner.watch(), Err(RemoteError::Channel));
        assert_eq!(
            owner.publish_until("fixture text", stamp(1), later(&cx, 500_000)),
            Publication::NotSubmitted(PlatformError::Unavailable)
        );
        NativeClipboard::close(&mut owner);
        assert!(owner.child.is_none() && owner.privacy.is_none());
        assert_eq!(transcript(&log), before, "no retry or Stop after the fence");
    }
}

#[test]
fn a_read_completed_before_the_privacy_fence_still_discards_its_text() {
    let runtime = runtime();
    let cx = runtime.request_cx_with_budget(Budget::INFINITE);
    let app = application();
    let (image, log) = fixture("normal");
    let mut owner =
        RemoteClipboard::start_inner(&launch(&image), cx, 4096, Some(app.control())).unwrap();
    owner.watch().unwrap();
    local_copy(&log, "private fixture clipboard");
    owner.changes().unwrap();
    owner.begin_read().unwrap();
    let request = Request::PollRead;
    let reply = owner.round_trip(request, None, REPLY_TIMEOUT).unwrap();
    assert!(matches!(reply.0, Reply::ReadText { .. }));
    assert!(reply.1.is_some());
    owner.privacy.as_ref().unwrap().stop();
    assert!(owner.finish_exchange(request, Ok(reply)).is_err());
    assert!(owner.is_poisoned());
    assert_eq!(owner.poll_read().err(), Some(RemoteError::Channel));
    NativeClipboard::close(&mut owner);
    assert!(owner.child.is_none() && owner.privacy.is_none());
}

#[test]
fn a_publication_reply_cannot_replace_another_request_after_privacy_stops() {
    let runtime = runtime();
    let cx = runtime.request_cx_with_budget(Budget::INFINITE);
    let (image, _log) = fixture("normal");
    let mut owner = RemoteClipboard::start(&launch(&image), cx.clone(), 4096).unwrap();
    owner.watch().unwrap();
    owner.prepare("fixture text", stamp(1)).unwrap();
    let request = Request::Publish {
        stamp: stamp(1),
        not_after_ns: owner.not_after_ns(later(&cx, 500_000)).unwrap(),
    };
    let reply = owner.round_trip(request, None, REPLY_TIMEOUT).unwrap();
    assert!(matches!(reply.0, Reply::Published { .. }));
    owner.privacy.as_ref().unwrap().stop();
    assert!(owner.finish_exchange(Request::Watch, Ok(reply)).is_err());
    assert!(owner.is_poisoned());
    NativeClipboard::close(&mut owner);
    assert!(owner.child.is_none() && owner.privacy.is_none());
}

#[test]
fn privacy_stopped_before_publication_is_a_definitive_no_submission() {
    let runtime = runtime();
    let cx = runtime.request_cx_with_budget(Budget::INFINITE);
    let app = application();
    let (image, log) = fixture("normal");
    let mut owner =
        RemoteClipboard::start_inner(&launch(&image), cx.clone(), 4096, Some(app.control()))
            .unwrap();
    owner.watch().unwrap();
    owner.prepare("fixture text", stamp(1)).unwrap();
    owner.privacy.as_ref().unwrap().stop();
    let before = owner.sequence;
    assert_eq!(
        owner.publish_until("fixture text", stamp(1), later(&cx, 500_000)),
        Publication::NotSubmitted(PlatformError::Unavailable)
    );
    assert_eq!(owner.sequence, before, "no request entered the channel");
    assert!(owner.is_poisoned());
    assert!(line(&log, "PUBLISH").is_none());
    NativeClipboard::close(&mut owner);
    assert!(owner.child.is_none() && owner.privacy.is_none());
}
