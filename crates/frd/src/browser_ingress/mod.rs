#![forbid(unsafe_code)]
//! Asupersync HTTPS/WSS browser workstation ingress (`frd`).
//!
//! Serves self-hosted assets and optional approved view-only desktop sessions.
//! An unregistered ingress remains explicitly unavailable. HTTP bootstrap and
//! authenticated WebSockets never create desktop authority on their own.
//! Wires together the existing security and transport layers:
//! - [`crate::broker::browser_security::BrowserSecurityPolicy`] (exact `Origin`,
//!   `Host`/SNI DNS-rebinding defense, Fetch Metadata, URL query safety,
//!   CSPRNG HMAC-bound single-use bootstrap nonces & role-specific auxiliary tickets,
//!   bounded canonical first-message authentication).
//! - [`crate::broker::browser_assets::BrowserAssets`] (self-hosted `web/` shell,
//!   strict CSP, zero CDN, zero authority on `GET`).
//! - [`fr_transport::wss`] (`ReceiverCreditWindow`, `ChannelSupervisor`,
//!   `WssProfile` honest TCP head-of-line degradation labeling, no compression).
//!
//! Pure safe Rust (`#![forbid(unsafe_code)]`), Asupersync-only (no Tokio, no
//! third-party web server frameworks).

#[cfg(target_os = "linux")]
pub mod desktop;
pub mod http;
pub mod websocket;

use crate::broker::{
    browser_assets::BrowserAssets,
    browser_security::{
        AuxiliaryChannelRole, BROWSER_API_CSP, BROWSER_UI_CSP, BootstrapRequest,
        BrowserSecurityPolicy, BrowserSessionRole, ExpectedHostOrigin, FIRST_MESSAGE_AUTH_TIMEOUT,
        FetchMetadata, check_url_query_safety,
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
    WireError,
    input::InputDelivery,
    negotiation::{self, Role, browser},
    refusal::{self, Reason, Refused},
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
    #[cfg(target_os = "linux")]
    desktop: Option<desktop::Service>,
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
            #[cfg(target_os = "linux")]
            desktop: None,
        }
    }

    /// Register the installed host's real admission, local consent and capture
    /// factory. The caller retains its protected listener and original guards.
    /// The first profile is one Observe session with one dedicated media socket.
    #[must_use]
    #[cfg(target_os = "linux")]
    pub fn with_desktop(mut self, factory: desktop::Factory) -> Self {
        self.config.limits = self.config.limits.negotiated(&browser::limits());
        self.desktop = Some(desktop::Service::new(factory));
        self
    }

    /// Fence desktop delivery synchronously, before aborting TCP task owners.
    #[cfg(target_os = "linux")]
    pub fn close_desktop(&self) {
        if let Some(desktop) = &self.desktop {
            desktop.close();
        }
    }

    /// A closed original source whose receipt needs listener-owned retirement.
    /// The listener keeps driving all socket owners while this cleanup runs.
    #[cfg(target_os = "linux")]
    pub fn has_closed_desktop(&self) -> bool {
        self.desktop
            .as_ref()
            .is_some_and(desktop::Service::has_closed)
    }

    /// Retire only a closed exact original session. Unlike final shutdown,
    /// this leaves admission open after confirmed exit for the next viewer.
    #[cfg(target_os = "linux")]
    pub async fn retire_closed_desktop(&self, cleanup: &asupersync::cx::Cx) -> std::io::Result<()> {
        if let Some(desktop) = &self.desktop {
            desktop.retire_closed(cleanup).await?;
        }
        Ok(())
    }

    /// Drain the retained ORIGINAL child after both connection owners ended.
    /// Failure retains custody and keeps the single session slot occupied.
    #[cfg(target_os = "linux")]
    pub async fn retire_desktop(&self, cleanup: &asupersync::cx::Cx) -> std::io::Result<()> {
        if let Some(desktop) = &self.desktop {
            desktop.retire(cleanup).await?;
        }
        Ok(())
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
        #[cfg(target_os = "linux")]
        if let Some(metrics) = self
            .desktop
            .as_ref()
            .and_then(|desktop| desktop.metrics(session_id))
        {
            return Ok(Some(metrics));
        }
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

    /// Legacy generic TLS entry. It does not carry installed-daemon authority;
    /// only serve_verified_stream can activate a registered desktop factory.
    pub async fn serve_tls_connection(
        &self,
        acceptor: &TlsAcceptor,
        tcp: TcpStream,
        peer_ip: IpAddr,
        now_fn: impl Fn() -> HostInstant,
    ) -> std::io::Result<()> {
        let tls = bounded_startup(self.startup_budget(), async {
            acceptor
                .accept(tcp)
                .await
                .map_err(|_| std::io::Error::other("TLS handshake failed"))
        })
        .await?;
        let sni = tls.sni_hostname().map(str::to_owned);
        self.serve_stream_inner(tls, peer_ip, sni.as_deref(), now_fn, None)
            .await
    }

    /// Serve an unavailable/test ingress without claiming verified peer identity.
    /// Installing a desktop factory alone cannot turn a plain stream into one.
    pub async fn serve_stream<S: AsyncRead + AsyncWrite + Unpin>(
        &self,
        stream: S,
        peer_ip: IpAddr,
        sni: Option<&str>,
        now_fn: impl Fn() -> HostInstant,
    ) -> std::io::Result<()> {
        self.serve_stream_inner(stream, peer_ip, sni, now_fn, None)
            .await
    }

    /// Serve the original accepted HTTPS connection AFTER the installed
    /// NativeServerIdentity verified TLS and LocalAPI authenticated its exact
    /// endpoints. The caller retains and refreshes this original Admission.
    ///
    /// Every resumed transport poll checks that socket lease. The desktop adds
    /// original observation and local-supervisor gates, including on media.
    #[cfg(target_os = "linux")]
    pub async fn serve_verified_stream<S: AsyncRead + AsyncWrite + Unpin>(
        &self,
        stream: S,
        peer: fr_tailnet::Lease,
        cx: asupersync::cx::Cx,
        sni: Option<&str>,
        now_fn: impl Fn() -> HostInstant,
    ) -> std::io::Result<()> {
        peer.observe()
            .map_err(|_| std::io::Error::other("browser peer unavailable"))?;
        let peer_ip = peer.addresses().peer.ip();
        let stream = desktop::GuardedIo::ingress(stream, peer.clone(), cx.clone());
        self.serve_stream_inner(stream, peer_ip, sni, now_fn, Some((peer, cx)))
            .await
    }

    fn startup_budget(&self) -> Duration {
        Duration::from_millis(u64::from(self.config.limits.max_handshake_duration_ms()))
    }

    async fn serve_stream_inner<S: AsyncRead + AsyncWrite + Unpin>(
        &self,
        mut stream: S,
        peer_ip: IpAddr,
        sni: Option<&str>,
        now_fn: impl Fn() -> HostInstant,
        verified: Option<(fr_tailnet::Lease, asupersync::cx::Cx)>,
    ) -> std::io::Result<()> {
        // The HTTP/authentication budget ends before any live desktop owner is
        // created. Never wrap capture or its cleanup in a future-drop timeout.
        let parsed = bounded_startup(self.startup_budget(), async {
            Ok(http::read_http_request(&mut stream).await)
        })
        .await?;
        let req = match parsed {
            Ok(req) => req,
            Err(http::HttpParseError::Eof) => return Ok(()),
            Err(err) => {
                let status = match err {
                    http::HttpParseError::HeadersTooLarge
                    | http::HttpParseError::TooManyHeaders => 431,
                    http::HttpParseError::BodyTooLarge => 413,
                    _ => 400,
                };
                return bounded_startup(
                    self.startup_budget(),
                    http::write_http_response(
                        &mut stream,
                        status,
                        "Bad Request",
                        "application/json",
                        BROWSER_API_CSP,
                        b"{\"error\":\"malformed_http_request\"}",
                    ),
                )
                .await;
            }
        };
        let metadata = FetchMetadata::new(
            req.sec_fetch_site.as_deref(),
            req.sec_fetch_mode.as_deref(),
            req.sec_fetch_dest.as_deref(),
        );
        match req.path() {
            "/api/session/bootstrap" => {
                bounded_startup(
                    self.startup_budget(),
                    self.handle_bootstrap(
                        &mut stream,
                        &req,
                        &metadata,
                        sni,
                        verified.as_ref().map(|(peer, _)| peer),
                        now_fn(),
                    ),
                )
                .await
            }
            "/ws/control" => {
                #[cfg(target_os = "linux")]
                if self.desktop.is_some()
                    && let Some((peer, cx)) = verified
                {
                    return desktop::serve_control(
                        self,
                        &mut stream,
                        &req,
                        &metadata,
                        peer,
                        cx,
                        sni,
                    )
                    .await;
                }
                bounded_startup(
                    first_message_auth_duration().min(self.startup_budget()),
                    self.handle_ws_control(&mut stream, &req, &metadata, peer_ip, sni, &now_fn),
                )
                .await
            }
            "/ws/video" => {
                #[cfg(target_os = "linux")]
                if self.desktop.is_some()
                    && let Some((peer, _)) = verified
                {
                    return desktop::serve_video(self, &mut stream, &req, &metadata, peer, sni)
                        .await;
                }
                bounded_startup(
                    first_message_auth_duration().min(self.startup_budget()),
                    self.handle_ws_auxiliary(
                        &mut stream,
                        &req,
                        &metadata,
                        sni,
                        WssChannelRole::Video,
                    ),
                )
                .await
            }
            "/ws/audio" => {
                bounded_startup(
                    first_message_auth_duration().min(self.startup_budget()),
                    self.handle_ws_auxiliary(
                        &mut stream,
                        &req,
                        &metadata,
                        sni,
                        WssChannelRole::Audio,
                    ),
                )
                .await
            }
            _ => {
                bounded_startup(
                    self.startup_budget(),
                    self.handle_static_get(&mut stream, &req, &metadata, sni),
                )
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
                    if asset.status_code == 503 {
                        "Service Unavailable"
                    } else {
                        "OK"
                    },
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
        sni: Option<&str>,
        verified: Option<&fr_tailnet::Lease>,
        now: HostInstant,
    ) -> std::io::Result<()> {
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
        // Preserve all origin checks even for an unavailable capability. No
        // identity, approval, nonce or session is inferred from this request.
        if !self.state_changing_allowed(req, metadata, sni) {
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
        let Ok(role) = parse_requested_role(&req.body) else {
            return http::write_http_response(
                stream,
                400,
                "Bad Request",
                "application/json",
                BROWSER_API_CSP,
                b"{\"error\":\"invalid_bootstrap_role\"}",
            )
            .await;
        };

        // A nonce is only transport bootstrap. The installed host factory
        // still performs local observation admission before SessionOpened or
        // creating a capture child. No query string ever carries this secret.
        #[cfg(target_os = "linux")]
        let available = role == BrowserSessionRole::Observer
            && verified.is_some()
            && self
                .desktop
                .as_ref()
                .is_some_and(desktop::Service::available);
        #[cfg(target_os = "linux")]
        let body = if available {
            let peer = verified.ok_or_else(|| std::io::Error::other("missing browser peer"))?;
            peer.observe()
                .map_err(|_| std::io::Error::other("browser peer unavailable"))?;
            let secret = self
                .policy
                .handle_bootstrap_post(&BootstrapRequest {
                    uri: &req.target,
                    host_header: req.host.as_deref(),
                    origin_header: req.origin.as_deref(),
                    sni,
                    metadata,
                    peer_ip: peer.addresses().peer.ip(),
                    session_id: RemoteSessionId::from_raw(desktop::random_id()?),
                    role,
                    now,
                })
                .map_err(|_| std::io::Error::other("browser bootstrap unavailable"))?;
            const HEX: &[u8; 16] = b"0123456789abcdef";
            let mut nonce = String::with_capacity(64);
            for byte in secret.as_bytes() {
                nonce.push(char::from(HEX[usize::from(byte >> 4)]));
                nonce.push(char::from(HEX[usize::from(byte & 15)]));
            }
            format!(
                "{{\"transport_profile\":\"{}\",\"profile_id\":{},\"profile_version\":{},\"role\":\"{role}\",\"desktop_available\":true,\"nonce_hex\":\"{nonce}\",\"expires_in_ms\":15000}}",
                self.transport_profile().diagnostic_label(),
                browser::WSS_PROFILE,
                browser::PROFILE_VERSION,
            )
        } else {
            format!(
                "{{\"transport_profile\":\"{}\",\"profile_id\":{},\"profile_version\":{},\"role\":\"{role}\",\"desktop_available\":false,\"refusal_code\":\"browser_session_unavailable\"}}",
                self.transport_profile().diagnostic_label(),
                browser::WSS_PROFILE,
                browser::PROFILE_VERSION,
            )
        };
        #[cfg(not(target_os = "linux"))]
        let body = format!(
            "{{\"transport_profile\":\"{}\",\"profile_id\":{},\"profile_version\":{},\"role\":\"{role}\",\"desktop_available\":false,\"refusal_code\":\"browser_session_unavailable\"}}",
            self.transport_profile().diagnostic_label(),
            browser::WSS_PROFILE,
            browser::PROFILE_VERSION,
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

    fn state_changing_allowed(
        &self,
        req: &http::HttpRequest,
        metadata: &FetchMetadata<'_>,
        sni: Option<&str>,
    ) -> bool {
        check_url_query_safety(&req.target).is_ok()
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
            && metadata.validate_state_changing().is_ok()
    }

    async fn validate_ws_upgrade<S: AsyncWrite + Unpin>(
        &self,
        stream: &mut S,
        req: &http::HttpRequest,
        metadata: &FetchMetadata<'_>,
        sni: Option<&str>,
        role: WssChannelRole,
    ) -> std::io::Result<bool> {
        // Reject bearer query strings, Host/SNI mismatches, non-exact Origins, and cross-site/iframe.
        let origin_ok = self.state_changing_allowed(req, metadata, sni);
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
        let started = now_fn();
        if !self
            .validate_ws_upgrade(stream, req, metadata, sni, WssChannelRole::Control)
            .await?
        {
            return Ok(());
        }
        let maximum = browser::MAX_RECORD
            .min(usize::try_from(self.config.limits.max_control_message_bytes()).unwrap_or(0));
        let Ok(websocket::WsFrame::Binary(first_frame)) =
            websocket::read_ws_frame(stream, maximum, true).await
        else {
            return websocket::write_server_close(stream, 1008).await;
        };
        // Terminal refusal never begins another negotiation or refusal exchange.
        if refusal::decode(&first_frame, 0, maximum, InputDelivery::Reliable).is_ok() {
            return websocket::write_server_close(stream, 1008).await;
        }
        let now = now_fn();
        let reason = if now < started
            || started
                .checked_add(FIRST_MESSAGE_AUTH_TIMEOUT)
                .is_none_or(|deadline| now >= deadline)
        {
            Reason::Expired
        } else {
            self.hello_refusal(&first_frame, peer_ip, now, maximum)
        };
        let mut bytes = [0; refusal::MAX_BYTES];
        let length = refusal::encode(
            Refused::connection(reason),
            0,
            maximum,
            &mut bytes,
            InputDelivery::Reliable,
        )
        .map_err(|_| std::io::Error::other("browser refusal encoding failed"))?;
        websocket::write_server_binary(stream, &bytes[..length]).await?;
        websocket::write_server_close(stream, 1008).await
    }

    fn hello_refusal(
        &self,
        bytes: &[u8],
        peer_ip: IpAddr,
        now: HostInstant,
        maximum: usize,
    ) -> Reason {
        let hello = match browser::decode_hello(bytes, maximum) {
            Ok(hello) => hello,
            Err(negotiation::Error::Profile) => return Reason::UnsupportedProfile,
            Err(
                negotiation::Error::Version
                | negotiation::Error::Wire(WireError::UnsupportedVersion),
            ) => return Reason::UnsupportedVersion,
            Err(negotiation::Error::Limits) => return Reason::InvalidLimits,
            Err(
                negotiation::Error::Allocation
                | negotiation::Error::Wire(WireError::ResourceLimit | WireError::ArithmeticOverflow),
            ) => return Reason::ResourceLimit,
            Err(_) => return Reason::InvalidMessage,
        };
        let role = match hello.offer.role {
            Role::Observe => BrowserSessionRole::Observer,
            Role::RequestControl => BrowserSessionRole::Controller,
        };
        // Only the exact peer/role-bound nonce can be consumed, once. Transport
        // authentication is not a host observation grant or a SessionOpened.
        if !matches!(
            self.policy.with_nonce_manager(|manager| {
                manager.consume_nonce(hello.nonce(), peer_ip, role, now)
            }),
            Ok(Ok(_))
        ) {
            return Reason::PermissionDenied;
        }
        if !hello.offer.versions.contains(&0) {
            return Reason::UnsupportedVersion;
        }
        let supported = browser::offer(browser::limits());
        if hello.offer.capabilities.iter().any(|cap| {
            cap.required
                && !supported
                    .capabilities
                    .iter()
                    .any(|host| host.name == cap.name && host.version == cap.version)
        }) {
            return Reason::RequiredCapability;
        }
        Reason::HostUnavailable
    }

    async fn handle_ws_auxiliary<S: AsyncRead + AsyncWrite + Unpin>(
        &self,
        stream: &mut S,
        req: &http::HttpRequest,
        metadata: &FetchMetadata<'_>,
        sni: Option<&str>,
        role: WssChannelRole,
    ) -> std::io::Result<()> {
        if self
            .validate_ws_upgrade(stream, req, metadata, sni, role)
            .await?
        {
            // No production observation session exists, so no auxiliary
            // channel can attach. Do not consume a ticket or wait for payload.
            websocket::write_server_close(stream, 1008).await?;
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
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Request<'a> {
        role: &'a str,
    }
    let request: Request<'_> = serde_json::from_slice(body).map_err(|_| ())?;
    match request.role {
        "observer" => Ok(BrowserSessionRole::Observer),
        "controller" => Ok(BrowserSessionRole::Controller),
        _ => Err(()),
    }
}

/// Startup has no workers or admitted authority. On expiry the sole owning
/// connection is dropped; a partial reply cannot be retried on a successor.
async fn bounded_startup<T>(
    budget: Duration,
    operation: impl core::future::Future<Output = std::io::Result<T>>,
) -> std::io::Result<T> {
    let cx = asupersync::cx::Cx::current()
        .ok_or_else(|| std::io::Error::other("browser ingress requires runtime context"))?;
    asupersync::time::timeout(cx.now(), budget, operation)
        .await
        .map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "browser startup deadline expired",
            )
        })?
}
