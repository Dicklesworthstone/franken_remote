#![forbid(unsafe_code)]
//! Asupersync HTTPS/WSS browser workstation ingress (`frd`).
//!
//! Wires together the existing hardened security and transport layers:
//! - [`crate::broker::browser_security::BrowserSecurityPolicy`] (exact `Origin`,
//!   `Host`/SNI DNS-rebinding defense, Fetch Metadata, URL query safety,
//!   CSPRNG HMAC-bound single-use bootstrap nonces & role-specific auxiliary tickets,
//!   first-message `SocketAuthGuard`).
//! - [`crate::broker::browser_assets::BrowserAssets`] (self-hosted `web/` shell,
//!   strict CSP, zero CDN, zero authority on `GET`).
//! - [`fr_transport::wss`] (`ReceiverCreditWindow`, `ChannelSupervisor`,
//!   `WssProfile` honest TCP head-of-line degradation labeling, no compression).
//!
//! Pure safe Rust (`#![forbid(unsafe_code)]`), Asupersync-only (no Tokio, no
//! third-party web server frameworks).

pub mod http;
pub mod websocket;

use crate::broker::{
    browser_assets::BrowserAssets,
    browser_security::{
        AuxiliaryChannelRole, BROWSER_API_CSP, BROWSER_UI_CSP, BootstrapRequest,
        BrowserSecurityPolicy, BrowserSessionRole, ExpectedHostOrigin, FIRST_MESSAGE_AUTH_TIMEOUT,
        FetchMetadata, MAX_AUTH_MESSAGE_BYTES, SocketAuthGuard, check_url_query_safety,
    },
};
use asupersync::{
    io::{AsyncRead, AsyncWrite},
    net::{TcpListener, TcpStream},
    tls::TlsAcceptor,
};
use core::fmt;
use fr_core::{
    ids::{HostBootId, OsSessionId, RemoteSessionId},
    limits::ProtocolLimits,
    time::HostInstant,
};
use fr_transport::wss::{
    ChannelGeneration, ChannelSupervisor, ReceiverCreditWindow, WssChannelRole, WssError,
    WssProfile, is_compression_allowed,
};
use fr_wire::{
    input::{InputDelivery, InputDirection},
    negotiation::ControlBinding,
    receiver_pressure::{self, ChannelRole},
};
use std::{
    collections::{HashMap, VecDeque},
    net::{IpAddr, SocketAddr},
    sync::{Arc, Mutex},
    time::Duration,
};

/// Maximum concurrent active browser workstation sessions on this host.
pub const MAX_BROWSER_SESSIONS: usize = 16;
/// Maximum bounded outbound video frames queued per browser session before write.
pub const MAX_OUTBOUND_VIDEO_QUEUE: usize = 8;
/// Default receiver credit ceiling in bytes for a browser video WSS channel (2 MiB).
pub const DEFAULT_VIDEO_CREDIT_BYTES: u64 = 2 * 1024 * 1024;
/// Default receiver credit ceiling in frames for a browser video WSS channel.
pub const DEFAULT_VIDEO_CREDIT_FRAMES: u64 = 4;

const ATTACH_MAGIC: [u8; 4] = *b"FRAT";
const TICKET_GRANT_MAGIC: [u8; 4] = *b"FRTK";
const ATTACH_MESSAGE_BYTES: usize = 4 + 16 + 1 + 32;
const TICKET_GRANT_BYTES: usize = 4 + 1 + 16 + 32;

/// Configuration for the `frd` HTTPS/WSS browser ingress listener.
#[derive(Debug, Clone)]
pub struct BrowserIngressConfig {
    pub expected_origin: ExpectedHostOrigin,
    pub host_boot: HostBootId,
    pub os_session: OsSessionId,
    pub limits: ProtocolLimits,
    pub max_video_credit_bytes: u64,
    pub max_video_credit_frames: u64,
}

impl BrowserIngressConfig {
    /// Standard configuration for a tailnet host FQDN and HTTPS/WSS port.
    #[must_use]
    pub fn https_tailnet(
        fqdn: &str,
        port: u16,
        host_boot: HostBootId,
        os_session: OsSessionId,
        limits: ProtocolLimits,
    ) -> Self {
        Self {
            expected_origin: ExpectedHostOrigin::https_tailnet(fqdn, port),
            host_boot,
            os_session,
            limits,
            max_video_credit_bytes: DEFAULT_VIDEO_CREDIT_BYTES,
            max_video_credit_frames: DEFAULT_VIDEO_CREDIT_FRAMES,
        }
    }
}

/// Typed errors surfaced by browser ingress operations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BrowserIngressError {
    SessionCapacityExceeded,
    SessionNotFound,
    EntropyUnavailable,
    LockPoisoned,
    Wss(WssError),
    OutboundQueueFull,
}

impl fmt::Display for BrowserIngressError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SessionCapacityExceeded => f.write_str("browser session capacity exceeded"),
            Self::SessionNotFound => f.write_str("browser session not found"),
            Self::EntropyUnavailable => f.write_str("CSPRNG entropy unavailable"),
            Self::LockPoisoned => f.write_str("browser ingress state lock poisoned"),
            Self::Wss(e) => write!(f, "WSS channel error: {e:?}"),
            Self::OutboundQueueFull => f.write_str("bounded outbound video queue full"),
        }
    }
}

impl std::error::Error for BrowserIngressError {}

/// Observable state of one authenticated browser session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BrowserSessionMetrics {
    pub session_id: RemoteSessionId,
    pub role: BrowserSessionRole,
    pub video_generation: ChannelGeneration,
    pub video_attached: bool,
    pub audio_attached: bool,
    pub outstanding_video_bytes: u64,
    pub outstanding_video_frames: u64,
    pub credit_epoch: u64,
    pub transport_label: &'static str,
}

#[derive(Debug)]
struct ActiveBrowserSession {
    peer_ip: IpAddr,
    role: BrowserSessionRole,
    control_binding: ControlBinding,
    video_channel_binding: u32,
    credit_epoch: u64,
    video_supervisor: ChannelSupervisor,
    video_credit: ReceiverCreditWindow,
    video_attached: bool,
    audio_attached: bool,
    outbound_video: VecDeque<Vec<u8>>,
    profile: WssProfile,
}

/// Shared, thread-safe state owned by the `frd` browser ingress listener.
#[derive(Debug)]
pub struct BrowserIngress {
    config: BrowserIngressConfig,
    policy: BrowserSecurityPolicy,
    sessions: Mutex<HashMap<RemoteSessionId, ActiveBrowserSession>>,
}

impl BrowserIngress {
    /// Construct a new browser ingress policy & session coordinator.
    #[must_use]
    pub fn new(config: BrowserIngressConfig) -> Self {
        let policy = BrowserSecurityPolicy::new(config.expected_origin.clone());
        Self {
            config,
            policy,
            sessions: Mutex::new(HashMap::new()),
        }
    }

    /// Reference to the underlying [`BrowserSecurityPolicy`].
    #[must_use]
    pub const fn policy(&self) -> &BrowserSecurityPolicy {
        &self.policy
    }

    /// Honest WSS degraded transport profile used by this ingress.
    #[must_use]
    pub const fn transport_profile(&self) -> WssProfile {
        WssProfile::new(false)
    }

    /// Inspect metrics for an authenticated browser session.
    pub fn session_metrics(
        &self,
        session_id: RemoteSessionId,
    ) -> Result<Option<BrowserSessionMetrics>, BrowserIngressError> {
        let guard = self
            .sessions
            .lock()
            .map_err(|_| BrowserIngressError::LockPoisoned)?;
        Ok(guard.get(&session_id).map(|s| BrowserSessionMetrics {
            session_id,
            role: s.role,
            video_generation: s.video_supervisor.active_generation(),
            video_attached: s.video_attached,
            audio_attached: s.audio_attached,
            outstanding_video_bytes: s.video_credit.outstanding_bytes(),
            outstanding_video_frames: s.video_credit.outstanding_frames(),
            credit_epoch: s.credit_epoch,
            transport_label: s.profile.diagnostic_label(),
        }))
    }

    /// Reserve receiver credit on `session_id`'s video channel and enqueue one binary
    /// video frame ONLY if `channel_gen` matches the active generation and receiver credit allows it.
    pub fn enqueue_video_frame(
        &self,
        session_id: RemoteSessionId,
        channel_gen: ChannelGeneration,
        payload: Vec<u8>,
    ) -> Result<(), BrowserIngressError> {
        let byte_len = u64::try_from(payload.len())
            .map_err(|_| BrowserIngressError::Wss(WssError::InvalidCreditCharge))?;
        let mut guard = self
            .sessions
            .lock()
            .map_err(|_| BrowserIngressError::LockPoisoned)?;
        let session = guard
            .get_mut(&session_id)
            .ok_or(BrowserIngressError::SessionNotFound)?;

        session
            .video_supervisor
            .check_admission(channel_gen)
            .map_err(BrowserIngressError::Wss)?;
        if session.outbound_video.len() >= MAX_OUTBOUND_VIDEO_QUEUE {
            return Err(BrowserIngressError::OutboundQueueFull);
        }
        session
            .video_credit
            .try_reserve(byte_len)
            .map_err(BrowserIngressError::Wss)?;
        session.outbound_video.push_back(payload);
        Ok(())
    }

    /// Pop the next credit-reserved video frame for an attached `/ws/video` stream.
    pub fn pop_outbound_video_frame(
        &self,
        session_id: RemoteSessionId,
        channel_gen: ChannelGeneration,
    ) -> Result<Option<Vec<u8>>, BrowserIngressError> {
        let mut guard = self
            .sessions
            .lock()
            .map_err(|_| BrowserIngressError::LockPoisoned)?;
        let session = guard
            .get_mut(&session_id)
            .ok_or(BrowserIngressError::SessionNotFound)?;
        session
            .video_supervisor
            .check_admission(channel_gen)
            .map_err(BrowserIngressError::Wss)?;
        Ok(session.outbound_video.pop_front())
    }

    /// Replace the active video WSS channel generation, immediately fencing the old one.
    pub fn replace_video_channel(
        &self,
        session_id: RemoteSessionId,
    ) -> Result<ChannelGeneration, BrowserIngressError> {
        let mut guard = self
            .sessions
            .lock()
            .map_err(|_| BrowserIngressError::LockPoisoned)?;
        let session = guard
            .get_mut(&session_id)
            .ok_or(BrowserIngressError::SessionNotFound)?;
        session.video_attached = false;
        session.outbound_video.clear();
        session
            .video_supervisor
            .replace_channel()
            .map_err(BrowserIngressError::Wss)
    }

    /// Issue an additional single-use auxiliary ticket for an authenticated browser session.
    pub fn issue_auxiliary_ticket_for_session(
        &self,
        session_id: RemoteSessionId,
        role: AuxiliaryChannelRole,
        now: HostInstant,
    ) -> Result<[u8; 32], BrowserIngressError> {
        let peer_ip = {
            let guard = self
                .sessions
                .lock()
                .map_err(|_| BrowserIngressError::LockPoisoned)?;
            guard
                .get(&session_id)
                .ok_or(BrowserIngressError::SessionNotFound)?
                .peer_ip
        };
        let secret = self
            .policy
            .issue_auxiliary_ticket(session_id, role, peer_ip, now)
            .map_err(|_| BrowserIngressError::EntropyUnavailable)?;
        Ok(*secret.as_bytes())
    }

    /// Accept one TLS-wrapped TCP connection, extract real TLS SNI, and serve HTTPS/WSS.
    pub async fn serve_tls_connection(
        &self,
        acceptor: &TlsAcceptor,
        tcp: TcpStream,
        peer_ip: IpAddr,
        now_fn: impl Fn() -> HostInstant,
    ) -> std::io::Result<()> {
        let tls = acceptor
            .accept(tcp)
            .await
            .map_err(|_| std::io::Error::other("TLS handshake failed"))?;
        let sni = tls.sni_hostname().map(str::to_owned);
        self.serve_stream(tls, peer_ip, sni.as_deref(), now_fn)
            .await
    }

    /// Serve one HTTP/1.1 or RFC 6455 WSS connection over any Asupersync stream.
    ///
    /// Production callers MUST pass a TLS stream via [`Self::serve_tls_connection`];
    /// passing a plain stream is for loopback tests and enforces identical origin,
    /// Host/SNI, Fetch Metadata, nonce, ticket, and credit checks.
    pub async fn serve_stream<S: AsyncRead + AsyncWrite + Unpin>(
        &self,
        mut stream: S,
        peer_ip: IpAddr,
        sni: Option<&str>,
        now_fn: impl Fn() -> HostInstant,
    ) -> std::io::Result<()> {
        let req = match http::read_http_request(&mut stream).await {
            Ok(req) => req,
            Err(http::HttpParseError::Eof) => return Ok(()),
            Err(err) => {
                let status = match err {
                    http::HttpParseError::HeadersTooLarge
                    | http::HttpParseError::TooManyHeaders => 431,
                    http::HttpParseError::BodyTooLarge => 413,
                    _ => 400,
                };
                let _ = http::write_http_response(
                    &mut stream,
                    status,
                    "Bad Request",
                    "application/json",
                    BROWSER_API_CSP,
                    b"{\"error\":\"malformed_http_request\"}",
                )
                .await;
                return Ok(());
            }
        };

        let metadata = FetchMetadata::new(
            req.sec_fetch_site.as_deref(),
            req.sec_fetch_mode.as_deref(),
            req.sec_fetch_dest.as_deref(),
        );

        match req.path() {
            "/api/session/bootstrap" => {
                self.handle_bootstrap(&mut stream, &req, &metadata, peer_ip, sni, now_fn())
                    .await
            }
            "/ws/control" => {
                self.handle_ws_control(&mut stream, &req, &metadata, peer_ip, sni, &now_fn)
                    .await
            }
            "/ws/video" => {
                self.handle_ws_auxiliary(
                    &mut stream,
                    &req,
                    &metadata,
                    peer_ip,
                    sni,
                    AuxiliaryChannelRole::VideoStream,
                    &now_fn,
                )
                .await
            }
            "/ws/audio" => {
                self.handle_ws_auxiliary(
                    &mut stream,
                    &req,
                    &metadata,
                    peer_ip,
                    sni,
                    AuxiliaryChannelRole::AudioDownlink,
                    &now_fn,
                )
                .await
            }
            _ => {
                self.handle_static_get(&mut stream, &req, &metadata, sni)
                    .await
            }
        }
    }

    async fn handle_static_get<S: AsyncWrite + Unpin>(
        &self,
        stream: &mut S,
        req: &http::HttpRequest,
        metadata: &FetchMetadata<'_>,
        sni: Option<&str>,
    ) -> std::io::Result<()> {
        if req.method != "GET" {
            return http::write_http_response(
                stream,
                405,
                "Method Not Allowed",
                "application/json",
                BROWSER_API_CSP,
                b"{\"error\":\"method_not_allowed\"}",
            )
            .await;
        }
        if self
            .policy
            .validate_navigation_get(&req.target, req.host.as_deref(), sni, metadata)
            .is_err()
        {
            return http::write_http_response(
                stream,
                403,
                "Forbidden",
                "application/json",
                BROWSER_API_CSP,
                b"{\"error\":\"origin_or_host_refused\"}",
            )
            .await;
        }

        match BrowserAssets::serve(req.path()) {
            Ok(asset) => {
                http::write_http_response(
                    stream,
                    asset.status_code,
                    "OK",
                    asset.content_type,
                    BROWSER_UI_CSP,
                    &asset.body,
                )
                .await
            }
            Err(_) => {
                http::write_http_response(
                    stream,
                    404,
                    "Not Found",
                    "application/json",
                    BROWSER_API_CSP,
                    b"{\"error\":\"not_found\"}",
                )
                .await
            }
        }
    }

    async fn handle_bootstrap<S: AsyncWrite + Unpin>(
        &self,
        stream: &mut S,
        req: &http::HttpRequest,
        metadata: &FetchMetadata<'_>,
        peer_ip: IpAddr,
        sni: Option<&str>,
        now: HostInstant,
    ) -> std::io::Result<()> {
        // State-changing bootstrap is strictly POST; navigation GET must never issue a nonce.
        if req.method != "POST" {
            return http::write_http_response(
                stream,
                405,
                "Method Not Allowed",
                "application/json",
                BROWSER_API_CSP,
                b"{\"error\":\"bootstrap_requires_post\"}",
            )
            .await;
        }

        let role = match parse_requested_role(&req.body) {
            Ok(role) => role,
            Err(()) => {
                return http::write_http_response(
                    stream,
                    400,
                    "Bad Request",
                    "application/json",
                    BROWSER_API_CSP,
                    b"{\"error\":\"invalid_bootstrap_role\"}",
                )
                .await;
            }
        };

        let Ok(session_id) = random_nonzero_session_id() else {
            return http::write_http_response(
                stream,
                503,
                "Service Unavailable",
                "application/json",
                BROWSER_API_CSP,
                b"{\"error\":\"entropy_unavailable\"}",
            )
            .await;
        };

        let boot_req = BootstrapRequest {
            uri: &req.target,
            host_header: req.host.as_deref(),
            origin_header: req.origin.as_deref(),
            sni,
            metadata,
            peer_ip,
            session_id,
            role,
            now,
        };

        let secret = match self.policy.handle_bootstrap_post(&boot_req) {
            Ok(secret) => secret,
            Err(_) => {
                return http::write_http_response(
                    stream,
                    403,
                    "Forbidden",
                    "application/json",
                    BROWSER_API_CSP,
                    b"{\"error\":\"bootstrap_origin_policy_refused\"}",
                )
                .await;
            }
        };

        let body = format!(
            "{{\"nonce_hex\":\"{}\",\"session_id_hex\":\"{:032x}\",\"host_boot_hex\":\"{:032x}\",\"os_session_hex\":\"{:032x}\",\"control_binding_id\":1,\"video_binding_id\":2,\"credit_epoch\":1,\"role\":\"{role}\",\"transport_profile\":\"{}\",\"expires_in_ms\":15000}}",
            hex_encode(secret.as_bytes()),
            session_id.as_raw(),
            self.config.host_boot.as_raw(),
            self.config.os_session.as_raw(),
            self.transport_profile().diagnostic_label(),
        );
        http::write_http_response(
            stream,
            200,
            "OK",
            "application/json",
            BROWSER_API_CSP,
            body.as_bytes(),
        )
        .await
    }

    async fn validate_ws_upgrade<S: AsyncWrite + Unpin>(
        &self,
        stream: &mut S,
        req: &http::HttpRequest,
        metadata: &FetchMetadata<'_>,
        sni: Option<&str>,
        role: WssChannelRole,
    ) -> std::io::Result<bool> {
        if req.method != "GET" || !req.is_websocket_upgrade() {
            http::write_http_response(
                stream,
                400,
                "Bad Request",
                "application/json",
                BROWSER_API_CSP,
                b"{\"error\":\"websocket_upgrade_required\"}",
            )
            .await?;
            return Ok(false);
        }

        // Reject bearer query strings, Host/SNI mismatches, non-exact Origins, and cross-site/iframe.
        let origin_ok = check_url_query_safety(&req.target).is_ok()
            && self
                .policy
                .expected_origin()
                .validate_host_authority(req.host.as_deref(), sni)
                .is_ok()
            && self
                .policy
                .expected_origin()
                .validate_origin(req.origin.as_deref())
                .is_ok()
            && metadata.validate_state_changing().is_ok();
        if !origin_ok {
            http::write_http_response(
                stream,
                403,
                "Forbidden",
                "application/json",
                BROWSER_API_CSP,
                b"{\"error\":\"websocket_origin_policy_refused\"}",
            )
            .await?;
            return Ok(false);
        }

        // Compression is strictly forbidden for WSS channels (`is_compression_allowed == false`).
        debug_assert!(!is_compression_allowed(role));
        let key = req.sec_websocket_key.as_deref().unwrap_or("");
        let accept = websocket::compute_accept_key(key);
        http::write_ws_upgrade_response(stream, &accept).await?;
        Ok(true)
    }

    async fn handle_ws_control<S: AsyncRead + AsyncWrite + Unpin>(
        &self,
        stream: &mut S,
        req: &http::HttpRequest,
        metadata: &FetchMetadata<'_>,
        peer_ip: IpAddr,
        sni: Option<&str>,
        now_fn: &impl Fn() -> HostInstant,
    ) -> std::io::Result<()> {
        if !self
            .validate_ws_upgrade(stream, req, metadata, sni, WssChannelRole::Control)
            .await?
        {
            return Ok(());
        }

        let mut guard = SocketAuthGuard::new(peer_ip, now_fn());
        let first_frame = match websocket::read_ws_frame(stream, MAX_AUTH_MESSAGE_BYTES, true).await
        {
            Ok(websocket::WsFrame::Binary(bytes)) => bytes,
            _ => {
                let _ = websocket::write_server_close(stream, 1008).await;
                return Ok(());
            }
        };

        // Parse optional 37th role byte (default: Observer; observation precedes control).
        let requested_role = if first_frame.len() == 37 && first_frame[36] == 1 {
            BrowserSessionRole::Controller
        } else {
            BrowserSessionRole::Observer
        };

        let auth_now = now_fn();
        let auth_result = self.policy.with_nonce_manager(|mgr| {
            guard.process_auth_message(&first_frame, requested_role, mgr, auth_now)
        });
        let Ok(Ok(session_id)) = auth_result else {
            let _ = websocket::write_server_close(stream, 1008).await;
            return Ok(());
        };
        if guard.check_media_or_input_allowed().is_err() {
            let _ = websocket::write_server_close(stream, 1008).await;
            return Ok(());
        }

        // Register bounded active browser session with ReceiverCreditWindow and ChannelSupervisor.
        let control_binding = ControlBinding {
            id: 1,
            host_boot: self.config.host_boot,
            os_session: self.config.os_session,
            remote_session: session_id,
        };
        let video_channel_binding = 2u32;
        let credit_epoch = 1u64;
        let Ok(credit_window) = ReceiverCreditWindow::new(
            credit_epoch,
            self.config.max_video_credit_bytes,
            self.config.max_video_credit_frames,
        ) else {
            let _ = websocket::write_server_close(stream, 1011).await;
            return Ok(());
        };

        {
            let Ok(mut sessions) = self.sessions.lock() else {
                let _ = websocket::write_server_close(stream, 1011).await;
                return Ok(());
            };
            if sessions.len() >= MAX_BROWSER_SESSIONS && !sessions.contains_key(&session_id) {
                let _ = websocket::write_server_close(stream, 1013).await;
                return Ok(());
            }
            sessions.insert(
                session_id,
                ActiveBrowserSession {
                    peer_ip,
                    role: requested_role,
                    control_binding,
                    video_channel_binding,
                    credit_epoch,
                    video_supervisor: ChannelSupervisor::new(WssChannelRole::Video, 4),
                    video_credit: credit_window,
                    video_attached: false,
                    audio_attached: false,
                    outbound_video: VecDeque::with_capacity(MAX_OUTBOUND_VIDEO_QUEUE),
                    profile: self.transport_profile(),
                },
            );
        }

        // Issue single-use role-specific VideoStream auxiliary ticket over authenticated /ws/control.
        let Ok(video_ticket) = self.policy.issue_auxiliary_ticket(
            session_id,
            AuxiliaryChannelRole::VideoStream,
            peer_ip,
            now_fn(),
        ) else {
            let _ = websocket::write_server_close(stream, 1011).await;
            return Ok(());
        };
        let mut grant_frame = [0u8; TICKET_GRANT_BYTES];
        grant_frame[..4].copy_from_slice(&TICKET_GRANT_MAGIC);
        grant_frame[4] = 1; // VideoStream
        grant_frame[5..21].copy_from_slice(&session_id.as_raw().to_be_bytes());
        grant_frame[21..53].copy_from_slice(video_ticket.as_bytes());
        websocket::write_server_binary(stream, &grant_frame).await?;

        // Bounded binary control loop: processes 0x0081 ReceiverPressure credit releases.
        let max_ctrl = usize::try_from(self.config.limits.max_control_message_bytes())
            .unwrap_or(64 * 1024)
            .min(64 * 1024);
        loop {
            match websocket::read_ws_frame(stream, max_ctrl, true).await {
                Ok(websocket::WsFrame::Binary(payload)) => {
                    if let Ok((decoded_binding, report)) = receiver_pressure::decode(
                        &payload,
                        &self.config.limits,
                        InputDirection::ViewerToHost,
                        InputDelivery::Reliable,
                    ) {
                        if decoded_binding != control_binding {
                            let _ = websocket::write_server_close(stream, 1008).await;
                            break;
                        }
                        let applied = {
                            let Ok(mut sessions) = self.sessions.lock() else {
                                let _ = websocket::write_server_close(stream, 1011).await;
                                break;
                            };
                            sessions.get_mut(&session_id).map(|s| {
                                s.video_credit.apply_wire_pressure(
                                    s.video_channel_binding,
                                    ChannelRole::Video,
                                    &report,
                                )
                            })
                        };
                        match applied {
                            Some(Ok(_)) => {
                                // Acknowledge credit applied with a 4-byte binary receipt "FRCR".
                                websocket::write_server_binary(stream, b"FRCR").await?;
                            }
                            _ => {
                                let _ = websocket::write_server_close(stream, 1008).await;
                                break;
                            }
                        }
                    } else {
                        let _ = websocket::write_server_close(stream, 1008).await;
                        break;
                    }
                }
                Ok(websocket::WsFrame::Ping(data)) => {
                    websocket::write_ws_frame(stream, 0x0A, &data, None).await?;
                }
                Ok(websocket::WsFrame::Pong(_)) => {}
                Ok(websocket::WsFrame::Close) | Err(websocket::WsFrameError::Eof) => break,
                Err(_) => {
                    let _ = websocket::write_server_close(stream, 1002).await;
                    break;
                }
            }
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn handle_ws_auxiliary<S: AsyncRead + AsyncWrite + Unpin>(
        &self,
        stream: &mut S,
        req: &http::HttpRequest,
        metadata: &FetchMetadata<'_>,
        peer_ip: IpAddr,
        sni: Option<&str>,
        expected_role: AuxiliaryChannelRole,
        now_fn: &impl Fn() -> HostInstant,
    ) -> std::io::Result<()> {
        let wss_role = match expected_role {
            AuxiliaryChannelRole::VideoStream => WssChannelRole::Video,
            AuxiliaryChannelRole::AudioDownlink | AuxiliaryChannelRole::AudioUplink => {
                WssChannelRole::Audio
            }
            _ => WssChannelRole::Control,
        };
        if !self
            .validate_ws_upgrade(stream, req, metadata, sni, wss_role)
            .await?
        {
            return Ok(());
        }

        // First binary message MUST be FRAT + 16B session_id + 1B role + 32B single-use ticket.
        let first_frame = match websocket::read_ws_frame(stream, MAX_AUTH_MESSAGE_BYTES, true).await
        {
            Ok(websocket::WsFrame::Binary(bytes)) => bytes,
            _ => {
                let _ = websocket::write_server_close(stream, 1008).await;
                return Ok(());
            }
        };

        let Some((session_id, wire_role, ticket)) = parse_attach_frame(&first_frame) else {
            let _ = websocket::write_server_close(stream, 1008).await;
            return Ok(());
        };
        if wire_role != expected_role {
            let _ = websocket::write_server_close(stream, 1008).await;
            return Ok(());
        }

        if self
            .policy
            .consume_auxiliary_ticket(Some(&ticket), session_id, expected_role, peer_ip, now_fn())
            .is_err()
        {
            let _ = websocket::write_server_close(stream, 1008).await;
            return Ok(());
        }

        let attached_gen = {
            let Ok(mut sessions) = self.sessions.lock() else {
                let _ = websocket::write_server_close(stream, 1011).await;
                return Ok(());
            };
            let Some(session) = sessions.get_mut(&session_id) else {
                let _ = websocket::write_server_close(stream, 1008).await;
                return Ok(());
            };
            match expected_role {
                AuxiliaryChannelRole::VideoStream => {
                    session.video_attached = true;
                    session.video_supervisor.active_generation()
                }
                AuxiliaryChannelRole::AudioDownlink => {
                    session.audio_attached = true;
                    session.video_supervisor.active_generation()
                }
                _ => session.video_supervisor.active_generation(),
            }
        };

        // Send a 12-byte attachment confirmation ("FROK" + 8-byte active generation BE).
        let mut ok_frame = [0u8; 12];
        ok_frame[..4].copy_from_slice(b"FROK");
        ok_frame[4..12].copy_from_slice(&attached_gen.0.to_be_bytes());
        websocket::write_server_binary(stream, &ok_frame).await?;

        // Drain any already-reserved outbound video frames for this generation, then wait for
        // client poll pings ("FRPL") or Close so callers can drive bounded credit delivery.
        let drain_queued = |ingress: &Self| -> Vec<Vec<u8>> {
            let mut frames = Vec::new();
            while let Ok(Some(frame)) = ingress.pop_outbound_video_frame(session_id, attached_gen) {
                frames.push(frame);
            }
            frames
        };

        for frame in drain_queued(self) {
            websocket::write_server_binary(stream, &frame).await?;
        }

        loop {
            match websocket::read_ws_frame(stream, 256, true).await {
                Ok(websocket::WsFrame::Binary(cmd)) if cmd.as_slice() == b"FRPL" => {
                    // Check channel generation hasn't been fenced before draining!
                    let frames = match self.pop_outbound_video_frame(session_id, attached_gen) {
                        Ok(Some(first)) => {
                            let mut batch = vec![first];
                            batch.extend(drain_queued(self));
                            batch
                        }
                        Ok(None) => Vec::new(),
                        Err(_) => {
                            let _ = websocket::write_server_close(stream, 1008).await;
                            break;
                        }
                    };
                    for frame in frames {
                        websocket::write_server_binary(stream, &frame).await?;
                    }
                }
                Ok(websocket::WsFrame::Ping(data)) => {
                    websocket::write_ws_frame(stream, 0x0A, &data, None).await?;
                }
                Ok(websocket::WsFrame::Close) | Err(websocket::WsFrameError::Eof) => break,
                _ => {
                    let _ = websocket::write_server_close(stream, 1008).await;
                    break;
                }
            }
        }
        Ok(())
    }
}

/// Bind a TCP listener on `addr` using Asupersync (for HTTPS/WSS browser ingress).
pub async fn bind_listener(addr: SocketAddr) -> std::io::Result<TcpListener> {
    TcpListener::bind(addr).await
}

/// Timeout budget matching [`FIRST_MESSAGE_AUTH_TIMEOUT`] as a [`Duration`].
#[must_use]
pub const fn first_message_auth_duration() -> Duration {
    Duration::from_micros(FIRST_MESSAGE_AUTH_TIMEOUT.as_micros())
}

/// Shared handle convenience alias.
pub type SharedBrowserIngress = Arc<BrowserIngress>;

fn parse_requested_role(body: &[u8]) -> Result<BrowserSessionRole, ()> {
    if body.is_empty() {
        return Ok(BrowserSessionRole::Observer);
    }
    let val: serde_json::Value = serde_json::from_slice(body).map_err(|_| ())?;
    match val.get("role").and_then(serde_json::Value::as_str) {
        None | Some("observer") => Ok(BrowserSessionRole::Observer),
        Some("controller") => Ok(BrowserSessionRole::Controller),
        Some(_) => Err(()),
    }
}

fn parse_attach_frame(bytes: &[u8]) -> Option<(RemoteSessionId, AuxiliaryChannelRole, [u8; 32])> {
    if bytes.len() != ATTACH_MESSAGE_BYTES || bytes[..4] != ATTACH_MAGIC {
        return None;
    }
    let mut sid_bytes = [0u8; 16];
    sid_bytes.copy_from_slice(&bytes[4..20]);
    let session_id = RemoteSessionId::from_raw(u128::from_be_bytes(sid_bytes));
    if session_id.as_raw() == 0 {
        return None;
    }
    let role = match bytes[20] {
        1 => AuxiliaryChannelRole::VideoStream,
        2 => AuxiliaryChannelRole::AudioDownlink,
        3 => AuxiliaryChannelRole::AudioUplink,
        4 => AuxiliaryChannelRole::ClipboardSync,
        5 => AuxiliaryChannelRole::FilesTransfer,
        6 => AuxiliaryChannelRole::DiagnosticsStream,
        _ => return None,
    };
    let mut ticket = [0u8; 32];
    ticket.copy_from_slice(&bytes[21..53]);
    Some((session_id, role, ticket))
}

fn random_nonzero_session_id() -> Result<RemoteSessionId, ()> {
    for _ in 0..4 {
        let mut bytes = [0u8; 16];
        getrandom::fill(&mut bytes).map_err(|_| ())?;
        let raw = u128::from_be_bytes(bytes);
        if raw != 0 {
            return Ok(RemoteSessionId::from_raw(raw));
        }
    }
    Err(())
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(char::from(HEX[usize::from(b >> 4)]));
        out.push(char::from(HEX[usize::from(b & 0x0F)]));
    }
    out
}
