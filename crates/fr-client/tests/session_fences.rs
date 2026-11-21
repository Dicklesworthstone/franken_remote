//! The shared session projection cannot create control from local lifecycle
//! events or apply a retired request's grant to a replacement request.
use fr_client::{
    ClientSession, ReconnectPolicy, ReconnectReason, SessionError, SessionState,
    input::ClientInstant,
};
use fr_core::{
    ids::{
        DisplayGeometryGeneration, HostBootId, InputLeaseId, InputTicketId, OsSessionId,
        RemoteSessionId, ViewportMappingGeneration,
    },
    limits::ProtocolLimits,
};
use fr_wire::{
    display::{Catalog, Display},
    negotiation::ControlBinding,
};

const NOW: ClientInstant = ClientInstant(1_000);

fn viewing() -> ClientSession {
    let mut session = ClientSession::new(ReconnectPolicy::default());
    session.connect(NOW).unwrap();
    session
        .on_session_opened(
            ControlBinding {
                id: 1,
                host_boot: HostBootId::from_raw(2),
                os_session: OsSessionId::from_raw(3),
                remote_session: RemoteSessionId::from_raw(4),
            },
            NOW,
        )
        .unwrap();
    session.update_view_freshness(true, NOW).unwrap();
    session
}

fn grant(session: &mut ClientSession, request: u64, id: u128) -> Result<(), SessionError> {
    session.on_control_granted(
        request,
        InputLeaseId::from_raw(id),
        InputTicketId::from_raw(id + 1),
    )
}

#[derive(Clone, Copy, Debug)]
enum Fence {
    Focus,
    Hidden,
    Stale,
    Mapping,
    Recovery,
}

impl Fence {
    const ALL: [Self; 5] = [
        Self::Focus,
        Self::Hidden,
        Self::Stale,
        Self::Mapping,
        Self::Recovery,
    ];

    fn apply(self, session: &mut ClientSession) {
        match self {
            Self::Focus => session.on_focus_loss(),
            Self::Hidden => session.set_window_visible(false),
            Self::Stale => session.update_view_freshness(false, NOW).unwrap(),
            Self::Mapping => session.update_mapping(
                DisplayGeometryGeneration::from_raw(2),
                ViewportMappingGeneration::from_raw(2),
            ),
            Self::Recovery => session.invalidate_control(),
        }
    }

    fn restore_local(self, session: &mut ClientSession) {
        match self {
            Self::Focus => assert!(!session.on_focus_gained(NOW).unwrap()),
            Self::Hidden => session.set_window_visible(true),
            Self::Stale | Self::Mapping | Self::Recovery => {}
        }
    }
}

#[test]
fn every_lifecycle_fence_retires_pending_control_before_a_late_grant() {
    for fence in Fence::ALL {
        let mut session = viewing();
        let original = session.request_control(NOW).unwrap();
        fence.apply(&mut session);
        assert_eq!(
            grant(&mut session, original, 10),
            Err(SessionError::InvalidState),
            "{fence:?}"
        );
        assert_eq!(session.diagnostics().current_lease, None);
        assert!(!session.can_request_control());

        fence.restore_local(&mut session);
        assert!(
            !session.can_request_control(),
            "local focus/visibility is not presentation"
        );
        session.update_view_freshness(true, NOW).unwrap();
        let replacement = session.request_control(NOW).unwrap();
        assert!(replacement > original);
        assert_eq!(
            grant(&mut session, original, 10),
            Err(SessionError::InvalidState),
            "{fence:?}"
        );
        assert!(
            matches!(session.state(), SessionState::RequestingControl { request_sequence, .. } if *request_sequence == replacement)
        );
        grant(&mut session, replacement, 20).unwrap();
        assert_eq!(session.diagnostics().current_lease, Some(20));
    }
}

#[test]
fn every_lifecycle_fence_requires_fresh_evidence_and_a_new_explicit_grant() {
    for fence in Fence::ALL {
        let mut session = viewing();
        let original = session.request_control(NOW).unwrap();
        grant(&mut session, original, 10).unwrap();
        fence.apply(&mut session);
        assert_eq!(session.diagnostics().current_lease, None, "{fence:?}");
        assert!(!session.diagnostics().view_fresh);
        fence.restore_local(&mut session);
        assert!(!session.diagnostics().is_controlling);
        session.update_view_freshness(true, NOW).unwrap();
        assert!(matches!(
            session.state(),
            SessionState::Viewing {
                view_fresh: true,
                ..
            }
        ));
        assert_eq!(
            grant(&mut session, original, 10),
            Err(SessionError::InvalidState)
        );
        let replacement = session.request_control(NOW).unwrap();
        grant(&mut session, replacement, 20).unwrap();
        assert!(session.diagnostics().is_controlling);
    }
}

#[test]
fn hidden_or_unfocused_callbacks_cannot_restore_freshness_or_start_control() {
    for fence in [Fence::Focus, Fence::Hidden] {
        let mut session = viewing();
        fence.apply(&mut session);
        assert_eq!(
            session.update_view_freshness(true, NOW),
            Err(SessionError::InputSuspended)
        );
        assert_eq!(
            session.request_control(NOW),
            Err(SessionError::ViewNotFresh)
        );
        assert!(!session.diagnostics().view_fresh);
        // The other positive lifecycle event cannot bypass the original loss.
        match fence {
            Fence::Focus => session.set_window_visible(true),
            Fence::Hidden => assert!(!session.on_focus_gained(NOW).unwrap()),
            _ => unreachable!(),
        }
        assert_eq!(
            session.update_view_freshness(true, NOW),
            Err(SessionError::InputSuspended)
        );
        fence.restore_local(&mut session);
        session.update_view_freshness(true, NOW).unwrap();
        assert!(session.can_request_control());
    }
}

#[test]
fn unchanged_mapping_preserves_control_but_changed_mapping_needs_a_new_grant() {
    let mut session = viewing();
    let request = session.request_control(NOW).unwrap();
    grant(&mut session, request, 10).unwrap();
    session.update_mapping(
        DisplayGeometryGeneration::INITIAL,
        ViewportMappingGeneration::INITIAL,
    );
    assert_eq!(session.diagnostics().current_lease, Some(10));

    let geometry = DisplayGeometryGeneration::from_raw(2);
    let viewport = ViewportMappingGeneration::from_raw(3);
    session.update_mapping(geometry, viewport);
    assert_eq!(session.diagnostics().current_lease, None);
    assert_eq!(
        session.validate_coordinate_mapping(geometry, viewport),
        Err(SessionError::InvalidState)
    );
    assert_eq!(
        session.validate_coordinate_mapping(
            DisplayGeometryGeneration::INITIAL,
            ViewportMappingGeneration::INITIAL
        ),
        Err(SessionError::StaleMapping)
    );
    session.update_view_freshness(true, NOW).unwrap();
    let request = session.request_control(NOW).unwrap();
    grant(&mut session, request, 20).unwrap();
    assert_eq!(
        session.validate_coordinate_mapping(geometry, viewport),
        Ok(())
    );
}

fn display(handle: u128, generation: u64, x: i32) -> Display {
    Display {
        handle,
        geometry: DisplayGeometryGeneration::from_raw(generation),
        x,
        y: 0,
        pixel_width: 640,
        pixel_height: 480,
        logical_width: 640,
        logical_height: 480,
        scale_numerator: 1,
        scale_denominator: 1,
        rotation: 0,
    }
}

fn catalog(revision: u64, displays: &[Display]) -> Catalog {
    Catalog::new(revision, displays, &ProtocolLimits::ABSOLUTE).unwrap()
}

#[test]
fn explicit_target_change_retires_a_pending_or_active_grant() {
    for active in [false, true] {
        let mut session = viewing();
        session
            .on_catalog_received(catalog(1, &[display(10, 1, 0), display(20, 2, 640)]))
            .unwrap();
        session.select_display(10).unwrap();
        session.update_view_freshness(true, NOW).unwrap();
        let original = session.request_control(NOW).unwrap();
        if active {
            grant(&mut session, original, 30).unwrap();
        }
        session.set_target_display(20).unwrap();
        assert_eq!(session.target_display(), Some(20));
        assert_eq!(session.diagnostics().current_lease, None);
        assert_eq!(
            grant(&mut session, original, 30),
            Err(SessionError::InvalidState)
        );
        session.update_view_freshness(true, NOW).unwrap();
        let current = session.request_control(NOW).unwrap();
        grant(&mut session, current, 40).unwrap();
        assert_eq!(
            session.validate_coordinate_mapping(
                DisplayGeometryGeneration::from_raw(2),
                ViewportMappingGeneration::INITIAL
            ),
            Ok(())
        );
    }
}

#[test]
fn hot_unplug_never_retargets_control_to_another_selected_display() {
    let mut session = viewing();
    session
        .on_catalog_received(catalog(1, &[display(10, 1, 0), display(20, 1, 640)]))
        .unwrap();
    session.select_display(10).unwrap();
    session.select_display(20).unwrap();
    session.update_view_freshness(true, NOW).unwrap();
    let request = session.request_control(NOW).unwrap();
    grant(&mut session, request, 30).unwrap();
    session
        .on_catalog_received(catalog(2, &[display(20, 2, 640)]))
        .unwrap();
    assert_eq!(session.target_display(), None);
    assert_eq!(session.selected_displays(), &[20]);
    assert_eq!(session.diagnostics().current_lease, None);
    assert!(!session.can_request_control());
    session.set_target_display(20).unwrap();
    session.update_view_freshness(true, NOW).unwrap();
    let current = session.request_control(NOW).unwrap();
    grant(&mut session, current, 40).unwrap();
    assert_eq!(session.diagnostics().current_lease, Some(40));
}

#[test]
fn unselecting_the_target_retires_control_without_an_implicit_replacement() {
    let mut session = viewing();
    session
        .on_catalog_received(catalog(1, &[display(10, 1, 0), display(20, 1, 640)]))
        .unwrap();
    session.select_display(10).unwrap();
    session.select_display(20).unwrap();
    session.update_view_freshness(true, NOW).unwrap();
    let request = session.request_control(NOW).unwrap();
    grant(&mut session, request, 30).unwrap();
    session.unselect_display(10);
    assert_eq!(session.target_display(), None);
    assert_eq!(session.selected_displays(), &[20]);
    assert_eq!(session.diagnostics().current_lease, None);
}

#[test]
fn original_catalog_is_preserved_on_regressed_or_contradictory_revision() {
    let mut session = viewing();
    let original = catalog(2, &[display(10, 2, 0)]);
    session.on_catalog_received(original).unwrap();
    session.select_display(10).unwrap();
    session.update_view_freshness(true, NOW).unwrap();
    let request = session.request_control(NOW).unwrap();
    grant(&mut session, request, 30).unwrap();
    session.on_catalog_received(original).unwrap();
    assert_eq!(session.diagnostics().current_lease, Some(30));
    for stale in [
        catalog(1, &[display(10, 1, 0)]),
        catalog(2, &[display(10, 2, 10)]),
    ] {
        assert_eq!(
            session.on_catalog_received(stale),
            Err(SessionError::StaleMapping)
        );
        assert_eq!(session.current_catalog(), Some(&original));
        assert_eq!(session.target_display(), Some(10));
    }
    session
        .on_catalog_received(catalog(3, &[display(10, 3, 10)]))
        .unwrap();
    assert_eq!(session.diagnostics().current_lease, None);
    assert!(!session.can_request_control());
}

#[test]
fn reconnect_does_not_reuse_catalog_target_or_request_identity() {
    let mut session = viewing();
    session
        .on_catalog_received(catalog(1, &[display(10, 1, 0)]))
        .unwrap();
    session.select_display(10).unwrap();
    session.update_view_freshness(true, NOW).unwrap();
    let original = session.request_control(NOW).unwrap();
    session.on_disconnect(ReconnectReason::TransportDrop, NOW);
    let later = ClientInstant(101_000);
    assert!(session.tick_reconnect(later).unwrap());
    session
        .on_session_opened(
            ControlBinding {
                id: 1,
                host_boot: HostBootId::from_raw(2),
                os_session: OsSessionId::from_raw(3),
                remote_session: RemoteSessionId::from_raw(5),
            },
            later,
        )
        .unwrap();
    assert!(session.current_catalog().is_none());
    assert_eq!(session.selected_displays(), []);
    assert_eq!(session.target_display(), None);
    session.update_view_freshness(true, later).unwrap();
    let replacement = session.request_control(later).unwrap();
    assert!(replacement > original);
    assert_eq!(
        grant(&mut session, original, 10),
        Err(SessionError::InvalidState)
    );
    grant(&mut session, replacement, 20).unwrap();
}

#[test]
fn zero_grant_ids_retire_the_request_without_claiming_control() {
    let mut session = viewing();
    let request = session.request_control(NOW).unwrap();
    assert_eq!(
        session.on_control_granted(
            request,
            InputLeaseId::from_raw(0),
            InputTicketId::from_raw(2)
        ),
        Err(SessionError::LeaseRevoked)
    );
    assert_eq!(session.diagnostics().current_lease, None);
    assert_eq!(
        grant(&mut session, request, 10),
        Err(SessionError::InvalidState)
    );
}
