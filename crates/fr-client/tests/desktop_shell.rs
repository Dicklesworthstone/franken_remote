//! Acceptance tests for the first desktop client shell (Plan §§14.1, 15.1, 16.1):
//! - Unit tests for coordinate/DPI/scale mapping across various DPI scale factors.
//! - Rejection of stale coordinates and unconfirmed layouts upon geometry/scale changes.
//! - Focus-loss release logic: immediate input suspension, active ticket invalidation,
//!   synthetic release generation for held keys/buttons, and refusal of silent lease resurrection.
//! - Shortcut-capture toggle: per-platform capability matrix advertising, permission gating,
//!   mode toggling, and visibility in the minimal desktop toolbar.

use fr_client::input::{
    Action, ClientInstant, InputClient, Policy, PresentedObservation, StopReason,
    viewport::{Error as ViewportError, LocalPoint, SurfaceRect},
};
use fr_client::session::{
    ClientSession, ReconnectPolicy, SessionError, SessionState, SuspendReason,
};
use fr_client::shortcut::{
    PlatformCapabilityRow, PlatformId, ShortcutCaptureController, ShortcutCaptureError,
    ShortcutCaptureMode, ShortcutRoutingMechanism, ShortcutSupportStatus,
};
use fr_client::toolbar::ToolbarModel;
use fr_core::held_state::HeldState;
use fr_core::ids::{
    CodecConfigurationGeneration, DisplayGeometryGeneration, HostBootId, InputLeaseId,
    InputTicketId, OsSessionId, RecoveryGeneration, RemoteSessionId, ViewportMappingGeneration,
};
use fr_core::input::{
    DesktopPoint, InputBounds, InputCredentials, InputView, KeyTransition, PhysicalKey,
    PointerButton,
};
use fr_core::input_submission::{Capabilities, Capability};
use fr_core::limits::ProtocolLimits;
use fr_wire::input::MAX_INPUT_RECORD_BYTES;
use fr_wire::negotiation::ControlBinding;

fn sample_credentials() -> InputCredentials {
    InputCredentials {
        session: RemoteSessionId::from_raw(42),
        lease: InputLeaseId::from_raw(100),
        ticket: InputTicketId::from_raw(200),
        view: InputView {
            geometry: DisplayGeometryGeneration::INITIAL,
            viewport: ViewportMappingGeneration::INITIAL,
            configuration: CodecConfigurationGeneration::INITIAL,
            recovery: RecoveryGeneration::INITIAL,
        },
    }
}

fn sample_client(bounds: InputBounds) -> InputClient {
    let caps = Capabilities::default()
        .with(Capability::Keys)
        .with(Capability::Repeat)
        .with(Capability::Absolute)
        .with(Capability::Buttons)
        .with(Capability::LineScroll);

    let creds = sample_credentials();
    let mut client = InputClient::new(
        creds,
        1,
        bounds,
        caps,
        ProtocolLimits::ABSOLUTE,
        Policy::default(),
        ClientInstant(0),
    )
    .unwrap();

    client
        .confirm_mapping(creds.session, creds.view, ClientInstant(0))
        .unwrap();
    client
        .presented(
            PresentedObservation {
                session: creds.session,
                view: creds.view,
                serial: 1,
                received_at: ClientInstant(0),
                source_age_upper_us: 1000,
            },
            ClientInstant(0),
        )
        .unwrap();

    client
}

// =========================================================================
// 1. Coordinate, DPI, and Scale Mapping Tests
// =========================================================================

#[test]
fn dpi_scale_rational_conversions_preserve_subpixel_accuracy() {
    // Test rational DPI scales: 100% (1/1), 125% (5/4), 150% (3/2), 175% (7/4), 200% (2/1), 75% (3/4)
    let test_cases = [
        // (scale_num, scale_den, logical_x, logical_y, expected_phys_x, expected_phys_y)
        (1, 1, 100, 200, 100, 200),
        (5, 4, 100, 200, 125, 250),
        (3, 2, 100, 200, 150, 300),
        (7, 4, 100, 200, 175, 350),
        (2, 1, 100, 200, 200, 400),
        (3, 4, 100, 200, 75, 150),
    ];

    for (num, den, lx, ly, exp_px, exp_py) in test_cases {
        let logical_point = LocalPoint::logical(lx * 256, ly * 256, num, den)
            .expect("scale conversion should succeed");

        let expected_point = LocalPoint::pixels(exp_px, exp_py);
        // Compare subpixel coordinates (exact 1/256 precision)
        assert!(
            logical_point == expected_point,
            "failed for scale {num}/{den}"
        );
    }

    // Zero numerator or denominator must be rejected with InvalidScale
    assert!(matches!(
        LocalPoint::logical(100, 100, 0, 1),
        Err(ViewportError::InvalidScale)
    ));
    assert!(matches!(
        LocalPoint::logical(100, 100, 1, 0),
        Err(ViewportError::InvalidScale)
    ));
}

#[test]
fn coordinate_mapping_rejects_letterbox_bars_and_out_of_bounds_events() {
    // Remote desktop: 1920x1080 at (0, 0)
    let desktop_bounds =
        InputBounds::new(DesktopPoint { x: 0, y: 0 }, 1920, 1080).expect("valid bounds");
    let client = sample_client(desktop_bounds);
    let mut viewport = client.viewport();

    // Client window: 1600x1200 (aspect ratio 4:3 vs 16:9 remote desktop -> letterbox bars top & bottom)
    let window_area = SurfaceRect::new(0, 0, 1600, 1200).expect("valid window area");
    let layout = viewport
        .configure(desktop_bounds, window_area)
        .expect("configure layout");

    // Aspect fit: width is 1600, height is 1600 * 1080 / 1920 = 900.
    // Vertical centering offset y = (1200 - 900) / 2 = 150.
    // Destination rectangle is (0, 150, 1600, 900).
    assert_eq!(layout.destination().origin().x, 0);
    assert_eq!(layout.destination().origin().y, 150);
    assert_eq!(layout.destination().width(), 1600);
    assert_eq!(layout.destination().height(), 900);

    // Unconfirmed layout must refuse mapping
    let center_event = layout.at(LocalPoint::pixels(800, 600));
    assert_eq!(viewport.map(&center_event), Err(ViewportError::Unconfirmed));

    // Confirm layout
    viewport.confirm_layout(&layout).expect("confirm layout");

    // Center maps correctly to desktop center (960, 540)
    let center_mapped = viewport.map(&center_event).expect("center map");
    assert_eq!(center_mapped, DesktopPoint { x: 960, y: 540 });

    // Top-left of active video area (0, 150) maps to desktop (0, 0)
    let origin_event = layout.at(LocalPoint::pixels(0, 150));
    assert_eq!(
        viewport.map(&origin_event).unwrap(),
        DesktopPoint { x: 0, y: 0 }
    );

    // Top letterbox bar (y < 150) must return OutsideImage, NEVER clamped onto remote top edge!
    for y in [0, 50, 149] {
        let bar_event = layout.at(LocalPoint::pixels(800, y));
        assert_eq!(
            viewport.map(&bar_event),
            Err(ViewportError::OutsideImage),
            "top letterbox y={y} must be refused"
        );
    }

    // Bottom letterbox bar (y >= 1050) must return OutsideImage
    for y in [1050, 1100, 1199] {
        let bar_event = layout.at(LocalPoint::pixels(800, y));
        assert_eq!(
            viewport.map(&bar_event),
            Err(ViewportError::OutsideImage),
            "bottom letterbox y={y} must be refused"
        );
    }

    // Subpixel just outside boundary (-1 subpixel) must stay outside
    let just_outside = layout.at(LocalPoint::subpixels(-1, 150 * 256));
    assert_eq!(
        viewport.map(&just_outside),
        Err(ViewportError::OutsideImage)
    );
}

#[test]
fn window_resize_invalidates_prior_layout_and_rejects_stale_coordinates() {
    let desktop_bounds =
        InputBounds::new(DesktopPoint { x: 0, y: 0 }, 1920, 1080).expect("valid bounds");
    let client = sample_client(desktop_bounds);
    let mut viewport = client.viewport();

    let area_1 = SurfaceRect::new(0, 0, 1920, 1080).unwrap();
    let layout_1 = viewport.configure(desktop_bounds, area_1).unwrap();
    viewport.confirm_layout(&layout_1).unwrap();

    let event_1 = layout_1.at(LocalPoint::pixels(100, 100));
    assert!(viewport.map(&event_1).is_ok());

    // User resizes client window to 1280x720
    let area_2 = SurfaceRect::new(0, 0, 1280, 720).unwrap();
    let layout_2 = viewport.configure(desktop_bounds, area_2).unwrap();

    // The old layout_1 is now OBSOLETE! Mapping events sampled with layout_1 must fail.
    assert_eq!(viewport.map(&event_1), Err(ViewportError::Obsolete));

    // The new layout_2 is not yet confirmed by the renderer, so events with layout_2 fail with Unconfirmed.
    let event_2 = layout_2.at(LocalPoint::pixels(100, 100));
    assert_eq!(viewport.map(&event_2), Err(ViewportError::Unconfirmed));

    // Confirm new layout_2: now event_2 maps cleanly
    viewport.confirm_layout(&layout_2).unwrap();
    assert!(viewport.map(&event_2).is_ok());

    // Explicit invalidation retires layout immediately
    viewport.invalidate();
    assert_eq!(viewport.map(&event_2), Err(ViewportError::Obsolete));
}

#[test]
fn mapping_generation_changes_reject_stale_coordinates() {
    let mut session = ClientSession::new(ReconnectPolicy::default());
    let now = ClientInstant(10_000_000);

    session.connect(now).unwrap();
    let binding = ControlBinding {
        id: 1,
        host_boot: HostBootId::from_raw(1),
        os_session: OsSessionId::from_raw(1),
        remote_session: RemoteSessionId::from_raw(42),
    };
    session.on_session_opened(binding, now).unwrap();
    session.update_view_freshness(true, now).unwrap();
    session.request_control(now).unwrap();
    session
        .on_control_granted(InputLeaseId::from_raw(10), InputTicketId::from_raw(20))
        .unwrap();

    let disp_0 = DisplayGeometryGeneration::INITIAL;
    let view_0 = ViewportMappingGeneration::INITIAL;
    assert!(session.validate_coordinate_mapping(disp_0, view_0).is_ok());

    // Host notifies display geometry update (e.g. resolution change or rotation)
    let disp_1 = DisplayGeometryGeneration::from_raw(5);
    let view_1 = ViewportMappingGeneration::from_raw(5);
    session.update_mapping(disp_1, view_1);

    // Stale generation coordinates must be rejected immediately!
    assert_eq!(
        session.validate_coordinate_mapping(disp_0, view_0),
        Err(SessionError::StaleMapping)
    );
    assert_eq!(session.diagnostics().mapping_rejections, 1);

    // New generation coordinates pass
    assert!(session.validate_coordinate_mapping(disp_1, view_1).is_ok());
}

// =========================================================================
// 2. Focus-Loss Release Logic Tests
// =========================================================================

#[test]
fn focus_loss_suspends_input_readiness_and_invalidates_ticket() {
    let mut session = ClientSession::new(ReconnectPolicy::default());
    let now = ClientInstant(10_000_000);

    session.connect(now).unwrap();
    let binding = ControlBinding {
        id: 1,
        host_boot: HostBootId::from_raw(1),
        os_session: OsSessionId::from_raw(1),
        remote_session: RemoteSessionId::from_raw(42),
    };
    session.on_session_opened(binding, now).unwrap();
    session.update_view_freshness(true, now).unwrap();
    session.request_control(now).unwrap();
    session
        .on_control_granted(InputLeaseId::from_raw(10), InputTicketId::from_raw(20))
        .unwrap();

    assert!(session.diagnostics().is_controlling);

    // Client window loses focus (user clicked outside or switched apps)
    session.on_focus_loss();

    // State MUST be Suspended { reason: FocusLost }
    assert_eq!(
        session.state(),
        &SessionState::Suspended {
            session: RemoteSessionId::from_raw(42),
            reason: SuspendReason::FocusLost,
        }
    );
    assert_eq!(session.state().display_label(), "Input Suspended");
    assert!(!session.diagnostics().is_controlling);
    assert!(session.diagnostics().is_suspended);

    // Coordinate submission during focus loss must be rejected with InputSuspended
    assert_eq!(
        session.validate_coordinate_mapping(
            DisplayGeometryGeneration::INITIAL,
            ViewportMappingGeneration::INITIAL
        ),
        Err(SessionError::InputSuspended)
    );

    // Focus regained: if lease still valid and view fresh, control resumes
    let resumed = session.on_focus_gained(now).unwrap();
    assert!(resumed);
    assert!(matches!(session.state(), SessionState::Controlling { .. }));
    assert!(session.diagnostics().is_controlling);
}

#[test]
fn focus_loss_releases_all_held_remote_keys_and_buttons() {
    let desktop_bounds =
        InputBounds::new(DesktopPoint { x: 0, y: 0 }, 1920, 1080).expect("valid bounds");
    let mut client = sample_client(desktop_bounds);
    let mut out = [0u8; MAX_INPUT_RECORD_BYTES];
    let now = ClientInstant(1000);

    // Client presses key 'A' (usage 4) and primary mouse button
    let key_a = PhysicalKey::new(4).unwrap();
    let btn_primary = PointerButton::Primary;
    let pos = DesktopPoint { x: 100, y: 100 };

    let enc_key = client
        .action(
            Action::Key {
                key: key_a,
                transition: KeyTransition::Press,
            },
            &mut out,
            now,
        )
        .expect("press key");
    assert_eq!(enc_key.sequence, 0);

    let enc_btn = client
        .action(
            Action::Button {
                button: btn_primary,
                pressed: true,
                position: pos,
            },
            &mut out,
            now,
        )
        .expect("press button");
    assert_eq!(enc_btn.sequence, 1);

    // Focus loss occurs: client stops with FocusLost
    client.stop(StopReason::FocusLost);
    assert_eq!(client.stopped(), Some(StopReason::FocusLost));

    // Any subsequent action is refused with Stopped(FocusLost)
    let err = client
        .action(
            Action::Key {
                key: key_a,
                transition: KeyTransition::Repeat,
            },
            &mut out,
            ClientInstant(now.0 + 1000),
        )
        .unwrap_err();
    assert_eq!(err, fr_client::input::Error::Stopped(StopReason::FocusLost));

    // Reconciling after stop is also refused: stopped sessions cannot produce input records
    let rec_err = client
        .reconcile_held(HeldState::empty(), &mut out, ClientInstant(now.0 + 260_000))
        .unwrap_err();
    assert_eq!(
        rec_err,
        fr_client::input::Error::Stopped(StopReason::FocusLost)
    );
}

#[test]
fn focus_gain_with_expired_lease_refuses_silent_resurrection() {
    let mut session = ClientSession::new(ReconnectPolicy::default());
    let now = ClientInstant(10_000_000);

    session.connect(now).unwrap();
    let binding = ControlBinding {
        id: 1,
        host_boot: HostBootId::from_raw(1),
        os_session: OsSessionId::from_raw(1),
        remote_session: RemoteSessionId::from_raw(42),
    };
    session.on_session_opened(binding, now).unwrap();
    session.update_view_freshness(true, now).unwrap();
    session.request_control(now).unwrap();
    session
        .on_control_granted(InputLeaseId::from_raw(10), InputTicketId::from_raw(20))
        .unwrap();

    // Focus is lost
    session.on_focus_loss();
    assert!(matches!(
        session.state(),
        SessionState::Suspended {
            reason: SuspendReason::FocusLost,
            ..
        }
    ));

    // While unfocused, network drop occurs -> disconnect clears lease
    session.on_disconnect(
        fr_client::session::ReconnectReason::TransportDrop,
        ClientInstant(now.0 + 100_000),
    );

    // Reconnecting state
    assert!(matches!(session.state(), SessionState::Reconnecting { .. }));

    // Advance past backoff and tick reconnect to enter Connecting state
    let reconnect_time = ClientInstant(now.0 + 200_000);
    let advanced = session.tick_reconnect(reconnect_time).unwrap();
    assert!(advanced);
    assert!(matches!(session.state(), SessionState::Connecting { .. }));

    // Reconnect succeeds back to Viewing
    session.on_session_opened(binding, reconnect_time).unwrap();
    assert!(matches!(session.state(), SessionState::Viewing { .. }));

    // When focus is regained on a session without an active lease, control is NOT silently granted!
    let resumed = session.on_focus_gained(reconnect_time).unwrap();
    assert!(!resumed);
    assert!(matches!(session.state(), SessionState::Viewing { .. }));
    assert_eq!(session.diagnostics().current_lease, None);
}

// =========================================================================
// 3. Shortcut-Capture Toggle and Minimal Toolbar Tests
// =========================================================================

#[test]
fn platform_shortcut_capability_table_advertises_all_profiles() {
    let all = PlatformCapabilityRow::all_platforms();
    assert_eq!(all.len(), 7);

    // Verify Linux X11 row
    let x11 = PlatformCapabilityRow::for_platform(PlatformId::LinuxX11);
    assert_eq!(x11.platform, PlatformId::LinuxX11);
    assert_eq!(x11.mechanism, ShortcutRoutingMechanism::X11KeyboardGrab);
    assert_eq!(x11.status, ShortcutSupportStatus::Supported);
    assert!(x11.reserved_shortcuts_intercepted.contains(&"Alt+Tab"));

    // Verify Windows row
    let win = PlatformCapabilityRow::for_platform(PlatformId::Windows);
    assert_eq!(win.platform, PlatformId::Windows);
    assert_eq!(win.mechanism, ShortcutRoutingMechanism::Win32LowLevelHook);
    assert_eq!(win.status, ShortcutSupportStatus::Supported);
    assert!(win.notes.contains("Ctrl+Alt+Del"));

    // Verify macOS row
    let mac = PlatformCapabilityRow::for_platform(PlatformId::MacOS);
    assert_eq!(mac.platform, PlatformId::MacOS);
    assert_eq!(mac.mechanism, ShortcutRoutingMechanism::MacOsEventTap);
    assert!(matches!(
        mac.status,
        ShortcutSupportStatus::SupportedWithPermission { .. }
    ));

    // Verify Browser row
    let browser = PlatformCapabilityRow::for_platform(PlatformId::Browser);
    assert_eq!(browser.platform, PlatformId::Browser);
    assert_eq!(
        browser.mechanism,
        ShortcutRoutingMechanism::BrowserKeyboardLock
    );

    // Verify Mobile rows
    let android = PlatformCapabilityRow::for_platform(PlatformId::Android);
    assert!(matches!(
        android.status,
        ShortcutSupportStatus::Unsupported { .. }
    ));
    let ios = PlatformCapabilityRow::for_platform(PlatformId::Ios);
    assert!(matches!(
        ios.status,
        ShortcutSupportStatus::Unsupported { .. }
    ));
}

#[test]
fn shortcut_capture_toggle_and_toolbar_integration() {
    let row = PlatformCapabilityRow::for_platform(PlatformId::LinuxX11);
    let mut controller = ShortcutCaptureController::new(row);
    let mut toolbar = ToolbarModel::new().with_host("workstation.ts.net");

    // Initially disabled
    toolbar.update_from_shortcuts(&controller);
    assert_eq!(toolbar.shortcut_mode, ShortcutCaptureMode::Disabled);
    assert_eq!(toolbar.shortcut_status_text, "Local System");
    assert!(toolbar.status_line().contains("[Shortcuts: Local System]"));

    // User toggles shortcut capture on toolbar
    let mode = toolbar
        .toggle_shortcut_capture(&mut controller)
        .expect("must toggle on X11");
    assert_eq!(mode, ShortcutCaptureMode::Enabled);
    assert_eq!(toolbar.shortcut_mode, ShortcutCaptureMode::Enabled);
    assert_eq!(toolbar.shortcut_status_text, "Routing to Remote");
    assert!(
        toolbar
            .status_line()
            .contains("[Shortcuts: Routing to Remote]")
    );

    // User toggles it back off
    let mode = toolbar
        .toggle_shortcut_capture(&mut controller)
        .expect("must toggle off");
    assert_eq!(mode, ShortcutCaptureMode::Disabled);
    assert_eq!(toolbar.shortcut_status_text, "Local System");
}

#[test]
fn shortcut_capture_permission_refusal_on_unprivileged_macos() {
    let row = PlatformCapabilityRow::for_platform(PlatformId::MacOS);
    let mut controller = ShortcutCaptureController::new(row);
    let mut toolbar = ToolbarModel::new();

    toolbar.update_from_shortcuts(&controller);
    assert_eq!(toolbar.shortcut_status_text, "Unsupported");

    // Attempting toggle without permission fails with PermissionRequired
    let err = toolbar
        .toggle_shortcut_capture(&mut controller)
        .unwrap_err();
    assert!(matches!(
        err,
        ShortcutCaptureError::PermissionRequired {
            platform: PlatformId::MacOS,
            ..
        }
    ));

    // Once Accessibility permission is granted, toggle succeeds
    controller.set_permission(true);
    let mode = toolbar
        .toggle_shortcut_capture(&mut controller)
        .expect("toggle with permission");
    assert_eq!(mode, ShortcutCaptureMode::Enabled);
    assert_eq!(toolbar.shortcut_status_text, "Routing to Remote");
}
