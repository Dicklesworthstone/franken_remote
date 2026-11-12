//! Session rules that hold without a host: failure classification, the
//! control request, and input refusal before any grant. The live session is
//! proven against a real frd (docs/ios-client.md), not here.
use super::*;
use fr_tailnet::Error as T;
use frd::native_connection::Error as C;

#[test]
fn failures_map_to_the_apps_end_codes() {
    let cases = [
        (
            Failure::Connection(C::Tailnet(T::NameUnresolved)),
            end::NAME_UNRESOLVED,
        ),
        (
            Failure::Connection(C::Tailnet(T::OutsideTailnet)),
            end::OUTSIDE_TAILNET,
        ),
        (
            Failure::Connection(C::Tailnet(T::IdentityChanged)),
            end::IDENTITY_CHANGED,
        ),
        (
            Failure::Connection(C::Tailnet(T::NativeHandshake)),
            end::TLS_REFUSED,
        ),
        (
            Failure::Connection(C::Tailnet(T::PeerCertificateRejected)),
            end::TLS_REFUSED,
        ),
        (
            Failure::Connection(C::Tailnet(T::HandshakeTimeout)),
            end::UNREACHABLE,
        ),
        (
            Failure::Connection(C::Tailnet(T::NativeRouteIo)),
            end::UNREACHABLE,
        ),
        (
            Failure::Connection(C::Tailnet(T::Timeout)),
            end::UNREACHABLE,
        ),
        (
            Failure::Connection(C::Tailnet(T::Busy)),
            end::SESSION_FAILED,
        ),
        (Failure::Connection(C::Tailnet(T::HostBusy)), end::HOST_BUSY),
        (
            Failure::Connection(C::Session(frd::session_startup::Error::Denied)),
            end::HOST_REFUSED,
        ),
        (Failure::Cancelled, end::CANCELLED),
        (Failure::Cleanup, end::INTERNAL),
    ];
    for (failure, code) in cases {
        assert_eq!(classify(failure, false), code, "{failure:?}");
    }
    // A close this app asked for is an orderly end, not a failure...
    assert_eq!(classify(Failure::Cancelled, true), end::OK);
    // ...but it never hides why a connection was refused (planted negative:
    // the same refusal with the close flag keeps its code).
    assert_eq!(
        classify(Failure::Connection(C::Tailnet(T::IdentityChanged)), true),
        end::IDENTITY_CHANGED
    );
}

#[test]
fn the_control_request_never_asks_for_client_side_repeat() {
    let caps = requested_capabilities();
    for capability in [
        Capability::Keys,
        Capability::Absolute,
        Capability::Buttons,
        Capability::LineScroll,
    ] {
        assert!(caps.contains(capability), "{capability:?}");
    }
    assert!(!caps.contains(Capability::Repeat));
    assert!(!caps.contains(Capability::Text));
}

#[test]
fn input_is_refused_until_control_is_granted() {
    let shared = Shared::new();
    assert_eq!(shared.pointer(1.0, 1.0), InputResult::NotControlled);
    assert_eq!(shared.button(0, true, 1.0, 1.0), InputResult::NotControlled);
    assert_eq!(
        shared.scroll(0.0, 3.0, 1.0, 1.0),
        InputResult::NotControlled
    );
    assert_eq!(shared.key(0x04, true), InputResult::NotControlled);
    assert_eq!(shared.text("a"), InputResult::NotControlled);
    assert_eq!(shared.release_all(), InputResult::NotControlled);
    // Values the protocol cannot carry are refused before the grant check.
    assert_eq!(shared.button(5, true, 1.0, 1.0), InputResult::Invalid);
    assert_eq!(shared.key(0, true), InputResult::Invalid);
    assert!(!shared.controlled());
    // Hiding or closing without a grant reports no control change.
    shared.set_visible(false);
    shared.close();
    shared.close();
    assert!(shared.events.is_empty());
    assert_eq!(shared.end_code(), u32::MAX);
}

#[test]
fn releasing_control_closes_the_session_and_stops_any_pending_request() {
    let shared = Shared::new();
    shared.set_visible(true);
    shared.request_control();
    assert!(shared.want_control.load(Ordering::SeqCst));
    shared.release_control();
    assert!(shared.close.load(Ordering::SeqCst));
    assert!(!shared.want_control.load(Ordering::SeqCst));
    // Planted negative: a plain hide does not close.
    let other = Shared::new();
    other.set_visible(false);
    assert!(!other.close.load(Ordering::SeqCst));
}

#[test]
fn a_submitted_picture_counts_as_shown_once_the_app_has_drawn_it() {
    use fr_media::access_unit::FrameId;
    let picture = |raw, stage| {
        Some(Presentation {
            frame: FrameId::from_raw(raw),
            stage,
        })
    };
    let submitted = |raw| picture(raw, PresentationStage::SubmittedToCompositor);
    let shared = Shared::new();
    shared.set_visible(true);
    let mut pending = None;
    // Planted race: frd submits picture 7 before the Metal pass draws it.
    // Checking only at submission (the old rule) never saw a drawn picture.
    assert_eq!(shown(&shared, &mut pending, submitted(7)), None);
    assert_eq!(shown(&shared, &mut pending, None), None);
    shared.frames.mark_drawn(7);
    assert_eq!(shown(&shared, &mut pending, None), Some(7));
    assert_eq!(shown(&shared, &mut pending, None), None, "reported once");
    // A newer submission replaces an undrawn one.
    assert_eq!(shown(&shared, &mut pending, submitted(8)), None);
    assert_eq!(shown(&shared, &mut pending, submitted(9)), None);
    shared.frames.mark_drawn(8);
    assert_eq!(shown(&shared, &mut pending, None), None);
    shared.frames.mark_drawn(9);
    assert_eq!(shown(&shared, &mut pending, None), Some(9));
    // A picture the app drew before frd's callback counts at once.
    shared.frames.mark_drawn(10);
    assert_eq!(shown(&shared, &mut pending, submitted(10)), Some(10));
    // Decoded-only pictures and a hidden view never count.
    assert_eq!(
        shown(
            &shared,
            &mut pending,
            picture(11, PresentationStage::DecodedOnly)
        ),
        None
    );
    shared.frames.mark_drawn(11);
    assert_eq!(shown(&shared, &mut pending, None), None);
    assert_eq!(shown(&shared, &mut pending, submitted(12)), None);
    shared.set_visible(false);
    shared.frames.mark_drawn(12);
    assert_eq!(shown(&shared, &mut pending, None), None);
    // ...but a picture submitted while hidden stays pending: an idle host
    // may send nothing newer once the view is back.
    assert_eq!(shown(&shared, &mut pending, submitted(13)), None);
    shared.frames.mark_drawn(13);
    assert_eq!(shown(&shared, &mut pending, None), None);
    shared.set_visible(true);
    assert_eq!(shown(&shared, &mut pending, None), Some(13));
}

#[test]
fn a_picture_drawn_in_an_earlier_attempt_never_counts_in_a_later_one() {
    use fr_media::access_unit::FrameId;
    let submitted = |raw| {
        Some(Presentation {
            frame: FrameId::from_raw(raw),
            stage: PresentationStage::SubmittedToCompositor,
        })
    };
    let shared = Shared::new();
    shared.set_visible(true);
    shared.frames.begin_attempt();
    shared.frames.mark_drawn(shared.frames.app_id(500));
    // A reconnect: frd's frame ids start again from low numbers.
    shared.frames.begin_attempt();
    let mut pending = None;
    // Planted negative: compared without the generation, the earlier
    // attempt's 500 would make picture 3 count before it was drawn.
    assert!(shared.frames.drawn() & ((1 << 44) - 1) >= 3);
    assert_eq!(shown(&shared, &mut pending, submitted(3)), None);
    shared.frames.mark_drawn(shared.frames.app_id(3));
    assert_eq!(shown(&shared, &mut pending, None), Some(3));
    assert_eq!(shared.frames.drawn_raw(), 3);
}

#[test]
fn hiding_counts_once_per_visible_spell_and_without_a_grant_stops_nothing() {
    let shared = Shared::new();
    shared.set_visible(false);
    assert_eq!(shared.hides.load(Ordering::SeqCst), 0, "never shown");
    shared.set_visible(true);
    shared.set_visible(false);
    shared.set_visible(false);
    assert_eq!(shared.hides.load(Ordering::SeqCst), 1);
    shared.set_visible(true);
    shared.set_visible(false);
    assert_eq!(shared.hides.load(Ordering::SeqCst), 2);
    // No grant was fenced, so no local stop is recorded.
    assert_eq!(shared.local_stop.load(Ordering::SeqCst), 0);
    // The first recorded reason wins.
    shared.note_local_stop(LocalStop::Hidden);
    shared.note_local_stop(LocalStop::InputRefused);
    assert_eq!(
        LocalStop::from_code(shared.local_stop.load(Ordering::SeqCst)),
        Some(LocalStop::Hidden)
    );
}

#[test]
fn the_end_code_says_who_ended_the_session() {
    let hidden = Some(LocalStop::Hidden);
    let observed = Failure::Observation(ObserverError::Application);
    // The host ended an orderly session this device did not stop.
    assert_eq!(ending(Ok(()), false, None, None), (end::HOST_CLOSED, None));
    // Planted negative (the old rule mapped this to HOST_CLOSED): this
    // device stopped control, which ended the session.
    assert_eq!(
        ending(Ok(()), false, hidden, None),
        (end::CONTROL_STOPPED, Some("Hidden".into()))
    );
    assert_eq!(
        ending(Err(observed), false, hidden, None).0,
        end::CONTROL_STOPPED
    );
    // This app's own close stays an orderly end.
    assert_eq!(ending(Ok(()), true, hidden, None), (end::OK, None));
    // A refused connection keeps its reason even after a local stop.
    assert_eq!(
        ending(
            Err(Failure::Connection(C::Tailnet(T::IdentityChanged))),
            false,
            hidden,
            None
        )
        .0,
        end::IDENTITY_CHANGED
    );
    // A close during a reconnect (the status callback refuses to go on).
    assert_eq!(classify(Failure::Notification, true), end::OK);
    assert_ne!(classify(Failure::Notification, false), end::OK);
}
