//! Integration and fault recovery test suite for the Windows host adapter (plan §8.3, §10.3, §23 Phase 2).

use fr_core::{
    input::{DesktopPoint, InputBounds, KeyTransition, PhysicalKey, PointerButton, ScrollUnit},
    input_submission::{InputSink, Operation, PlatformError, Submission},
};

use frd::windows::{
    AdapterDesc, AdapterLuid, CrossAdapterStrategy, DesktopDuplicationSession, DuplicationError,
    DuplicationState, DxgiErrorCode, DxgiFormat, DxgiRotation, GpuVendor, HardwareEncoderKind,
    HybridGpuSelector, HybridTopology, QUALIFIED_WINDOWS_ROWS, QualificationStatus,
    RecordingSendInputPoster, SendInputRecordedEvent, SessionBoundPipe, SessionLockState,
    SessionServiceError, StreamPixelPoint, StreamResolution, TargetWindowSecurity, TransitionCause,
    TransitionCoordinator, TransitionRecoveryState, VirtualDesktopRect, WindowsInputSink,
    WindowsIntegrityLevel, WindowsReleaseFamily, WindowsSessionInfo, WindowsSessionKind,
    WindowsSessionManager, WtsSessionEvent, classify_build_number, stream_pixel_to_virtual_desktop,
    virtual_desktop_to_send_input,
};

#[test]
fn test_desktop_duplication_prompt_frame_release_invariant() {
    let mut session = DesktopDuplicationSession::new(
        0,
        0,
        StreamResolution {
            width: 1920,
            height: 1080,
        },
        DxgiFormat::B8G8R8A8Unorm,
    );

    assert_eq!(session.state(), DuplicationState::Idle);
    assert_eq!(session.frames_captured(), 0);
    assert_eq!(session.frames_released(), 0);

    // 1. Acquire first frame
    let frame = session.acquire_frame().expect("first acquire succeeds");
    assert_eq!(session.state(), DuplicationState::FrameHeld);
    assert_eq!(session.frames_captured(), 1);
    assert_eq!(frame.dirty_rects.len(), 1);

    // 2. Prompt release invariant: acquiring another frame while holding one MUST fail
    let err = session
        .acquire_frame()
        .expect_err("acquiring while frame held must fail");
    assert_eq!(err, DuplicationError::FrameAlreadyHeld);

    // 3. Copy to owned GPU surface before releasing
    let surface = session
        .copy_to_owned_surface(120)
        .expect("copy to owned surface succeeds");
    assert_eq!(surface.resolution.width, 1920);
    assert_eq!(surface.resolution.height, 1080);
    assert_eq!(surface.byte_size, 1920 * 1080 * 4);
    assert_eq!(surface.copy_duration_micros, 120);

    // 4. Release frame to unblock next capture
    session.release_frame().expect("release succeeds");
    assert_eq!(session.state(), DuplicationState::Idle);
    assert_eq!(session.frames_released(), 1);

    // 5. Subsequent acquire succeeds
    let _frame2 = session
        .acquire_frame()
        .expect("subsequent acquire succeeds");
    assert_eq!(session.frames_captured(), 2);
    session.release_frame().expect("second release succeeds");
}

#[test]
fn test_duplicate_output1_hdr_tone_mapping() {
    // SDR configuration
    let sdr_session = DesktopDuplicationSession::new(
        0,
        0,
        StreamResolution {
            width: 2560,
            height: 1440,
        },
        DxgiFormat::B8G8R8A8Unorm,
    );
    assert!(!sdr_session.format().is_hdr());
    assert_eq!(
        sdr_session.tone_mapping(),
        frd::windows::ToneMappingMethod::None
    );

    // HDR 10-bit configuration (HDR enabled without modifying user OS display settings)
    let hdr_session = DesktopDuplicationSession::new(
        0,
        0,
        StreamResolution {
            width: 3840,
            height: 2160,
        },
        DxgiFormat::R10G10B10A2Unorm,
    );
    assert!(hdr_session.format().is_hdr());
    assert_eq!(
        hdr_session.tone_mapping(),
        frd::windows::ToneMappingMethod::Bt2446MethodA
    );

    // HDR 16-bit float configuration
    let hdr_float_session = DesktopDuplicationSession::new(
        0,
        0,
        StreamResolution {
            width: 3840,
            height: 2160,
        },
        DxgiFormat::R16G16B16A16Float,
    );
    assert!(hdr_float_session.format().is_hdr());
    assert_eq!(hdr_float_session.format().bits_per_pixel(), 64);
}

#[test]
fn test_display_transitions_and_gpu_fault_recovery() {
    let mut coord = TransitionCoordinator::new();
    assert_eq!(coord.state(), TransitionRecoveryState::Normal);

    // 1. Access lost (e.g. lock screen or UAC prompt)
    let state = coord.on_dxgi_fault(DxgiErrorCode::AccessLost, 1_000);
    assert!(matches!(
        state,
        TransitionRecoveryState::AwaitingRetry { attempt: 1, .. }
    ));
    assert_eq!(coord.history().len(), 1);
    assert_eq!(coord.history()[0].cause, TransitionCause::LockScreen);

    // 2. Recovery succeeds on unlock
    coord.on_recovery_succeeded(1_250, TransitionCause::Unlock);
    assert_eq!(coord.state(), TransitionRecoveryState::Normal);
    assert_eq!(coord.history().len(), 2);
    assert!(coord.history()[1].recovery_successful);

    // 3. GPU device removed (TDR reset)
    let state = coord.on_dxgi_fault(DxgiErrorCode::DeviceRemoved, 2_000);
    assert!(matches!(
        state,
        TransitionRecoveryState::AwaitingRetry { attempt: 1, .. }
    ));
    assert!(matches!(
        coord.history()[2].cause,
        TransitionCause::GpuDeviceReset { .. }
    ));

    // 4. Duplication exhaustion (all sessions taken)
    let state = coord.on_dxgi_fault(DxgiErrorCode::NotCurrentlyAvailable, 3_000);
    assert!(matches!(
        state,
        TransitionRecoveryState::AwaitingRetry { .. }
    ));
    assert_eq!(
        coord.history()[3].cause,
        TransitionCause::DuplicationExhaustion
    );
}

#[test]
fn test_hybrid_gpu_selection_and_pairing() {
    // Muxless laptop with Intel iGPU driving outputs + NVIDIA RTX 4080 dGPU for NVENC
    let igpu = AdapterDesc {
        index: 0,
        luid: AdapterLuid {
            low_part: 0x1000,
            high_part: 0,
        },
        description: "Intel Iris Xe Graphics".into(),
        vendor: GpuVendor::Intel,
        vendor_id: 0x8086,
        device_id: 0x9A49,
        dedicated_video_memory_bytes: 128 * 1024 * 1024,
        shared_system_memory_bytes: 16 * 1024 * 1024 * 1024,
        output_count: 2, // Drives laptop screen + HDMI
        is_software: false,
        supported_encoder: HardwareEncoderKind::None,
    };

    let dgpu = AdapterDesc {
        index: 1,
        luid: AdapterLuid {
            low_part: 0x2000,
            high_part: 0,
        },
        description: "NVIDIA GeForce RTX 4080 Laptop GPU".into(),
        vendor: GpuVendor::Nvidia,
        vendor_id: 0x10DE,
        device_id: 0x27E0,
        dedicated_video_memory_bytes: 12 * 1024 * 1024 * 1024,
        shared_system_memory_bytes: 16 * 1024 * 1024 * 1024,
        output_count: 0, // No direct outputs in muxless configuration
        is_software: false,
        supported_encoder: HardwareEncoderKind::Nvenc,
    };

    let adapters = vec![igpu, dgpu];

    // Detect topology
    let topology = HybridGpuSelector::detect_topology(&adapters);
    assert_eq!(topology, HybridTopology::MuxlessHybrid);

    // Select pairing for output 0
    let pairing =
        HybridGpuSelector::select_optimal_pairing(&adapters, 0).expect("pairing should succeed");
    assert_eq!(pairing.capture_adapter.low_part, 0x1000); // Capture on iGPU
    assert_eq!(pairing.encode_adapter.low_part, 0x2000); // Encode on dGPU NVENC
    assert_eq!(pairing.strategy, CrossAdapterStrategy::SharedNtHandle);
    assert_eq!(pairing.encoder_kind, HardwareEncoderKind::Nvenc);
    assert!(pairing.estimated_copy_overhead_micros > 0);
}

#[test]
fn test_session0_isolation_and_named_pipe_security() {
    let mut manager = WindowsSessionManager::new(1);

    manager.update_session(WindowsSessionInfo {
        session_id: 0,
        kind: WindowsSessionKind::SessionZeroService,
        user_name: Some("SYSTEM".into()),
        domain_name: Some("NT AUTHORITY".into()),
        lock_state: SessionLockState::Unlocked,
    });

    manager.update_session(WindowsSessionInfo {
        session_id: 1,
        kind: WindowsSessionKind::InteractiveConsole,
        user_name: Some("alice".into()),
        domain_name: Some("WORKGROUP".into()),
        lock_state: SessionLockState::Unlocked,
    });

    // Invariant: Service NEVER captures Session 0 as user desktop
    let res = manager.validate_capture_target(0);
    assert_eq!(res, Err(SessionServiceError::SessionZeroCaptureForbidden));

    // Interactive session 1 is valid
    assert!(manager.validate_capture_target(1).is_ok());

    // Pipe path creation rejects Session 0
    let pipe_err = SessionBoundPipe::new(0, "tok123").expect_err("Session 0 pipe forbidden");
    assert_eq!(pipe_err, SessionServiceError::SessionZeroCaptureForbidden);

    // Interactive session pipe creation succeeds
    let pipe = SessionBoundPipe::new(1, "tok123").expect("session 1 pipe succeeds");
    assert_eq!(pipe.pipe_path, r"\\.\pipe\frankenremote-session-1-tok123");

    // Path traversal in pipe token is rejected
    let bad_pipe = SessionBoundPipe::new(1, "../malicious").expect_err("traversal rejected");
    assert_eq!(bad_pipe, SessionServiceError::InvalidPipeFormat);

    // Lock and unlock session updates
    manager.on_wts_event(WtsSessionEvent::SessionLock, 1);
    assert_eq!(
        manager.get_session(1).unwrap().lock_state,
        SessionLockState::Locked
    );

    manager.on_wts_event(WtsSessionEvent::SessionUnlock, 1);
    assert_eq!(
        manager.get_session(1).unwrap().lock_state,
        SessionLockState::Unlocked
    );
}

fn make_test_bounds() -> InputBounds {
    InputBounds::new(DesktopPoint { x: 0, y: 0 }, 1920, 1080).expect("valid bounds")
}

#[test]
fn test_send_input_uipi_integrity_refusal_for_elevated_windows() {
    let poster = RecordingSendInputPoster::new();
    let bounds = make_test_bounds();
    let virtual_screen = VirtualDesktopRect::new(0, 0, 1920, 1080);

    // Medium integrity agent (standard non-elevated user context)
    let mut sink = WindowsInputSink::new(
        poster,
        bounds,
        virtual_screen,
        WindowsIntegrityLevel::Medium,
    );

    // 1. Normal window input succeeds
    sink.set_target_window_security(TargetWindowSecurity::NormalWindow);
    let sub = sink.submit(Operation::Absolute(DesktopPoint { x: 100, y: 100 }));
    assert_eq!(sub, Submission::Submitted);
    assert_eq!(sink.poster().events().len(), 1);

    // 2. Elevated window target: UIPI refusal returned as PlatformError::Unsupported
    sink.set_target_window_security(TargetWindowSecurity::ElevatedWindow);
    let sub = sink.submit(Operation::Absolute(DesktopPoint { x: 200, y: 200 }));
    assert_eq!(sub, Submission::NotSubmitted(PlatformError::Unsupported));

    // 3. Secure desktop target: refusal returned
    sink.set_target_window_security(TargetWindowSecurity::SecureDesktop);
    let sub = sink.submit(Operation::Button {
        button: PointerButton::Primary,
        pressed: true,
    });
    assert_eq!(sub, Submission::NotSubmitted(PlatformError::Unsupported));

    // 4. High integrity agent (elevated service / uiAccess) can inject into elevated windows
    let poster_high = RecordingSendInputPoster::new();
    let bounds_high = make_test_bounds();
    let mut sink_high = WindowsInputSink::new(
        poster_high,
        bounds_high,
        virtual_screen,
        WindowsIntegrityLevel::High,
    );
    sink_high.set_target_window_security(TargetWindowSecurity::ElevatedWindow);
    let sub_high = sink_high.submit(Operation::Absolute(DesktopPoint { x: 300, y: 300 }));
    assert_eq!(sub_high, Submission::Submitted);
}

#[test]
fn test_multi_monitor_virtual_desktop_negative_coordinates() {
    // Monitor 1 (primary): [0, 0, 1920, 1080]
    // Monitor 2 (left):    [-1920, 0, 0, 1080]
    // Virtual desktop:     [-1920, 0, 1920, 1080], width = 3840, height = 1080
    let virtual_desktop = VirtualDesktopRect::new(-1920, 0, 1920, 1080);
    assert_eq!(virtual_desktop.width().unwrap(), 3840);
    assert_eq!(virtual_desktop.height().unwrap(), 1080);

    let monitor2_rect = VirtualDesktopRect::new(-1920, 0, 0, 1080);
    let monitor2_res = StreamResolution {
        width: 1920,
        height: 1080,
    };

    // Pixel (960, 540) on Monitor 2 maps to virtual desktop coordinate (-960, 540)
    let virt_point = stream_pixel_to_virtual_desktop(
        StreamPixelPoint { x: 960, y: 540 },
        monitor2_res,
        monitor2_rect,
        DxgiRotation::Identity,
    )
    .expect("coordinate mapping succeeds");
    assert_eq!(virt_point.x, -960);
    assert_eq!(virt_point.y, 540);

    // Map virtual desktop point (-960, 540) to SendInput [0, 65535] range
    let send_input_pt = virtual_desktop_to_send_input(virt_point, virtual_desktop)
        .expect("send input normalization succeeds");

    // (-960 - (-1920)) / 3839 * 65535 = 960 / 3839 * 65535 ≈ 16388 (25% across screen)
    assert!(send_input_pt.x > 16000 && send_input_pt.x < 17000);
    // 540 / 1079 * 65535 ≈ 32799 (50% down screen)
    assert!(send_input_pt.y > 32000 && send_input_pt.y < 33500);
}

#[test]
fn test_dxgi_rotation_coordinates_mapping() {
    let output_rect = VirtualDesktopRect::new(0, 0, 1080, 1920); // Portrait monitor
    let stream_res = StreamResolution {
        width: 1080,
        height: 1920,
    };

    // Rotate 90 degrees
    let pt90 = stream_pixel_to_virtual_desktop(
        StreamPixelPoint { x: 100, y: 200 },
        stream_res,
        output_rect,
        DxgiRotation::Rotate90,
    )
    .expect("rotate 90 mapping succeeds");
    assert!(pt90.x >= 0 && pt90.y >= 0);

    // Rotate 180 degrees
    let pt180 = stream_pixel_to_virtual_desktop(
        StreamPixelPoint { x: 100, y: 200 },
        stream_res,
        output_rect,
        DxgiRotation::Rotate180,
    )
    .expect("rotate 180 mapping succeeds");
    assert!(pt180.x >= 0 && pt180.y >= 0);
}

#[test]
fn test_windows_qualification_matrix_coverage() {
    assert_eq!(QUALIFIED_WINDOWS_ROWS.len(), 3);

    let row_win11 = &QUALIFIED_WINDOWS_ROWS[0];
    assert_eq!(row_win11.os_family, WindowsReleaseFamily::Windows11_24H2);
    assert_eq!(row_win11.desktop_duplication, QualificationStatus::Passed);
    assert_eq!(row_win11.duplicate_output1_hdr, QualificationStatus::Passed);
    assert_eq!(row_win11.hardware_hevc.0, HardwareEncoderKind::Nvenc);
    assert_eq!(row_win11.hardware_hevc.1, QualificationStatus::Passed);
    assert_eq!(
        row_win11.session0_isolation_verified,
        QualificationStatus::Passed
    );
    assert_eq!(
        row_win11.uipi_elevation_refusal_typed,
        QualificationStatus::Passed
    );

    // Build classification
    assert_eq!(
        classify_build_number(26100),
        WindowsReleaseFamily::Windows11_24H2
    );
    assert_eq!(
        classify_build_number(22631),
        WindowsReleaseFamily::Windows11_23H2
    );
    assert_eq!(
        classify_build_number(19045),
        WindowsReleaseFamily::Windows10_22H2
    );
    assert_eq!(
        classify_build_number(17763),
        WindowsReleaseFamily::WindowsServer2022
    );
}

#[test]
fn test_send_input_keyboard_and_wheel_events() {
    let poster = RecordingSendInputPoster::new();
    let bounds = make_test_bounds();
    let virtual_screen = VirtualDesktopRect::new(0, 0, 1920, 1080);

    let mut sink = WindowsInputSink::new(
        poster,
        bounds,
        virtual_screen,
        WindowsIntegrityLevel::Medium,
    );

    // Key press
    let sub = sink.submit(Operation::Key {
        key: PhysicalKey::new(0x04).expect("valid key"), // 'A' key
        transition: KeyTransition::Press,
    });
    assert_eq!(sub, Submission::Submitted);

    // Key release
    let sub_rel = sink.submit(Operation::Key {
        key: PhysicalKey::new(0x04).expect("valid key"),
        transition: KeyTransition::Release,
    });
    assert_eq!(sub_rel, Submission::Submitted);

    // Wheel scroll
    let sub_wheel = sink.submit(Operation::Scroll {
        x: 0,
        y: 1,
        unit: ScrollUnit::Lines,
    });
    assert_eq!(sub_wheel, Submission::Submitted);

    let events = sink.poster().events();
    assert_eq!(events.len(), 3);
    assert!(matches!(
        events[0],
        SendInputRecordedEvent::KeyboardKey { down: true, .. }
    ));
    assert!(matches!(
        events[1],
        SendInputRecordedEvent::KeyboardKey { down: false, .. }
    ));
    assert!(matches!(
        events[2],
        SendInputRecordedEvent::MouseWheel { delta: 120, .. }
    ));
}
