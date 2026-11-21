//! Compressed-reference custody and decode/presentation progress remain
//! separate under pressure, hidden windows and replacement decoder callbacks.
use fr_client::{
    ClientSession, CloseReason, QueuedPicture, ReconnectPolicy, ReconnectReason, SessionError,
    input::ClientInstant, session::DecoderScheduler,
};
use fr_core::ids::{HostBootId, InputLeaseId, InputTicketId, OsSessionId, RemoteSessionId};
use fr_wire::negotiation::ControlBinding;

fn picture(frame_number: u64, is_reference: bool, byte_size: usize) -> QueuedPicture {
    QueuedPicture {
        frame_number,
        is_reference,
        byte_size,
        captured_at_us: frame_number,
    }
}

fn decode(scheduler: &mut DecoderScheduler, frame: u64) {
    let work = scheduler.next_decode().expect("queued decode");
    assert_eq!(work.picture().frame_number, frame);
    scheduler.on_frame_decoded(work).unwrap();
}

#[test]
fn oversized_picture_is_refused_before_admission_or_eviction() {
    let mut scheduler = DecoderScheduler::new(2, 100);
    scheduler.enqueue(picture(1, false, 30)).unwrap();
    assert_eq!(
        scheduler.enqueue(picture(2, true, 101)),
        Err(SessionError::PictureTooLarge)
    );
    assert_eq!(scheduler.queued_count(), 1);
    assert_eq!(scheduler.queued_bytes(), 30);
    decode(&mut scheduler, 1);
    assert_eq!(scheduler.select_presentation_frame(true), Some(1));
}

#[test]
fn exact_count_and_byte_limits_are_honored_without_reference_eviction() {
    for (count, bytes) in [(2, 1_000), (4, 100)] {
        let mut scheduler = DecoderScheduler::new(count, bytes);
        scheduler.enqueue(picture(1, true, 50)).unwrap();
        scheduler.enqueue(picture(2, true, 50)).unwrap();
        assert_eq!(
            scheduler.enqueue(picture(3, true, 1)),
            Err(SessionError::DecoderBackpressure)
        );
        assert_eq!(scheduler.queued_count(), 2);
        assert_eq!(scheduler.queued_bytes(), 100);
        decode(&mut scheduler, 1);
        decode(&mut scheduler, 2);
        assert_eq!(scheduler.select_presentation_frame(true), Some(2));
        assert_eq!(scheduler.queued_bytes(), 0);
        assert_eq!(scheduler.select_presentation_frame(true), None);
    }
}

#[test]
fn failed_admission_preserves_even_replaceable_work_when_it_cannot_make_room() {
    let mut scheduler = DecoderScheduler::new(4, 100);
    scheduler.enqueue(picture(1, true, 80)).unwrap();
    scheduler.enqueue(picture(2, false, 10)).unwrap();
    assert_eq!(
        scheduler.enqueue(picture(3, true, 30)),
        Err(SessionError::DecoderBackpressure)
    );
    assert_eq!(scheduler.queued_count(), 2);
    assert_eq!(scheduler.queued_bytes(), 90);
    decode(&mut scheduler, 1);
    decode(&mut scheduler, 2);
    assert_eq!(scheduler.select_presentation_frame(true), Some(2));
}

#[test]
fn only_queued_nonreferences_make_room_before_decode() {
    let mut scheduler = DecoderScheduler::new(3, 100);
    scheduler.enqueue(picture(1, true, 40)).unwrap();
    scheduler.enqueue(picture(2, false, 20)).unwrap();
    scheduler.enqueue(picture(3, false, 20)).unwrap();
    scheduler.enqueue(picture(4, true, 50)).unwrap();
    assert_eq!(scheduler.queued_count(), 2);
    assert_eq!(scheduler.queued_bytes(), 90);
    decode(&mut scheduler, 1);
    decode(&mut scheduler, 4);
    assert_eq!(scheduler.select_presentation_frame(true), Some(4));
}

#[test]
fn in_flight_nonreference_stays_charged_and_cannot_be_replaced() {
    let mut scheduler = DecoderScheduler::new(1, 100);
    scheduler.enqueue(picture(1, false, 80)).unwrap();
    let original = scheduler.next_decode().unwrap();
    assert!(scheduler.next_decode().is_none());
    assert_eq!(
        scheduler.enqueue(picture(2, false, 20)),
        Err(SessionError::DecoderBackpressure)
    );
    assert_eq!(scheduler.queued_count(), 1);
    assert_eq!(scheduler.queued_bytes(), 80);
    assert_eq!(scheduler.select_presentation_frame(true), None);
    scheduler.on_frame_decoded(original).unwrap();
    scheduler.enqueue(picture(2, false, 20)).unwrap();
    assert_eq!(scheduler.queued_bytes(), 20);
    decode(&mut scheduler, 2);
    assert_eq!(scheduler.select_presentation_frame(true), Some(2));
}

#[test]
fn presentation_requires_decode_and_never_goes_back_to_skipped_pictures() {
    let mut scheduler = DecoderScheduler::new(4, 100);
    for frame in 1..=3 {
        scheduler.enqueue(picture(frame, true, 10)).unwrap();
    }
    assert_eq!(scheduler.select_presentation_frame(true), None);
    decode(&mut scheduler, 1);
    decode(&mut scheduler, 2);
    assert_eq!(scheduler.select_presentation_frame(true), Some(2));
    assert_eq!(scheduler.queued_count(), 1);
    assert_eq!(scheduler.queued_bytes(), 10);
    assert_eq!(scheduler.select_presentation_frame(true), None);
    decode(&mut scheduler, 3);
    assert_eq!(scheduler.select_presentation_frame(true), Some(3));
    assert_eq!(scheduler.select_presentation_frame(true), None);
}

#[test]
fn hidden_presentation_retains_decoded_work_within_the_same_bound() {
    let mut scheduler = DecoderScheduler::new(2, 100);
    scheduler.enqueue(picture(1, true, 50)).unwrap();
    decode(&mut scheduler, 1);
    assert_eq!(scheduler.select_presentation_frame(false), None);
    assert_eq!(scheduler.queued_bytes(), 50);
    scheduler.enqueue(picture(2, true, 100)).unwrap();
    assert_eq!(scheduler.queued_count(), 1);
    assert_eq!(scheduler.queued_bytes(), 100);
    assert_eq!(scheduler.select_presentation_frame(true), None);
    decode(&mut scheduler, 2);
    assert_eq!(scheduler.select_presentation_frame(true), Some(2));
}

#[test]
fn old_decoder_callback_cannot_complete_a_same_number_replacement_picture() {
    let mut original = DecoderScheduler::new(1, 100);
    original.enqueue(picture(7, true, 100)).unwrap();
    let callback = original.next_decode().unwrap();
    drop(original);
    let mut replacement = DecoderScheduler::new(1, 100);
    replacement.enqueue(picture(7, true, 100)).unwrap();
    let current = replacement.next_decode().unwrap();
    assert_eq!(
        replacement.on_frame_decoded(callback),
        Err(SessionError::StaleDecode)
    );
    assert_eq!(replacement.queued_bytes(), 100);
    assert!(replacement.next_decode().is_none());
    assert_eq!(replacement.select_presentation_frame(true), None);
    replacement.on_frame_decoded(current).unwrap();
    assert_eq!(replacement.select_presentation_frame(true), Some(7));
}

#[test]
fn zero_count_and_exact_small_byte_budgets_are_not_silently_widened() {
    let mut none = DecoderScheduler::new(0, 100);
    assert_eq!(
        none.enqueue(picture(1, false, 0)),
        Err(SessionError::DecoderBackpressure)
    );
    assert_eq!(none.queued_count(), 0);
    let mut one_byte = DecoderScheduler::new(1, 1);
    assert_eq!(
        one_byte.enqueue(picture(1, true, 2)),
        Err(SessionError::PictureTooLarge)
    );
    one_byte.enqueue(picture(1, true, 1)).unwrap();
    assert_eq!(one_byte.queued_bytes(), 1);
}

#[test]
fn integer_maximum_cannot_overflow_accounting_or_trigger_eager_allocation() {
    let mut scheduler = DecoderScheduler::new(usize::MAX, usize::MAX);
    scheduler.enqueue(picture(1, true, usize::MAX)).unwrap();
    assert_eq!(
        scheduler.enqueue(picture(2, true, 1)),
        Err(SessionError::DecoderBackpressure)
    );
    assert_eq!(scheduler.queued_bytes(), usize::MAX);
    decode(&mut scheduler, 1);
    assert_eq!(scheduler.select_presentation_frame(true), Some(1));
    assert_eq!(scheduler.queued_bytes(), 0);
}

#[test]
fn diagnostics_count_only_real_decode_completion_and_once_only_presentation() {
    let mut session = ClientSession::new(ReconnectPolicy::default());
    for frame in 1..=3 {
        session
            .scheduler_mut()
            .enqueue(picture(frame, true, 10))
            .unwrap();
    }
    assert_eq!(session.diagnostics().frames_decoded, 0);
    for frame in 1..=3 {
        decode(session.scheduler_mut(), frame);
    }
    assert_eq!(
        session.scheduler_mut().select_presentation_frame(true),
        Some(3)
    );
    assert_eq!(
        session.scheduler_mut().select_presentation_frame(true),
        None
    );
    let diagnostics = session.diagnostics();
    assert_eq!(diagnostics.frames_decoded, 3);
    assert_eq!(diagnostics.frames_presented, 1);
    assert_eq!(diagnostics.frames_skipped_presentation, 2);
    assert_eq!(diagnostics.frames_dropped_decode, 0);
}

fn binding(id: u128) -> ControlBinding {
    ControlBinding {
        id: 1,
        host_boot: HostBootId::from_raw(2),
        os_session: OsSessionId::from_raw(3),
        remote_session: RemoteSessionId::from_raw(id),
    }
}

fn viewing() -> ClientSession {
    let mut session = ClientSession::new(ReconnectPolicy::default());
    session.connect(ClientInstant(1)).unwrap();
    session
        .on_session_opened(binding(4), ClientInstant(1))
        .unwrap();
    session
        .update_view_freshness(true, ClientInstant(1))
        .unwrap();
    session
}

#[test]
fn reconnect_retains_original_charges_until_close_and_rejects_old_decode_callbacks() {
    let mut session = viewing();
    session
        .scheduler_mut()
        .enqueue(picture(7, true, 100))
        .unwrap();
    session
        .scheduler_mut()
        .enqueue(picture(8, true, 50))
        .unwrap();
    let old_decode = session.scheduler_mut().next_decode().unwrap();
    session.on_disconnect(ReconnectReason::TransportDrop, ClientInstant(1));
    assert_eq!(session.scheduler().queued_count(), 2);
    assert_eq!(session.scheduler().queued_bytes(), 150);
    assert!(session.scheduler_mut().next_decode().is_none());
    assert_eq!(
        session.scheduler_mut().select_presentation_frame(true),
        None
    );
    let closed = session.scheduler_mut().take_close_request().unwrap();
    assert!(session.scheduler_mut().take_close_request().is_none());
    assert_eq!(
        session.scheduler_mut().restart_after_close(),
        Err(SessionError::InvalidState)
    );

    let now = ClientInstant(100_001);
    assert!(session.tick_reconnect(now).unwrap());
    session.on_session_opened(binding(5), now).unwrap();
    assert_eq!(
        session.scheduler_mut().enqueue(picture(7, true, 100)),
        Err(SessionError::DecoderClosed)
    );
    assert_eq!(session.scheduler().queued_bytes(), 150);
    // This is a simulated positive native-close callback, not OS evidence.
    session.scheduler_mut().on_decoder_closed(closed).unwrap();
    assert_eq!(session.scheduler().queued_count(), 0);
    assert_eq!(session.scheduler().queued_bytes(), 0);
    session.scheduler_mut().restart_after_close().unwrap();
    session
        .scheduler_mut()
        .enqueue(picture(7, true, 100))
        .unwrap();
    let current = session.scheduler_mut().next_decode().unwrap();
    assert_eq!(
        session.scheduler_mut().on_frame_decoded(old_decode),
        Err(SessionError::StaleDecode)
    );
    assert_eq!(
        session.scheduler_mut().select_presentation_frame(true),
        None
    );
    session.scheduler_mut().on_frame_decoded(current).unwrap();
    assert_eq!(
        session.scheduler_mut().select_presentation_frame(true),
        Some(7)
    );
}

#[test]
fn close_fences_presentation_but_completed_original_work_remains_charged() {
    let mut session = viewing();
    session
        .scheduler_mut()
        .enqueue(picture(7, true, 100))
        .unwrap();
    let work = session.scheduler_mut().next_decode().unwrap();
    session.close(CloseReason::UserRequested);
    session.scheduler_mut().on_frame_decoded(work).unwrap();
    assert_eq!(
        session.scheduler_mut().select_presentation_frame(true),
        None
    );
    assert_eq!(session.scheduler().queued_count(), 1);
    assert_eq!(session.scheduler().queued_bytes(), 100);
    let closed = session.scheduler_mut().take_close_request().unwrap();
    session.close(CloseReason::UserRequested);
    assert!(session.scheduler_mut().take_close_request().is_none());
    session.scheduler_mut().on_decoder_closed(closed).unwrap();
    assert_eq!(session.scheduler().queued_bytes(), 0);
}

#[test]
fn another_decoders_close_callback_cannot_release_this_owners_reservations() {
    let mut original = DecoderScheduler::new(1, 100);
    original.fence();
    let old_close = original.take_close_request().unwrap();
    let mut current = DecoderScheduler::new(1, 100);
    current.enqueue(picture(1, true, 100)).unwrap();
    current.fence();
    let current_close = current.take_close_request().unwrap();
    assert_eq!(
        current.on_decoder_closed(old_close),
        Err(SessionError::StaleDecode)
    );
    assert_eq!(current.queued_count(), 1);
    assert_eq!(current.queued_bytes(), 100);
    assert_eq!(
        current.restart_after_close(),
        Err(SessionError::InvalidState)
    );
    current.on_decoder_closed(current_close).unwrap();
    current.restart_after_close().unwrap();
    current.enqueue(picture(1, true, 100)).unwrap();
}

#[test]
fn explicit_decoder_reconfiguration_fences_pending_control_and_original_work() {
    let mut session = viewing();
    let request = session.request_control(ClientInstant(1)).unwrap();
    session
        .scheduler_mut()
        .enqueue(picture(1, true, 100))
        .unwrap();
    session.fence_decoder();
    assert_eq!(
        session.on_control_granted(
            request,
            InputLeaseId::from_raw(1),
            InputTicketId::from_raw(1)
        ),
        Err(SessionError::InvalidState)
    );
    assert!(!session.can_request_control());
    assert_eq!(session.scheduler().queued_bytes(), 100);
    assert!(session.scheduler_mut().next_decode().is_none());
    assert_eq!(
        session.scheduler_mut().enqueue(picture(2, true, 100)),
        Err(SessionError::DecoderClosed)
    );
}

#[test]
fn focus_and_readiness_fences_leave_observation_decoder_ownership_independent() {
    for event in 0..3 {
        let mut session = viewing();
        let request = session.request_control(ClientInstant(1)).unwrap();
        session
            .on_control_granted(
                request,
                InputLeaseId::from_raw(1),
                InputTicketId::from_raw(1),
            )
            .unwrap();
        session
            .scheduler_mut()
            .enqueue(picture(1, true, 100))
            .unwrap();
        let original = session.scheduler_mut().next_decode().unwrap();
        match event {
            0 => session.on_focus_loss(),
            1 => session.set_window_visible(false),
            _ => session
                .update_view_freshness(false, ClientInstant(1))
                .unwrap(),
        }
        assert!(!session.diagnostics().is_controlling);
        session.scheduler_mut().on_frame_decoded(original).unwrap();
        session
            .scheduler_mut()
            .enqueue(picture(2, true, 100))
            .unwrap();
        assert!(session.scheduler_mut().next_decode().is_some());
        assert!(session.scheduler_mut().take_close_request().is_none());
    }
}
