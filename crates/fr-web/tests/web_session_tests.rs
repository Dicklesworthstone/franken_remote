use fr_web::{
    CURRENT_WEB_ASSET_VERSION, WebClientSession, WebControlEvent, WebError, WebSessionConfig,
    WebSessionState, WebTouchMode, generate_webcodecs_config, validate_asset_version,
};

#[test]
fn test_session_first_auth_and_lease_lifecycle() {
    let config = WebSessionConfig {
        host_url: "https://host.tailnet.ts.net:443".into(),
        origin: "https://host.tailnet.ts.net".into(),
        nonce: "test_bootstrap_nonce_1234".into(),
        touch_mode: WebTouchMode::DirectTouch,
    };

    let mut session = WebClientSession::new(config);
    assert_eq!(session.state(), WebSessionState::Disconnected);

    // Transport open produces first auth message
    let auth_bytes = session.on_transport_open().expect("transport open");
    assert_eq!(session.state(), WebSessionState::Authenticating);

    let auth_val: serde_json::Value = serde_json::from_slice(&auth_bytes).unwrap();
    assert_eq!(auth_val["type"], "first_auth");
    assert_eq!(auth_val["nonce"], "test_bootstrap_nonce_1234");
    assert_eq!(auth_val["version"], CURRENT_WEB_ASSET_VERSION);

    // Auth success grants lease
    session
        .on_auth_response(true, Some(999))
        .expect("auth success");
    assert_eq!(session.state(), WebSessionState::Connected);
    assert_eq!(session.input_lease(), Some(999));

    // Can encode pointer when connected
    let ptr = session
        .encode_pointer(100, 200, 0, 1)
        .expect("encode pointer");
    assert_ne!(ptr, [] as [u8; 0]);
}

#[test]
fn test_hidden_tab_revokes_authority_and_refuses_input() {
    let config = WebSessionConfig {
        host_url: "https://host.tailnet.ts.net:443".into(),
        origin: "https://host.tailnet.ts.net".into(),
        nonce: "test_nonce".into(),
        touch_mode: WebTouchMode::Trackpad,
    };

    let mut session = WebClientSession::new(config);
    session.on_transport_open().unwrap();
    session.on_auth_response(true, Some(42)).unwrap();
    assert_eq!(session.input_lease(), Some(42));

    // Tab hidden
    let event = session.on_visibility_change(true);
    assert!(matches!(event, WebControlEvent::ControlRevoked(_)));
    assert_eq!(session.state(), WebSessionState::Suspended);
    assert_eq!(session.input_lease(), None);

    // Input while hidden MUST be refused
    let err = session.encode_pointer(50, 50, 0, 1).unwrap_err();
    assert_eq!(err, WebError::HiddenTabRefusal);

    let key_err = session.encode_key(65, 0).unwrap_err();
    assert_eq!(key_err, WebError::HiddenTabRefusal);

    // Tab resumed
    let resume_event = session.on_visibility_change(false);
    assert_eq!(resume_event, WebControlEvent::RequestRecoveryFrame);
    assert_eq!(session.state(), WebSessionState::Connected);
}

#[test]
fn test_bfcache_restoration_forces_reconnection() {
    let config = WebSessionConfig {
        host_url: "https://host.tailnet.ts.net:443".into(),
        origin: "https://host.tailnet.ts.net".into(),
        nonce: "test_nonce".into(),
        touch_mode: WebTouchMode::DirectTouch,
    };

    let mut session = WebClientSession::new(config);
    session.on_transport_open().unwrap();
    session.on_auth_response(true, Some(88)).unwrap();

    let event = session.on_bfcache_restore();
    assert_eq!(event, WebControlEvent::NeedsReconnection);
    assert_eq!(session.input_lease(), None);
    assert_eq!(session.state(), WebSessionState::Connecting);
}

#[test]
fn test_webcodecs_hvcc_generation() {
    let vps = [0x40, 0x01, 0x0c, 0x01, 0xff, 0xff, 0x01, 0x60];
    let sps = [0x42, 0x01, 0x01, 0x01, 0x60, 0x00, 0x00, 0x03];
    let pps = [0x44, 0x01, 0xc0, 0xf7];

    let config = generate_webcodecs_config(1920, 1080, &vps, &sps, &pps).expect("generate config");
    assert_eq!(config.codec_string, "hvc1.1.6.L93.B0");
    assert_eq!(config.coded_width, 1920);
    assert_eq!(config.coded_height, 1080);
    assert_ne!(config.description, [] as [u8; 0]);
    assert_eq!(config.description[0], 1); // configurationVersion
}

#[test]
fn test_asset_version_handshake() {
    assert!(validate_asset_version("1.0.0", "1.0.0").is_ok());
    assert!(validate_asset_version("1.0.0", "1.1.0").is_err());
}
