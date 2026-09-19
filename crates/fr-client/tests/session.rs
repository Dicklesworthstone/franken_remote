//! Integration and acceptance tests for `fr-client` session core:
//! - Auto-reconnect with bounded backoff and user-visible reconnecting state
//! - Reconnect-without-lease-resurrection (control re-request only after view freshness is re-established)
//! - Stale-view input suspension while read-only diagnostics continue
//! - Mapping-generation rejection of stale coordinates
//! - Decoder scheduling & presentation policy (newest ready frame wins, obsolete work discarded)
//! - Window occlusion suspension

use fr_client::input::ClientInstant;
use fr_client::session::{
    ClientSession, CloseReason, QueuedPicture, ReconnectPolicy, ReconnectReason, SessionError,
    SessionState, SuspendReason,
};
use fr_core::ids::{
    DisplayGeometryGeneration, HostBootId, InputLeaseId, InputTicketId, OsSessionId,
    RemoteSessionId, ViewportMappingGeneration,
};
use fr_wire::negotiation::ControlBinding;

fn sample_binding(session_raw: u128) -> ControlBinding {
    ControlBinding {
        id: 1,
        host_boot: HostBootId::from_raw(100),
        os_session: OsSessionId::from_raw(200),
        remote_session: RemoteSessionId::from_raw(session_raw),
    }
}

#[test]
fn auto_reconnect_with_bounded_backoff_and_user_visible_reconnecting_state() {
    let policy = ReconnectPolicy {
        initial_backoff_us: 100_000,
        max_backoff_us: 1_000_000,
        backoff_factor: 2,
        max_attempts: Some(4),
    };
    let mut session = ClientSession::new(policy);
    let mut now = ClientInstant(10_000_000);

    // Initial state
    assert_eq!(session.state(), &SessionState::Disconnected);
    assert_eq!(session.state().display_label(), "Disconnected");

    // Initiate connection
    session.connect(now).unwrap();
    assert!(matches!(
        session.state(),
        SessionState::Connecting { attempt: 1, .. }
    ));
    assert_eq!(session.state().display_label(), "Connecting");

    // Attempt 1 fails: enters user-visible Reconnecting state with initial backoff
    now = ClientInstant(now.0 + 50_000);
    session.on_disconnect(ReconnectReason::TransportDrop, now);

    assert!(matches!(
        session.state(),
        SessionState::Reconnecting {
            attempt: 1,
            backoff_us: 100_000,
            ..
        }
    ));
    assert_eq!(session.state().display_label(), "Reconnecting");

    // Ticking before backoff expires does not advance
    let advanced = session
        .tick_reconnect(ClientInstant(now.0 + 50_000))
        .unwrap();
    assert!(!advanced);
    assert_eq!(session.state().display_label(), "Reconnecting");

    // Ticking at/after expiry advances to Connecting
    now = ClientInstant(now.0 + 100_000);
    let advanced = session.tick_reconnect(now).unwrap();
    assert!(advanced);
    assert!(matches!(
        session.state(),
        SessionState::Connecting { attempt: 1, .. }
    ));

    // Attempt 2 fails: backoff doubles to 200_000 us
    session.on_disconnect(ReconnectReason::TransportDrop, now);
    assert!(matches!(
        session.state(),
        SessionState::Reconnecting {
            attempt: 2,
            backoff_us: 200_000,
            ..
        }
    ));

    // Attempt 3 fails: backoff doubles to 400_000 us
    now = ClientInstant(now.0 + 200_000);
    session.tick_reconnect(now).unwrap();
    session.on_disconnect(ReconnectReason::TransportDrop, now);
    assert!(matches!(
        session.state(),
        SessionState::Reconnecting {
            attempt: 3,
            backoff_us: 400_000,
            ..
        }
    ));

    // Attempt 4 fails: backoff would be 800_000 us
    now = ClientInstant(now.0 + 400_000);
    session.tick_reconnect(now).unwrap();
    session.on_disconnect(ReconnectReason::TransportDrop, now);
    assert!(matches!(
        session.state(),
        SessionState::Reconnecting {
            attempt: 4,
            backoff_us: 800_000,
            ..
        }
    ));

    // Attempt 5 exceeds max_attempts (4): transitions to Closed
    now = ClientInstant(now.0 + 800_000);
    session.tick_reconnect(now).unwrap();
    session.on_disconnect(ReconnectReason::TransportDrop, now);
    assert!(matches!(
        session.state(),
        SessionState::Closed {
            reason: CloseReason::MaxReconnectAttemptsExceeded
        }
    ));
    assert_eq!(session.state().display_label(), "Closed");
}

#[test]
fn reconnect_never_silently_restores_revoked_input_lease() {
    let mut session = ClientSession::new(ReconnectPolicy::default());
    let now = ClientInstant(10_000_000);

    // Connect and establish session
    session.connect(now).unwrap();
    let binding = sample_binding(42);
    session.on_session_opened(binding, now).unwrap();

    assert!(matches!(
        session.state(),
        SessionState::Viewing {
            view_fresh: false,
            ..
        }
    ));

    // Control CANNOT be requested before view freshness is proven!
    assert!(!session.can_request_control());
    assert_eq!(
        session.request_control(now),
        Err(SessionError::ViewNotFresh)
    );

    // Frame arrived and presented: view is now fresh
    session.update_view_freshness(true, now).unwrap();
    assert!(session.can_request_control());

    // Request control
    let seq = session.request_control(now).unwrap();
    assert_eq!(seq, 1);
    assert!(matches!(
        session.state(),
        SessionState::RequestingControl { .. }
    ));

    // Host grants control: lease 500, ticket 600
    let lease_1 = InputLeaseId::from_raw(500);
    let ticket_1 = InputTicketId::from_raw(600);
    session.on_control_granted(lease_1, ticket_1).unwrap();
    assert!(matches!(
        session.state(),
        SessionState::Controlling {
            lease,
            ticket,
            input_ready: true,
            ..
        } if *lease == lease_1 && *ticket == ticket_1
    ));
    assert_eq!(session.diagnostics().current_lease, Some(500));

    // Transport disconnects: LEASE MUST BE TERMINATED IMMEDIATELY
    session.on_disconnect(ReconnectReason::TransportDrop, now);
    assert!(session.diagnostics().current_lease.is_none());
    assert!(matches!(session.state(), SessionState::Reconnecting { .. }));

    // Fast-forward reconnect timer
    let reconnect_time = ClientInstant(now.0 + 100_000);
    session.tick_reconnect(reconnect_time).unwrap();
    assert!(matches!(session.state(), SessionState::Connecting { .. }));

    // Reconnection succeeds: opens session again
    let new_binding = sample_binding(43);
    session
        .on_session_opened(new_binding, reconnect_time)
        .unwrap();

    // INVARIANT: State is Viewing, NOT Controlling! Old lease was NOT resurrected!
    assert!(matches!(
        session.state(),
        SessionState::Viewing {
            view_fresh: false,
            ..
        }
    ));
    assert_eq!(session.diagnostics().current_lease, None);
    assert!(!session.diagnostics().is_controlling);

    // Control cannot be requested until a fresh frame establishes view freshness
    assert!(!session.can_request_control());
    assert_eq!(
        session.request_control(reconnect_time),
        Err(SessionError::ViewNotFresh)
    );

    // New IDR presented: view freshness re-established on resumed session
    session.update_view_freshness(true, reconnect_time).unwrap();
    assert!(session.can_request_control());

    // Now control can be requested freshly
    let seq_2 = session.request_control(reconnect_time).unwrap();
    assert_eq!(seq_2, 2);

    // Host issues a brand new lease: lease 501, ticket 601
    let lease_2 = InputLeaseId::from_raw(501);
    let ticket_2 = InputTicketId::from_raw(601);
    session.on_control_granted(lease_2, ticket_2).unwrap();
    assert_eq!(session.diagnostics().current_lease, Some(501));
    assert!(session.diagnostics().is_controlling);
}

#[test]
fn stale_view_suspends_input_readiness_while_diagnostics_continue() {
    let mut session = ClientSession::new(ReconnectPolicy::default());
    let now = ClientInstant(10_000_000);

    session.connect(now).unwrap();
    session.on_session_opened(sample_binding(1), now).unwrap();
    session.update_view_freshness(true, now).unwrap();
    session.request_control(now).unwrap();
    session
        .on_control_granted(InputLeaseId::from_raw(10), InputTicketId::from_raw(20))
        .unwrap();

    assert!(session.diagnostics().is_controlling);
    assert!(!session.diagnostics().is_suspended);

    // Coordinate check passes while fresh
    let disp = DisplayGeometryGeneration::INITIAL;
    let view = ViewportMappingGeneration::INITIAL;
    assert!(session.validate_coordinate_mapping(disp, view).is_ok());

    // Source verification fails / capture stalls: view becomes stale!
    session.update_view_freshness(false, now).unwrap();

    // Input MUST be suspended!
    assert!(matches!(
        session.state(),
        SessionState::Suspended {
            reason: SuspendReason::StaleView,
            ..
        }
    ));
    assert_eq!(session.state().display_label(), "Input Suspended");

    // Coordinate submission is rejected due to suspension
    assert_eq!(
        session.validate_coordinate_mapping(disp, view),
        Err(SessionError::InputSuspended)
    );

    // Read-only diagnostics continue reporting accurate metrics
    let diag = session.diagnostics();
    assert!(!diag.view_fresh);
    assert!(diag.is_suspended);
    assert!(!diag.is_controlling);
    assert_eq!(diag.stale_view_suspensions, 1);

    // View freshness restored: input resumes
    session.update_view_freshness(true, now).unwrap();
    assert!(matches!(session.state(), SessionState::Controlling { .. }));
    assert!(session.validate_coordinate_mapping(disp, view).is_ok());
}

#[test]
fn mapping_generation_rejection_of_stale_coordinates() {
    let mut session = ClientSession::new(ReconnectPolicy::default());
    let now = ClientInstant(10_000_000);

    session.connect(now).unwrap();
    session.on_session_opened(sample_binding(1), now).unwrap();
    session.update_view_freshness(true, now).unwrap();
    session.request_control(now).unwrap();
    session
        .on_control_granted(InputLeaseId::from_raw(10), InputTicketId::from_raw(20))
        .unwrap();

    let disp_0 = DisplayGeometryGeneration::INITIAL;
    let view_0 = ViewportMappingGeneration::INITIAL;
    assert!(session.validate_coordinate_mapping(disp_0, view_0).is_ok());

    // Host updates display crop / geometry to generation 2
    let disp_1 = DisplayGeometryGeneration::from_raw(2);
    let view_1 = ViewportMappingGeneration::from_raw(2);
    session.update_mapping(disp_1, view_1);

    // Old coordinates using generation INITIAL MUST be rejected!
    assert_eq!(
        session.validate_coordinate_mapping(disp_0, view_0),
        Err(SessionError::StaleMapping)
    );
    assert_eq!(session.diagnostics().mapping_rejections, 1);

    // Coordinates using the updated generation pass
    assert!(session.validate_coordinate_mapping(disp_1, view_1).is_ok());
}

#[test]
fn decoder_scheduler_newest_ready_frame_wins_and_discards_obsolete_presentation() {
    let mut scheduler = fr_client::session::DecoderScheduler::new(4, 10_000);

    // Enqueue 3 frames
    scheduler
        .enqueue(QueuedPicture {
            frame_number: 10,
            is_reference: true,
            byte_size: 1000,
            captured_at_us: 1000,
        })
        .unwrap();

    scheduler
        .enqueue(QueuedPicture {
            frame_number: 11,
            is_reference: false,
            byte_size: 1000,
            captured_at_us: 2000,
        })
        .unwrap();

    scheduler
        .enqueue(QueuedPicture {
            frame_number: 12,
            is_reference: false,
            byte_size: 1000,
            captured_at_us: 3000,
        })
        .unwrap();

    assert_eq!(scheduler.queued_count(), 3);
    assert_eq!(scheduler.queued_bytes(), 3000);

    // Present frames: Newest ready frame (12) wins!
    let winning = scheduler.select_presentation_frame(true);
    assert_eq!(winning, Some(12));

    // Obsolete presentation work discarded: frames 10 and 11 presentation skipped
    // Decoded vs presented progress reported separately
    scheduler.on_frame_decoded();
    scheduler.on_frame_decoded();
    scheduler.on_frame_decoded();

    assert_eq!(scheduler.queued_count(), 2);
    assert_eq!(scheduler.queued_bytes(), 2000);
}

#[test]
fn window_occlusion_does_not_present_and_suspends_input() {
    let mut session = ClientSession::new(ReconnectPolicy::default());
    let now = ClientInstant(10_000_000);

    session.connect(now).unwrap();
    session.on_session_opened(sample_binding(1), now).unwrap();
    session.update_view_freshness(true, now).unwrap();
    session.request_control(now).unwrap();
    session
        .on_control_granted(InputLeaseId::from_raw(10), InputTicketId::from_raw(20))
        .unwrap();

    assert!(matches!(session.state(), SessionState::Controlling { .. }));

    // Window is minimized / occluded by user
    session.set_window_visible(false);

    // Input is suspended due to occlusion
    assert!(matches!(
        session.state(),
        SessionState::Suspended {
            reason: SuspendReason::WindowHidden,
            ..
        }
    ));
    assert_eq!(
        session.validate_coordinate_mapping(
            DisplayGeometryGeneration::INITIAL,
            ViewportMappingGeneration::INITIAL
        ),
        Err(SessionError::InputSuspended)
    );

    // Enqueued frame is not presented while occluded
    session
        .scheduler_mut()
        .enqueue(QueuedPicture {
            frame_number: 1,
            is_reference: false,
            byte_size: 100,
            captured_at_us: 1000,
        })
        .unwrap();

    assert_eq!(
        session.scheduler_mut().select_presentation_frame(false),
        None
    );
}
