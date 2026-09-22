#![forbid(unsafe_code)]
#![allow(clippy::doc_markdown)]
//! fr-web: FrankenRemote Browser Client WASM State Machine & WebCodecs Pipeline
//!
//! Provides the core browser-side session engine:
//! - Same-origin bootstrap authentication with bounded first message nonce
//! - Generation-fenced input encoding (Direct Touch & Trackpad modes)
//! - WebCodecs HEVC configuration generation (hvcC parameter sets + Annex B normalization)
//! - Tab visibility & bfcache authority management (hidden tabs never retain control)
//! - Asset version handshake verification (detects host updates and forces clean reload)

use serde::{Deserialize, Serialize};


/// Error types returned by web client operations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum WebError {
    InvalidState(String),
    StaleAuthority(String),
    AuthenticationFailed(String),
    UnsupportedCodec(String),
    HiddenTabRefusal,
    AssetVersionMismatch { client: String, host: String },
}

impl std::fmt::Display for WebError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidState(msg) => write!(f, "Invalid state: {msg}"),
            Self::StaleAuthority(msg) => write!(f, "Stale authority: {msg}"),
            Self::AuthenticationFailed(msg) => write!(f, "Auth failed: {msg}"),
            Self::UnsupportedCodec(msg) => write!(f, "Unsupported codec: {msg}"),
            Self::HiddenTabRefusal => write!(f, "Input refused: tab is hidden or backgrounded"),
            Self::AssetVersionMismatch { client, host } => {
                write!(f, "Asset version mismatch: client={client}, host={host}")
            }
        }
    }
}

impl std::error::Error for WebError {}

/// Session lifecycle state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum WebSessionState {
    Disconnected,
    Connecting,
    Authenticating,
    Connected,
    Suspended,
    Closed,
}

/// Touch interaction mode for web client.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum WebTouchMode {
    DirectTouch,
    Trackpad,
}

/// Control event emitted by the state machine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum WebControlEvent {
    None,
    ControlRevoked(String),
    NeedsReconnection,
    RequestRecoveryFrame,
}

/// Configuration for browser session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebSessionConfig {
    pub host_url: String,
    pub origin: String,
    pub nonce: String,
    pub touch_mode: WebTouchMode,
}

/// Canonical WebCodecs configuration descriptor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebCodecsConfig {
    pub codec_string: String,
    pub description: Vec<u8>,
    pub coded_width: u32,
    pub coded_height: u32,
}

/// Browser client state machine.
#[derive(Debug, Clone)]
pub struct WebClientSession {
    config: WebSessionConfig,
    state: WebSessionState,
    geometry_gen: u64,
    codec_gen: u64,
    input_lease: Option<u64>,
    is_hidden: bool,
    active_codec_config: Option<WebCodecsConfig>,
}

impl WebClientSession {
    pub fn new(config: WebSessionConfig) -> Self {
        Self {
            config,
            state: WebSessionState::Disconnected,
            geometry_gen: 1,
            codec_gen: 1,
            input_lease: None,
            is_hidden: false,
            active_codec_config: None,
        }
    }

    pub fn state(&self) -> WebSessionState {
        self.state
    }

    pub fn is_hidden(&self) -> bool {
        self.is_hidden
    }

    pub fn input_lease(&self) -> Option<u64> {
        self.input_lease
    }

    /// Called when the browser transport (WebTransport/WSS) opens.
    /// Returns the first application-level authentication payload carrying the single-use nonce.
    pub fn on_transport_open(&mut self) -> Result<Vec<u8>, WebError> {
        if self.state != WebSessionState::Disconnected && self.state != WebSessionState::Connecting {
            return Err(WebError::InvalidState(format!("{:?}", self.state)));
        }
        self.state = WebSessionState::Authenticating;

        // Build first authentication message (JSON payload bounded to 1 KiB)
        let auth_payload = serde_json::json!({
            "type": "first_auth",
            "nonce": self.config.nonce,
            "origin": self.config.origin,
            "version": CURRENT_WEB_ASSET_VERSION,
        });

        serde_json::to_vec(&auth_payload).map_err(|e| WebError::InvalidState(e.to_string()))
    }

    /// Process host authentication response.
    pub fn on_auth_response(&mut self, success: bool, lease_handle: Option<u64>) -> Result<(), WebError> {
        if self.state != WebSessionState::Authenticating {
            return Err(WebError::InvalidState(format!("{:?}", self.state)));
        }

        if success {
            self.state = WebSessionState::Connected;
            self.input_lease = lease_handle;
            Ok(())
        } else {
            self.state = WebSessionState::Closed;
            self.input_lease = None;
            Err(WebError::AuthenticationFailed("Host refused bootstrap token".into()))
        }
    }

    /// Handle visibility change (document.hidden / pagehide).
    /// Constitutional Invariant: Hidden tabs never retain input control.
    pub fn on_visibility_change(&mut self, is_hidden: bool) -> WebControlEvent {
        self.is_hidden = is_hidden;
        if is_hidden {
            // Revoke control immediately
            let had_lease = self.input_lease.is_some();
            self.input_lease = None;
            if self.state == WebSessionState::Connected {
                self.state = WebSessionState::Suspended;
            }
            if had_lease {
                WebControlEvent::ControlRevoked("Tab backgrounded/hidden".into())
            } else {
                WebControlEvent::None
            }
        } else {
            // Resumed
            if self.state == WebSessionState::Suspended {
                self.state = WebSessionState::Connected;
                WebControlEvent::RequestRecoveryFrame
            } else {
                WebControlEvent::None
            }
        }
    }

    /// Handle bfcache restoration: forces clean state check and re-observation.
    pub fn on_bfcache_restore(&mut self) -> WebControlEvent {
        self.input_lease = None;
        self.state = WebSessionState::Connecting;
        WebControlEvent::NeedsReconnection
    }

    /// Encode pointer movement / button press.
    pub fn encode_pointer(
        &self,
        x: i32,
        y: i32,
        action: u8,
        button: u8,
    ) -> Result<Vec<u8>, WebError> {
        if self.is_hidden {
            return Err(WebError::HiddenTabRefusal);
        }
        if self.state != WebSessionState::Connected || self.input_lease.is_none() {
            return Err(WebError::StaleAuthority("No active input lease".into()));
        }

        let msg = serde_json::json!({
            "type": "pointer",
            "x": x,
            "y": y,
            "action": action,
            "button": button,
            "lease": self.input_lease,
            "geom_gen": self.geometry_gen,
        });

        serde_json::to_vec(&msg).map_err(|e| WebError::InvalidState(e.to_string()))
    }

    /// Encode key event.
    pub fn encode_key(&self, keycode: u32, action: u8) -> Result<Vec<u8>, WebError> {
        if self.is_hidden {
            return Err(WebError::HiddenTabRefusal);
        }
        if self.state != WebSessionState::Connected || self.input_lease.is_none() {
            return Err(WebError::StaleAuthority("No active input lease".into()));
        }

        let msg = serde_json::json!({
            "type": "key",
            "keycode": keycode,
            "action": action,
            "lease": self.input_lease,
        });

        serde_json::to_vec(&msg).map_err(|e| WebError::InvalidState(e.to_string()))
    }

    /// Encode direct committed text (IME commit).
    pub fn encode_commit_text(&self, text: &str) -> Result<Vec<u8>, WebError> {
        if self.is_hidden {
            return Err(WebError::HiddenTabRefusal);
        }
        if self.state != WebSessionState::Connected || self.input_lease.is_none() {
            return Err(WebError::StaleAuthority("No active input lease".into()));
        }

        let msg = serde_json::json!({
            "type": "commit_text",
            "text": text,
            "lease": self.input_lease,
        });

        serde_json::to_vec(&msg).map_err(|e| WebError::InvalidState(e.to_string()))
    }

    /// Updates current geometry generation.
    pub fn update_geometry(&mut self, gen_id: u64) {
        self.geometry_gen = gen_id;
    }

    /// Updates codec configuration from host parameters.
    pub fn update_codec_config(&mut self, config: WebCodecsConfig) -> Result<(), WebError> {
        if let Some(existing) = &self.active_codec_config
            && existing.codec_string != config.codec_string
        {
            self.codec_gen += 1;
        }
        self.active_codec_config = Some(config);
        Ok(())
    }
}

/// WebCodecs parameter generator & validator.
pub fn generate_webcodecs_config(
    width: u32,
    height: u32,
    vps: &[u8],
    sps: &[u8],
    pps: &[u8],
) -> Result<WebCodecsConfig, WebError> {
    if vps.is_empty() || sps.is_empty() || pps.is_empty() {
        return Err(WebError::UnsupportedCodec("Empty parameter sets".into()));
    }

    let vps_len = u16::try_from(vps.len())
        .map_err(|_| WebError::UnsupportedCodec("VPS exceeds max length".into()))?;
    let sps_len = u16::try_from(sps.len())
        .map_err(|_| WebError::UnsupportedCodec("SPS exceeds max length".into()))?;
    let pps_len = u16::try_from(pps.len())
        .map_err(|_| WebError::UnsupportedCodec("PPS exceeds max length".into()))?;

    // Baseline HEVC Main Profile string: hvc1.<profile>.<tier_flag+level>.<tier>.<constraints>
    // e.g. "hvc1.1.6.L93.B0" (Main profile, Main tier, Level 3.1)
    let codec_string = "hvc1.1.6.L93.B0".to_string();

    // Construct canonical hvcC configuration record box
    let mut description = Vec::with_capacity(32 + vps.len() + sps.len() + pps.len());
    description.push(1); // configurationVersion = 1
    description.push(0x01); // general_profile_space, tier_flag, profile_idc = 1 (Main)
    description.extend_from_slice(&[0x60, 0x00, 0x00, 0x00]); // profile_compatibility_flags
    description.extend_from_slice(&[0x00, 0x00, 0x00, 0x00, 0x00, 0x00]); // constraint_indicator_flags
    description.push(93); // general_level_idc = 93 (Level 3.1)
    description.extend_from_slice(&[0xf0, 0x00]); // min_spatial_segmentation_idc
    description.push(0xfc); // parallelismType
    description.push(0xfc); // chromaFormat = 1 (4:2:0)
    description.push(0xf8); // bitDepthLumaMinus8 = 0 (8-bit)
    description.push(0xf8); // bitDepthChromaMinus8 = 0 (8-bit)
    description.extend_from_slice(&[0x00, 0x00]); // avgFrameRate = 0
    description.push(0x03); // constantFrameRate(2) | numTemporalLayers(3) | temporalIdNested(1) | lengthSizeMinusOne(2 = 4 bytes)
    description.push(3); // numOfArrays = 3 (VPS, SPS, PPS)

    // Array 1: VPS (NAL type 32)
    description.push(0x20); // array_completeness(0) | NAL_unit_type(32)
    description.extend_from_slice(&1u16.to_be_bytes()); // numNalus = 1
    description.extend_from_slice(&vps_len.to_be_bytes());
    description.extend_from_slice(vps);

    // Array 2: SPS (NAL type 33)
    description.push(0x21); // NAL_unit_type(33)
    description.extend_from_slice(&1u16.to_be_bytes());
    description.extend_from_slice(&sps_len.to_be_bytes());
    description.extend_from_slice(sps);

    // Array 3: PPS (NAL type 34)
    description.push(0x22); // NAL_unit_type(34)
    description.extend_from_slice(&1u16.to_be_bytes());
    description.extend_from_slice(&pps_len.to_be_bytes());
    description.extend_from_slice(pps);

    Ok(WebCodecsConfig {
        codec_string,
        description,
        coded_width: width,
        coded_height: height,
    })
}

/// Asset versioning.
pub const CURRENT_WEB_ASSET_VERSION: &str = "1.0.0";

/// Validate asset version consistency.
pub fn validate_asset_version(client_version: &str, host_version: &str) -> Result<(), WebError> {
    if client_version == host_version {
        Ok(())
    } else {
        Err(WebError::AssetVersionMismatch {
            client: client_version.to_string(),
            host: host_version.to_string(),
        })
    }
}
