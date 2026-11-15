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
        Capability::Text,
    ] {
        assert!(caps.contains(capability), "{capability:?}");
    }
    assert!(!caps.contains(Capability::Repeat));
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
    assert_eq!(shared.begin_typing(), Err(InputResult::NotControlled));
    assert_eq!(
        shared.type_captured(1, 0, 0x04, 0),
        InputResult::NotControlled
    );
    shared.end_typing(1);
    assert!(matches!(
        shared.capture_input(),
        Err(InputResult::NotControlled)
    ));
    assert_eq!(shared.text("a"), InputResult::NotControlled);
    assert!(!shared.supports_text());
    assert!(matches!(
        shared.begin_text(),
        Err(InputResult::NotControlled)
    ));
    assert_eq!(shared.text(""), InputResult::Invalid);
    assert_eq!(shared.text("before\0after"), InputResult::Invalid);
    assert_eq!(
        shared.text(&"a".repeat(fr_core::input::MAX_COMMITTED_TEXT_BYTES + 1)),
        InputResult::Invalid
    );
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
fn native_key_capture_cannot_fabricate_a_grant_and_failure_is_typed() {
    let shared = Shared::new();
    assert!(matches!(
        shared.capture_key(0x04, true),
        Err(InputResult::NotControlled)
    ));
    assert!(matches!(
        shared.capture_key(0, true),
        Err(InputResult::Invalid)
    ));
    shared.set_visible(true);
    shared.request_control();
    assert!(shared.want_control.load(Ordering::SeqCst));
    shared.input_failed();
    assert!(!shared.want_control.load(Ordering::SeqCst));
    assert!(!shared.close.load(Ordering::SeqCst));
    assert_eq!(
        LocalStop::from_code(shared.local_stop.load(Ordering::SeqCst)),
        Some(LocalStop::InputRefused)
    );
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
fn native_control_choice_needs_the_original_ready_view_and_never_grants_input() {
    let shared = Shared::new();
    assert_eq!(shared.control_state(), ControlState::WaitingForView);
    assert!(shared.control_request().is_none());
    shared.refresh_control_request(true).unwrap();
    assert!(shared.control_request().is_none());
    shared.set_visible(true);
    shared.refresh_control_request(true).unwrap();
    assert_eq!(shared.control_state(), ControlState::Viewing);
    let request = shared.control_request().unwrap();
    assert!(shared.request_control_for(&request, Duration::ZERO));
    assert_eq!(shared.control_state(), ControlState::Requesting);
    assert!(!shared.request_control_for(&request, Duration::ZERO));
    assert!(shared.take_control_request().unwrap());
    assert!(!shared.take_control_request().unwrap());
    assert_eq!(shared.control_state(), ControlState::Requesting);
    assert_eq!(shared.key(0x04, true), InputResult::NotControlled);
    assert!(!shared.controlled());
    shared.close();
    assert_eq!(shared.control_state(), ControlState::Closing);
    assert!(!shared.request_control_for(&request, Duration::ZERO));
}

#[test]
fn observation_only_profile_never_offers_or_queues_control() {
    let view_only = Shared::with_control(false);
    view_only.set_visible(true);
    view_only.refresh_control_request(true).unwrap();
    view_only.request_control();
    assert!(view_only.control_request().is_none());
    assert!(!view_only.want_control.load(Ordering::SeqCst));
    assert_eq!(view_only.control_state(), ControlState::Unavailable);
    assert!(!view_only.take_control_request().unwrap());
}

#[test]
fn hiding_and_retirement_discard_queued_and_unused_control_choices() {
    let shared = Shared::new();
    shared.set_visible(true);
    shared.refresh_control_request(true).unwrap();
    let request = shared.control_request().unwrap();
    assert!(shared.request_control_for(&request, Duration::ZERO));
    shared.request_control();
    shared.set_visible(false);
    shared.set_visible(true);
    shared.refresh_control_request(true).unwrap();
    assert!(!shared.take_control_request().unwrap());
    assert!(!shared.request_control_for(&request, Duration::ZERO));
    let next = shared.control_request().unwrap();
    assert!(shared.request_control_for(&next, Duration::ZERO));
    shared.retire_control_request();
    shared.refresh_control_request(true).unwrap();
    assert!(!shared.take_control_request().unwrap());
    assert!(!shared.request_control_for(&next, Duration::ZERO));
}

#[test]
fn an_app_switch_retires_unsent_choices_but_neither_hides_nor_stops() {
    let shared = Shared::new();
    shared.set_visible(true);
    assert!(
        shared.active(),
        "shells that never call set_active stay active"
    );
    shared.refresh_control_request(true).unwrap();
    let request = shared.control_request().unwrap();
    assert!(shared.request_control_for(&request, Duration::ZERO));
    assert!(shared.set_active(false), "nothing held, nothing refused");
    assert!(!shared.active());
    // The choice made before the switch is gone, and none is offered or
    // taken while the app is in the background.
    assert!(!shared.request_control_for(&request, Duration::ZERO));
    shared.refresh_control_request(true).unwrap();
    assert!(shared.control_request().is_none());
    shared.request_control();
    assert!(!shared.take_control_request().unwrap());
    // Unlike hiding: no hide is counted, no local stop recorded, nothing
    // reported, and the session is not closing (only no choice is offered).
    assert_eq!(shared.hides.load(Ordering::SeqCst), 0);
    assert_eq!(shared.local_stop.load(Ordering::SeqCst), 0);
    assert!(shared.events.is_empty());
    assert_eq!(shared.control_state(), ControlState::WaitingForView);
    // Without a grant the grant check still answers first.
    assert_eq!(shared.key(0x04, true), InputResult::NotControlled);
    // Returning offers a fresh choice; the old one stays spent.
    assert!(shared.set_active(true));
    shared.refresh_control_request(true).unwrap();
    let fresh = shared.control_request().unwrap();
    assert!(!shared.request_control_for(&request, Duration::ZERO));
    assert!(shared.request_control_for(&fresh, Duration::ZERO));
    assert!(shared.take_control_request().unwrap());
    // Planted negative: hiding the same view still counts and retires.
    shared.set_visible(false);
    assert_eq!(shared.hides.load(Ordering::SeqCst), 1);
}

#[test]
fn expired_native_gesture_or_original_input_failure_cannot_queue_control() {
    let shared = Shared::new();
    shared.set_visible(true);
    shared.refresh_control_request(true).unwrap();
    let request = shared.control_request().unwrap();
    assert!(!shared.request_control_for(&request, control_request::MAX_GESTURE_AGE));
    assert!(!shared.take_control_request().unwrap());
    shared.input_failed();
    shared.request_control();
    shared.refresh_control_request(true).unwrap();
    assert!(shared.control_request().is_none());
    assert!(!shared.want_control.load(Ordering::SeqCst));
    assert!(!shared.request_control_for(&request, Duration::ZERO));
    assert_eq!(shared.control_state(), ControlState::Closing);
}

fn display_catalog() -> Catalog {
    Catalog::new(
        1,
        &[Display {
            handle: 19,
            geometry: fr_core::ids::DisplayGeometryGeneration::INITIAL,
            x: -1920,
            y: 32,
            pixel_width: 1920,
            pixel_height: 1080,
            logical_width: 1920,
            logical_height: 1080,
            scale_numerator: 1,
            scale_denominator: 1,
            rotation: 0,
        }],
        &fr_core::limits::ProtocolLimits::ABSOLUTE,
    )
    .unwrap()
}

#[test]
fn display_choice_requires_the_approved_callback_and_never_grants_input() {
    let shared = Shared::new();
    assert!(shared.display_request().is_none());
    shared.begin_display().unwrap();
    assert!(shared.display_request().is_none());
    let catalog = display_catalog();
    assert_eq!(
        shared.choose_display(DisplayChoice::Choose, &catalog),
        Ok(None)
    );
    let request = shared.display_request().unwrap();
    assert!(shared.display_request_current(&request));
    assert_eq!(
        shared.select_display(&request, 20),
        Err(DisplayError::MissingDisplay)
    );
    assert_eq!(shared.select_display(&request, 19), Ok(()));
    assert!(shared.display_request().is_none());
    assert_eq!(
        shared.choose_display(DisplayChoice::Choose, &catalog),
        Ok(Some(19))
    );
    assert!(!shared.controlled());
    assert_eq!(shared.key(0x04, true), InputResult::NotControlled);
    assert!(!shared.want_control.load(Ordering::SeqCst));
}

#[test]
fn stale_picker_cancel_cannot_close_a_new_attempt_with_identical_metadata() {
    let shared = Shared::new();
    let catalog = display_catalog();
    shared.begin_display().unwrap();
    shared
        .choose_display(DisplayChoice::Choose, &catalog)
        .unwrap();
    let old = shared.display_request().unwrap();
    shared.retire_display();
    shared.begin_display().unwrap();
    shared
        .choose_display(DisplayChoice::Choose, &catalog)
        .unwrap();
    let current = shared.display_request().unwrap();
    assert_eq!(shared.cancel_display(&old), Err(DisplayError::StaleRequest));
    assert!(!shared.close.load(Ordering::SeqCst));
    assert!(shared.display_request_current(&current));
    assert_eq!(shared.cancel_display(&current), Ok(()));
    assert!(shared.close.load(Ordering::SeqCst));
    assert!(shared.display_request().is_none());
    assert_eq!(
        shared.select_display(&current, 19),
        Err(DisplayError::Closed)
    );
    assert_eq!(shared.begin_display(), Err(ObserverError::Application));
}

#[test]
fn local_close_discards_a_queued_display_choice_before_startup_can_consume_it() {
    let shared = Shared::new();
    let catalog = display_catalog();
    shared.begin_display().unwrap();
    shared
        .choose_display(DisplayChoice::Choose, &catalog)
        .unwrap();
    let request = shared.display_request().unwrap();
    shared.select_display(&request, 19).unwrap();
    shared.close();
    assert_eq!(
        shared.choose_display(DisplayChoice::Choose, &catalog),
        Err(())
    );
    assert!(!shared.display_request_current(&request));
    assert!(shared.display_request().is_none());
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
    shared.frames.begin_attempt().unwrap();
    shared.frames.mark_drawn(shared.frames.app_id(500));
    // A reconnect: frd's frame ids start again from low numbers.
    shared.frames.begin_attempt().unwrap();
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
fn a_retired_file_chooser_cannot_hide_a_new_view_or_spend_its_control_choice() {
    let shared = Shared::new();
    shared.set_visible(true);
    shared.refresh_control_request(true).unwrap();
    let request = shared.control_request().unwrap();
    let clipboard = shared.clipboard_report();
    // An orphaned original token represents a callback whose granted source
    // has already retired. It is not evidence of a live native input grant.
    let original = Arc::new(AtomicBool::new(false));
    assert!(!shared.hide_file_source(&original));
    assert!(original.load(Ordering::SeqCst));
    assert!(shared.visible.load(Ordering::SeqCst));
    assert_eq!(shared.hides.load(Ordering::SeqCst), 0);
    assert_eq!(shared.local_stop.load(Ordering::SeqCst), 0);
    assert!(!shared.close.load(Ordering::SeqCst));
    assert_eq!(shared.clipboard_report(), clipboard);
    assert!(shared.request_control_for(&request, Duration::ZERO));
    // Even a repeated late callback cannot cancel the new explicit request.
    assert!(!shared.hide_file_source(&original));
    assert!(shared.take_control_request().unwrap());
}

#[test]
fn focus_continuity_requires_an_actual_published_grant_not_a_view_or_request() {
    let shared = Shared::new();
    shared.set_visible(true);
    shared.serving.store(true, Ordering::SeqCst);
    shared.refresh_control_request(true).unwrap();
    let request = shared.control_request().unwrap();
    let clipboard = shared.clipboard_report();
    assert!(!shared.close_for_focus_loss(shared.frames.app_id(0)));
    assert!(!shared.close.load(Ordering::SeqCst));
    assert!(!shared.focus_close_requested.load(Ordering::SeqCst));
    assert!(!shared.focus_closed());
    assert!(shared.visible.load(Ordering::SeqCst));
    assert_eq!(shared.local_stop.load(Ordering::SeqCst), 0);
    assert_eq!(shared.clipboard_report(), clipboard);
    assert!(shared.request_control_for(&request, Duration::ZERO));
    // A queued control gesture remains a request, never an attached grant.
    assert!(!shared.close_for_focus_loss(shared.frames.app_id(0)));
    assert!(shared.take_control_request().unwrap());
    shared.close();
    assert!(!shared.close_for_focus_loss(shared.frames.app_id(0)));
    assert!(!shared.focus_close_requested.load(Ordering::SeqCst));
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
    // A later close cannot hide the first local reason control stopped.
    assert_eq!(
        ending(Ok(()), true, hidden, None),
        (end::CONTROL_STOPPED, Some("Hidden".into()))
    );
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

#[test]
fn presentation_failure_closes_a_view_only_owner_and_preserves_earlier_stops() {
    let shared = Shared::new();
    shared.set_visible(true);
    shared.request_control();
    shared.presentation_failed();
    assert!(shared.close.load(Ordering::SeqCst));
    assert!(!shared.want_control.load(Ordering::SeqCst));
    assert_eq!(
        LocalStop::from_code(shared.local_stop.load(Ordering::SeqCst)),
        Some(LocalStop::PresentationFailed),
    );
    let earlier = Shared::new();
    earlier.note_local_stop(LocalStop::Hidden);
    earlier.presentation_failed();
    assert!(earlier.close.load(Ordering::SeqCst));
    assert_eq!(
        LocalStop::from_code(earlier.local_stop.load(Ordering::SeqCst)),
        Some(LocalStop::Hidden),
    );
}

#[test]
fn legacy_control_installation_cannot_finish_after_its_attempt_retired() {
    let shared = Shared::new();
    // Reproduce a native call interrupted before acquiring the requests lock.
    let original = shared.control_retirement.capture().unwrap();
    shared.retire_control_request();
    shared.request_control_at(original);
    assert!(!shared.want_control.load(Ordering::SeqCst));
    // A new explicit call can wait for this attempt's first visible frame.
    shared.request_control();
    assert!(shared.want_control.load(Ordering::SeqCst));
    let interrupted = shared.control_retirement.capture().unwrap();
    shared.retire_control_request();
    shared.request_control_at(interrupted);
    assert!(!shared.want_control.load(Ordering::SeqCst));
    shared.close();
    shared.request_control();
    assert!(!shared.want_control.load(Ordering::SeqCst));
}

#[test]
fn retained_picture_range_does_not_follow_a_new_decoder_configuration() {
    use crate::video::tests::{picture, release};

    for retained_range in [false, true] {
        let shared = Shared::new();
        shared.frames.begin_attempt().unwrap();
        let attempt = shared.frames.app_id(0);
        shared
            .publish_configuration(attempt, (128, 96), retained_range, None)
            .unwrap();
        shared
            .frames
            .publish_for(attempt, picture(retained_range), 1)
            .unwrap();
        // Configuration is acknowledged before its first picture. A resize
        // or cursor update can repaint the preceding decoder's retained image
        // during this gap, using that image's original range in BOTH directions.
        shared
            .publish_configuration(attempt, (128, 96), !retained_range, None)
            .unwrap();
        assert_eq!(shared.full_range(), !retained_range);
        let (raw, retained) = shared.frames.take_newer(0).unwrap();
        release(raw);
        assert_eq!(retained.full_range, retained_range);
        assert_eq!(retained.frame, attempt | 1);
        shared
            .frames
            .publish_for(attempt, picture(!retained_range), 2)
            .unwrap();
        let (raw, current) = shared.frames.take_newer(retained.sequence).unwrap();
        release(raw);
        assert_eq!(current.full_range, !retained_range);
        assert_eq!(current.frame, attempt | 2);
    }
}

#[test]
fn native_configuration_returning_after_retirement_publishes_no_video_or_metadata() {
    let shared = Shared::new();
    shared.frames.begin_attempt().unwrap();
    let original = shared.frames.app_id(0);
    shared
        .publish_configuration(original, (128, 96), false, Some(false))
        .unwrap();
    assert_eq!(shared.events.poll().unwrap().kind, kind::VIDEO);

    // The native configure call is still running when its bounded cleanup
    // fails. This is the real terminal path: retire presentation, then ENDED.
    shared.clear_presentation();
    finish(&shared, end::INTERNAL, Some("Cleanup".into()));
    assert!(shared.finished());
    assert!(
        shared
            .publish_configuration(original, (640, 480), true, Some(true))
            .is_err()
    );
    assert_eq!(*shared.picture.lock().unwrap(), (128, 96));
    assert!(!shared.full_range());
    assert_eq!(shared.hardware(), 0);
    assert_eq!(shared.events.poll().unwrap().kind, kind::ENDED);
    assert!(shared.events.poll().is_none(), "ENDED remains last");
}

#[test]
fn configuration_metadata_cannot_migrate_to_another_attempt_or_publish_during_close() {
    let shared = Shared::new();
    shared.frames.begin_attempt().unwrap();
    let original = shared.frames.app_id(0);
    shared.frames.begin_attempt().unwrap();
    let current = shared.frames.app_id(0);
    shared
        .publish_configuration(current, (128, 96), false, None)
        .unwrap();
    assert_eq!(shared.events.poll().unwrap().kind, kind::VIDEO);
    assert!(
        shared
            .publish_configuration(original, (640, 480), true, Some(true))
            .is_err()
    );
    shared.close();
    assert!(
        shared
            .publish_configuration(current, (640, 480), true, Some(true))
            .is_err()
    );
    assert_eq!(*shared.picture.lock().unwrap(), (128, 96));
    assert!(!shared.full_range());
    assert_eq!(shared.hardware(), -1);
    assert!(shared.events.poll().is_none());
}
