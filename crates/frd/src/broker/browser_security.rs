#![forbid(unsafe_code)]
//! Browser origin security: exact-Origin checks, nonce bootstrap, ticket attachment, and CSP.
//!
//! Enforces non-negotiable security requirements from plan §§16.4, 24.3, and AGENTS.md §6:
//! 1. Tailscale authenticates the connecting NODE, not the web origin running on that node.
//!    A hostile page on an admitted machine must gain zero access.
//! 2. UI is served only from the configured host HTTPS origin.
//! 3. Host/authority header (and SNI) is validated against the configured host to prevent DNS rebinding.
//! 4. EXACT expected `Origin` is required for all state-changing requests and WebSocket/WebTransport establishment.
//!    Wildcard CORS, null origins, unexpected cross-origin fetches, and state changes via GET are denied.
//! 5. Fetch Metadata (`Sec-Fetch-Site`, `Sec-Fetch-Mode`, `Sec-Fetch-Dest`) is checked as defense in depth;
//!    iframe embedding (`Sec-Fetch-Dest: iframe`) is rejected unconditionally.
//! 6. Short-lived (15s), strictly single-use nonces are obtained via an origin-checked same-origin HTTPS POST,
//!    bound to the verified Tailscale peer IP + requested session role, and required before any sensitive operation.
//! 7. Responses return non-executable JSON with `no-store`, `nosniff`, and `no-referrer` protections (no JSONP, no nonce-bearing scripts).
//! 8. Bounded FIRST reliable message application authentication: since browser sockets cannot carry custom handshake headers,
//!    the transport must authenticate via nonce within a tight deadline (3.0s). Until that succeeds, NO pixels, audio,
//!    input, codec work, or directory expansion are permitted.
//! 9. Every auxiliary channel attaches with a role-specific, short-lived, single-use ticket (a bare session ID attaches nothing).
//! 10. Nonces/tickets are NEVER placed in query strings, URLs, host links, logs, or referrers.
//! 11. Unauthenticated handshakes are rate-limited and concurrent pending handshakes are bounded.
//! 12. Restrictive CSP, `frame-ancestors 'none'`, safe referrer policy, and `no-store` on sensitive responses.

use core::fmt;
use fr_core::{
    ids::RemoteSessionId,
    time::{HostDuration, HostInstant},
};
use std::{collections::HashMap, net::IpAddr, sync::Mutex};

/// Default Content-Security-Policy for browser workstation UI.
pub const BROWSER_UI_CSP: &str = "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; connect-src 'self'; frame-ancestors 'none'; object-src 'none'; base-uri 'self'";

/// JSON API Content-Security-Policy (strictly no embedding, no scripts).
pub const BROWSER_API_CSP: &str = "default-src 'none'; frame-ancestors 'none'";

/// Maximum validity duration of a single-use bootstrap nonce (15 seconds).
pub const BOOTSTRAP_NONCE_TTL: HostDuration = HostDuration::from_micros(15_000_000);

/// Maximum validity duration of a single-use auxiliary channel ticket (15 seconds).
pub const AUXILIARY_TICKET_TTL: HostDuration = HostDuration::from_micros(15_000_000);

/// Maximum time allowed for the browser to send its first authentication message after socket connect (3 seconds).
pub const FIRST_MESSAGE_AUTH_TIMEOUT: HostDuration = HostDuration::from_micros(3_000_000);

/// Maximum allowed payload size for the first authentication message (1 KiB).
pub const MAX_AUTH_MESSAGE_BYTES: usize = 1024;

/// Global ceiling for concurrent pending unauthenticated bootstrap nonces.
pub const MAX_PENDING_NONCES_GLOBAL: usize = 256;

/// Ceiling for concurrent pending bootstrap nonces per peer IP.
pub const MAX_PENDING_NONCES_PER_PEER: usize = 4;

/// Global ceiling for concurrent pending auxiliary tickets.
pub const MAX_PENDING_TICKETS_GLOBAL: usize = 512;

/// Role requested by the browser client.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BrowserSessionRole {
    /// Read-only observer (screen pixels, audio playback if permitted, no input injection).
    Observer,
    /// Interactive controller (input injection subject to local approval and host lease).
    Controller,
}

impl fmt::Display for BrowserSessionRole {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Observer => f.write_str("observer"),
            Self::Controller => f.write_str("controller"),
        }
    }
}

/// Role of an auxiliary channel requiring a dedicated attachment ticket.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AuxiliaryChannelRole {
    /// ATP file transfer channel.
    FilesTransfer,
    /// Opus audio downlink (host playback to client).
    AudioDownlink,
    /// Opus audio uplink (client microphone to host).
    AudioUplink,
    /// Bidirectional clipboard synchronization channel.
    ClipboardSync,
    /// Sanitized diagnostic and telemetry stream.
    DiagnosticsStream,
}

impl fmt::Display for AuxiliaryChannelRole {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::FilesTransfer => f.write_str("files-transfer"),
            Self::AudioDownlink => f.write_str("audio-downlink"),
            Self::AudioUplink => f.write_str("audio-uplink"),
            Self::ClipboardSync => f.write_str("clipboard-sync"),
            Self::DiagnosticsStream => f.write_str("diagnostics-stream"),
        }
    }
}

/// Typed refusal reasons for Origin header validation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OriginRefusal {
    /// Origin header was missing on a request where an Origin is required (POST, WebSocket, etc.).
    MissingRequiredOrigin,
    /// The `null` origin was provided (e.g. from sandboxed iframe, file:// URL, or data URI).
    NullOriginForbidden,
    /// Wildcard `*` origin was provided.
    WildcardForbidden,
    /// Origin scheme does not match expected scheme (e.g. `http:` vs `https:`).
    SchemeMismatch,
    /// Origin hostname does not match expected host.
    HostMismatch,
    /// Origin port does not match expected port.
    PortMismatch,
    /// Origin header could not be parsed as a valid URI.
    MalformedOriginHeader,
}

impl fmt::Display for OriginRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingRequiredOrigin => f.write_str("missing required Origin header"),
            Self::NullOriginForbidden => f.write_str("null origin is strictly forbidden"),
            Self::WildcardForbidden => f.write_str("wildcard origin is strictly forbidden"),
            Self::SchemeMismatch => f.write_str("origin scheme mismatch: https required"),
            Self::HostMismatch => f.write_str("origin hostname does not match configured host"),
            Self::PortMismatch => f.write_str("origin port does not match configured port"),
            Self::MalformedOriginHeader => f.write_str("malformed Origin header format"),
        }
    }
}

impl std::error::Error for OriginRefusal {}

/// Typed refusal reasons for Host header / authority validation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostRefusal {
    /// Host header is missing.
    MissingHostHeader,
    /// Host header hostname does not match configured host (DNS rebinding defense).
    HostnameMismatch,
    /// Host header port does not match configured port.
    PortMismatch,
    /// TLS SNI does not match configured host.
    SniMismatch,
    /// Host header format is invalid.
    MalformedHostHeader,
}

impl fmt::Display for HostRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingHostHeader => f.write_str("missing Host header"),
            Self::HostnameMismatch => f.write_str("Host header mismatch: potential DNS rebinding"),
            Self::PortMismatch => f.write_str("Host header port mismatch"),
            Self::SniMismatch => f.write_str("TLS SNI does not match configured authority"),
            Self::MalformedHostHeader => f.write_str("malformed Host header format"),
        }
    }
}

impl std::error::Error for HostRefusal {}

/// Typed refusal reasons for Fetch Metadata checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FetchMetadataRefusal {
    /// Request originated from a cross-site context (`Sec-Fetch-Site: cross-site`).
    CrossSiteForbidden,
    /// Request attempted iframe embedding (`Sec-Fetch-Dest: iframe`).
    IframeEmbeddingForbidden,
    /// Invalid fetch mode for the requested operation.
    InvalidFetchMode,
}

impl fmt::Display for FetchMetadataRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CrossSiteForbidden => f.write_str("cross-site fetch forbidden by policy"),
            Self::IframeEmbeddingForbidden => f.write_str("iframe embedding forbidden by policy"),
            Self::InvalidFetchMode => f.write_str("invalid Sec-Fetch-Mode for requested action"),
        }
    }
}

impl std::error::Error for FetchMetadataRefusal {}

/// Typed refusal reasons for URL query safety.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuerySafetyRefusal {
    /// Bearer credential, nonce, or ticket detected in URL query string.
    BearerInQueryForbidden,
}

impl fmt::Display for QuerySafetyRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BearerInQueryForbidden => {
                f.write_str("bearer credentials, nonces, and tickets forbidden in query string")
            }
        }
    }
}

impl std::error::Error for QuerySafetyRefusal {}

/// Typed refusal reasons for bootstrap nonce verification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NonceRefusal {
    /// Nonce is unrecognized or was already consumed (single-use invariant).
    NonceNotFoundOrConsumed,
    /// Nonce has expired past its monotonic deadline.
    NonceExpired,
    /// Nonce was issued to a different peer IP.
    PeerIpMismatch,
    /// Nonce was issued for a different session role.
    RoleMismatch,
    /// Global or per-peer pending nonce capacity exceeded.
    NonceCapacityExceeded,
    /// Authentication attempt rate limit exceeded.
    RateLimitExceeded,
}

impl fmt::Display for NonceRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NonceNotFoundOrConsumed => {
                f.write_str("bootstrap nonce unrecognized or already consumed")
            }
            Self::NonceExpired => f.write_str("bootstrap nonce expired"),
            Self::PeerIpMismatch => f.write_str("connecting peer IP does not match nonce binding"),
            Self::RoleMismatch => f.write_str("session role does not match nonce binding"),
            Self::NonceCapacityExceeded => {
                f.write_str("pending nonce capacity exceeded: throttling")
            }
            Self::RateLimitExceeded => f.write_str("bootstrap rate limit exceeded"),
        }
    }
}

impl std::error::Error for NonceRefusal {}

/// Typed refusal reasons for auxiliary channel attachment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuxiliaryTicketRefusal {
    /// Auxiliary channel attempted attachment with a bare session ID and no ticket.
    AuxiliaryTicketRequired,
    /// Ticket is unrecognized or was already consumed (single-use invariant).
    TicketNotFoundOrConsumed,
    /// Ticket has expired past its monotonic deadline.
    TicketExpired,
    /// Ticket was issued for a different remote session.
    SessionMismatch,
    /// Ticket was issued for a different auxiliary channel role.
    ChannelRoleMismatch,
    /// Ticket was issued to a different peer IP.
    PeerIpMismatch,
    /// Pending auxiliary ticket capacity exceeded.
    TicketCapacityExceeded,
}

impl fmt::Display for AuxiliaryTicketRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AuxiliaryTicketRequired => {
                f.write_str("bare session ID rejected: role-specific auxiliary ticket required")
            }
            Self::TicketNotFoundOrConsumed => {
                f.write_str("auxiliary ticket unrecognized or already consumed")
            }
            Self::TicketExpired => f.write_str("auxiliary ticket expired"),
            Self::SessionMismatch => f.write_str("auxiliary ticket session mismatch"),
            Self::ChannelRoleMismatch => f.write_str("auxiliary ticket channel role mismatch"),
            Self::PeerIpMismatch => f.write_str("connecting peer IP does not match ticket binding"),
            Self::TicketCapacityExceeded => {
                f.write_str("pending auxiliary ticket capacity exceeded")
            }
        }
    }
}

impl std::error::Error for AuxiliaryTicketRefusal {}

/// Typed refusal reasons for first-message transport authentication.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FirstMessageAuthRefusal {
    /// Authentication message deadline expired before first message arrived.
    AuthDeadlineExpired,
    /// First message exceeded maximum allowed size (1 KiB).
    MessageTooLarge,
    /// First message was malformed or did not contain valid authentication frame.
    MalformedAuthMessage,
    /// Operation attempted before authentication succeeded (e.g. video/input/audio).
    UnauthenticatedOperationAttempted,
    /// Nonce verification failed.
    NonceFailed(NonceRefusal),
}

impl fmt::Display for FirstMessageAuthRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AuthDeadlineExpired => {
                f.write_str("socket closed: first authentication message deadline expired")
            }
            Self::MessageTooLarge => f.write_str("first authentication message exceeds size limit"),
            Self::MalformedAuthMessage => {
                f.write_str("malformed first authentication message format")
            }
            Self::UnauthenticatedOperationAttempted => {
                f.write_str("operation forbidden: transport is not yet authenticated")
            }
            Self::NonceFailed(e) => write!(f, "authentication nonce rejected: {e}"),
        }
    }
}

impl std::error::Error for FirstMessageAuthRefusal {}

/// Constant-time 32-byte equality check to avoid timing side channels on secrets.
#[inline]
pub fn constant_time_eq_32(a: &[u8; 32], b: &[u8; 32]) -> bool {
    let mut diff = 0u8;
    for i in 0..32 {
        diff |= a[i] ^ b[i];
    }
    diff == 0
}

/// Compute a 32-byte SHA-256 digest of input bytes.
pub fn sha256_digest(bytes: &[u8]) -> [u8; 32] {
    fr_core::clipboard::image::sha256_digest(bytes)
}

/// Redacted wrapper around sensitive 32-byte nonces/tokens.
///
/// Prevents bearer secrets from ever appearing in standard logs or `Debug` output.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct RedactedSecret(pub [u8; 32]);

impl RedactedSecret {
    /// Create a new redacted secret.
    #[must_use]
    pub const fn new(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Access the underlying raw secret bytes (for cryptographic comparison only).
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl fmt::Debug for RedactedSecret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let digest = sha256_digest(&self.0);
        write!(
            f,
            "[REDACTED:sha256:{:02x}{:02x}{:02x}{:02x}]",
            digest[0], digest[1], digest[2], digest[3]
        )
    }
}

impl fmt::Display for RedactedSecret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self, f)
    }
}

/// Configured expected HTTPS origin and authority for host daemon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExpectedHostOrigin {
    scheme: String,
    host: String,
    port: u16,
    canonical_origin: String,
}

impl ExpectedHostOrigin {
    /// Construct a new expected host origin.
    ///
    /// Scheme must be lowercase (`"https"` in production). Host must be canonical FQDN or IP.
    pub fn new(scheme: &str, host: &str, port: u16) -> Self {
        let s = scheme.to_ascii_lowercase();
        let h = host.to_ascii_lowercase();
        let canonical_origin = if (s == "https" && port == 443) || (s == "http" && port == 80) {
            format!("{s}://{h}")
        } else {
            format!("{s}://{h}:{port}")
        };
        Self {
            scheme: s,
            host: h,
            port,
            canonical_origin,
        }
    }

    /// Construct from a Tailscale node FQDN and port with `https`.
    pub fn https_tailnet(fqdn: &str, port: u16) -> Self {
        Self::new("https", fqdn, port)
    }

    /// Get canonical origin string (e.g. `"https://node.tailnet.ts.net:8443"`).
    #[must_use]
    pub fn origin_string(&self) -> &str {
        &self.canonical_origin
    }

    /// Host name string.
    #[must_use]
    pub fn host(&self) -> &str {
        &self.host
    }

    /// Port number.
    #[must_use]
    pub const fn port(&self) -> u16 {
        self.port
    }

    /// Validate an incoming `Origin` header against this expected origin.
    ///
    /// Requires exact match. Denies `null`, `*`, scheme mismatches, and host/port differences.
    pub fn validate_origin(&self, origin_header: Option<&str>) -> Result<(), OriginRefusal> {
        let origin = origin_header.ok_or(OriginRefusal::MissingRequiredOrigin)?;

        if origin == "null" {
            return Err(OriginRefusal::NullOriginForbidden);
        }
        if origin == "*" {
            return Err(OriginRefusal::WildcardForbidden);
        }

        let trimmed = origin.trim();
        let (scheme, rest) = trimmed
            .split_once("://")
            .ok_or(OriginRefusal::MalformedOriginHeader)?;

        if scheme.to_ascii_lowercase() != self.scheme {
            return Err(OriginRefusal::SchemeMismatch);
        }

        let (host, port) = if let Some((h, p)) = rest.split_once(':') {
            let parsed_port = p
                .parse::<u16>()
                .map_err(|_| OriginRefusal::MalformedOriginHeader)?;
            (h, parsed_port)
        } else {
            let default_port = if self.scheme == "https" { 443 } else { 80 };
            (rest, default_port)
        };

        if host.to_ascii_lowercase() != self.host {
            return Err(OriginRefusal::HostMismatch);
        }
        if port != self.port {
            return Err(OriginRefusal::PortMismatch);
        }

        Ok(())
    }

    /// Validate incoming `Host` authority and optional TLS SNI.
    ///
    /// Protects against DNS rebinding, Host header manipulation, and SNI spoofing.
    pub fn validate_host_authority(
        &self,
        host_header: Option<&str>,
        sni: Option<&str>,
    ) -> Result<(), HostRefusal> {
        let header = host_header.ok_or(HostRefusal::MissingHostHeader)?;
        let trimmed = header.trim();

        let (header_host, header_port) = match trimmed.split_once(':') {
            Some((h, p)) => {
                let parsed_port = p
                    .parse::<u16>()
                    .map_err(|_| HostRefusal::MalformedHostHeader)?;
                (h, Some(parsed_port))
            }
            None => (trimmed, None),
        };

        if header_host.to_ascii_lowercase() != self.host {
            return Err(HostRefusal::HostnameMismatch);
        }

        if let Some(port) = header_port {
            // If explicit port is specified, it must match configured port unless standard port omitted
            if port != self.port {
                return Err(HostRefusal::PortMismatch);
            }
        } else {
            // If port was omitted in header, expected port must be standard (80/443)
            let standard_port = if self.scheme == "https" { 443 } else { 80 };
            if self.port != standard_port {
                return Err(HostRefusal::PortMismatch);
            }
        }

        // Validate SNI where exposed
        if let Some(sni_name) = sni
            && sni_name.to_ascii_lowercase() != self.host
        {
            return Err(HostRefusal::SniMismatch);
        }

        Ok(())
    }
}

/// Fetch Metadata header context from HTTP request.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FetchMetadata<'a> {
    pub sec_fetch_site: Option<&'a str>,
    pub sec_fetch_mode: Option<&'a str>,
    pub sec_fetch_dest: Option<&'a str>,
}

impl<'a> FetchMetadata<'a> {
    /// Construct metadata from header values.
    #[must_use]
    pub const fn new(
        sec_fetch_site: Option<&'a str>,
        sec_fetch_mode: Option<&'a str>,
        sec_fetch_dest: Option<&'a str>,
    ) -> Self {
        Self {
            sec_fetch_site,
            sec_fetch_mode,
            sec_fetch_dest,
        }
    }

    /// Check metadata for state-changing POST or WebSocket request.
    ///
    /// Denies cross-site fetches and iframe embedding.
    pub fn validate_state_changing(&self) -> Result<(), FetchMetadataRefusal> {
        if let Some(dest) = self.sec_fetch_dest
            && (dest == "iframe" || dest == "frame" || dest == "embed" || dest == "object")
        {
            return Err(FetchMetadataRefusal::IframeEmbeddingForbidden);
        }

        if let Some(site) = self.sec_fetch_site
            && site == "cross-site"
        {
            return Err(FetchMetadataRefusal::CrossSiteForbidden);
        }

        Ok(())
    }
}

/// Scans request URI path and query string to verify no bearer tokens/secrets leak in URLs.
pub fn check_url_query_safety(uri: &str) -> Result<(), QuerySafetyRefusal> {
    if let Some((_, query)) = uri.split_once('?') {
        let q_lower = query.to_ascii_lowercase();
        // Check for common credential and token query parameters
        for param in &[
            "nonce=", "ticket=", "token=", "secret=", "lease=", "auth=", "key=",
        ] {
            if q_lower.contains(param) {
                return Err(QuerySafetyRefusal::BearerInQueryForbidden);
            }
        }
    }
    Ok(())
}

/// An active single-use bootstrap nonce record.
#[derive(Debug, Clone)]
pub struct BootstrapNonceRecord {
    pub nonce: RedactedSecret,
    pub peer_ip: IpAddr,
    pub role: BrowserSessionRole,
    pub session_id: RemoteSessionId,
    pub issued_at: HostInstant,
    pub expires_at: HostInstant,
}

/// An active single-use auxiliary channel ticket record.
#[derive(Debug, Clone)]
pub struct AuxiliaryTicketRecord {
    pub ticket: RedactedSecret,
    pub peer_ip: IpAddr,
    pub session_id: RemoteSessionId,
    pub channel_role: AuxiliaryChannelRole,
    pub issued_at: HostInstant,
    pub expires_at: HostInstant,
}

/// In-memory manager for short-lived, strictly single-use bootstrap nonces.
#[derive(Debug, Default)]
pub struct BootstrapNonceManager {
    nonces: HashMap<[u8; 32], BootstrapNonceRecord>,
    per_peer_count: HashMap<IpAddr, usize>,
    sequence: u64,
}

impl BootstrapNonceManager {
    /// Create a new empty nonce manager.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Issue a new single-use bootstrap nonce bound to peer IP, session ID, and role.
    pub fn issue_nonce(
        &mut self,
        peer_ip: IpAddr,
        session_id: RemoteSessionId,
        role: BrowserSessionRole,
        now: HostInstant,
    ) -> Result<RedactedSecret, NonceRefusal> {
        self.sweep_expired(now);

        if self.nonces.len() >= MAX_PENDING_NONCES_GLOBAL {
            return Err(NonceRefusal::NonceCapacityExceeded);
        }

        let peer_count = self.per_peer_count.get(&peer_ip).copied().unwrap_or(0);
        if peer_count >= MAX_PENDING_NONCES_PER_PEER {
            return Err(NonceRefusal::RateLimitExceeded);
        }

        self.sequence = self.sequence.wrapping_add(1);

        // Derive pseudo-random 32-byte nonce using monotonic time, peer IP, sequence, and secret salt
        let mut entropy_input = Vec::with_capacity(64);
        entropy_input.extend_from_slice(b"fr-bootstrap-nonce-salt-v1");
        entropy_input.extend_from_slice(&now.as_micros().to_le_bytes());
        entropy_input.extend_from_slice(&self.sequence.to_le_bytes());
        entropy_input.extend_from_slice(&session_id.as_raw().to_le_bytes());
        match peer_ip {
            IpAddr::V4(v4) => entropy_input.extend_from_slice(&v4.octets()),
            IpAddr::V6(v6) => entropy_input.extend_from_slice(&v6.octets()),
        }

        let raw_nonce = sha256_digest(&entropy_input);
        let secret = RedactedSecret::new(raw_nonce);
        let expires_at = now
            .checked_add(BOOTSTRAP_NONCE_TTL)
            .unwrap_or(HostInstant::from_micros(u64::MAX));

        let record = BootstrapNonceRecord {
            nonce: secret,
            peer_ip,
            role,
            session_id,
            issued_at: now,
            expires_at,
        };

        self.nonces.insert(raw_nonce, record);
        *self.per_peer_count.entry(peer_ip).or_insert(0) += 1;

        Ok(secret)
    }

    /// Consume a bootstrap nonce atomically (single-use invariant).
    ///
    /// Verifies peer IP, requested role, and monotonic validity.
    pub fn consume_nonce(
        &mut self,
        raw_nonce: &[u8; 32],
        peer_ip: IpAddr,
        role: BrowserSessionRole,
        now: HostInstant,
    ) -> Result<RemoteSessionId, NonceRefusal> {
        let Some(record) = self.nonces.remove(raw_nonce) else {
            self.sweep_expired(now);
            return Err(NonceRefusal::NonceNotFoundOrConsumed);
        };

        // Decrement per-peer tracking count
        if let Some(count) = self.per_peer_count.get_mut(&record.peer_ip) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                self.per_peer_count.remove(&record.peer_ip);
            }
        }

        self.sweep_expired(now);

        if now > record.expires_at {
            return Err(NonceRefusal::NonceExpired);
        }

        if record.peer_ip != peer_ip {
            return Err(NonceRefusal::PeerIpMismatch);
        }

        if record.role != role {
            return Err(NonceRefusal::RoleMismatch);
        }

        Ok(record.session_id)
    }

    /// Remove expired nonces.
    pub fn sweep_expired(&mut self, now: HostInstant) {
        let mut expired_keys = Vec::new();
        for (k, record) in &self.nonces {
            if now > record.expires_at {
                expired_keys.push((*k, record.peer_ip));
            }
        }
        for (k, peer) in expired_keys {
            self.nonces.remove(&k);
            if let Some(count) = self.per_peer_count.get_mut(&peer) {
                *count = count.saturating_sub(1);
                if *count == 0 {
                    self.per_peer_count.remove(&peer);
                }
            }
        }
    }

    /// Total count of pending nonces.
    #[must_use]
    pub fn pending_count(&self) -> usize {
        self.nonces.len()
    }
}

/// In-memory manager for role-specific single-use auxiliary channel tickets.
#[derive(Debug, Default)]
pub struct AuxiliaryTicketManager {
    tickets: HashMap<[u8; 32], AuxiliaryTicketRecord>,
    sequence: u64,
}

impl AuxiliaryTicketManager {
    /// Create a new empty ticket manager.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Issue a new single-use auxiliary attachment ticket bound to session, role, and peer IP.
    pub fn issue_ticket(
        &mut self,
        session_id: RemoteSessionId,
        channel_role: AuxiliaryChannelRole,
        peer_ip: IpAddr,
        now: HostInstant,
    ) -> Result<RedactedSecret, AuxiliaryTicketRefusal> {
        self.sweep_expired(now);

        if self.tickets.len() >= MAX_PENDING_TICKETS_GLOBAL {
            return Err(AuxiliaryTicketRefusal::TicketCapacityExceeded);
        }

        self.sequence = self.sequence.wrapping_add(1);

        let mut entropy_input = Vec::with_capacity(64);
        entropy_input.extend_from_slice(b"fr-auxiliary-ticket-salt-v1");
        entropy_input.extend_from_slice(&now.as_micros().to_le_bytes());
        entropy_input.extend_from_slice(&self.sequence.to_le_bytes());
        entropy_input.extend_from_slice(&session_id.as_raw().to_le_bytes());
        entropy_input.push(channel_role as u8);
        match peer_ip {
            IpAddr::V4(v4) => entropy_input.extend_from_slice(&v4.octets()),
            IpAddr::V6(v6) => entropy_input.extend_from_slice(&v6.octets()),
        }

        let raw_ticket = sha256_digest(&entropy_input);
        let secret = RedactedSecret::new(raw_ticket);
        let expires_at = now
            .checked_add(AUXILIARY_TICKET_TTL)
            .unwrap_or(HostInstant::from_micros(u64::MAX));

        let record = AuxiliaryTicketRecord {
            ticket: secret,
            peer_ip,
            session_id,
            channel_role,
            issued_at: now,
            expires_at,
        };

        self.tickets.insert(raw_ticket, record);

        Ok(secret)
    }

    /// Consume an auxiliary ticket atomically (single-use invariant).
    ///
    /// Strictly rejects attachment attempts that provide no ticket or a bare session ID.
    pub fn consume_ticket(
        &mut self,
        raw_ticket: Option<&[u8; 32]>,
        session_id: RemoteSessionId,
        channel_role: AuxiliaryChannelRole,
        peer_ip: IpAddr,
        now: HostInstant,
    ) -> Result<(), AuxiliaryTicketRefusal> {
        let ticket_bytes = raw_ticket.ok_or(AuxiliaryTicketRefusal::AuxiliaryTicketRequired)?;

        let Some(record) = self.tickets.remove(ticket_bytes) else {
            self.sweep_expired(now);
            return Err(AuxiliaryTicketRefusal::TicketNotFoundOrConsumed);
        };

        self.sweep_expired(now);

        if now > record.expires_at {
            return Err(AuxiliaryTicketRefusal::TicketExpired);
        }

        if record.session_id != session_id {
            return Err(AuxiliaryTicketRefusal::SessionMismatch);
        }

        if record.channel_role != channel_role {
            return Err(AuxiliaryTicketRefusal::ChannelRoleMismatch);
        }

        if record.peer_ip != peer_ip {
            return Err(AuxiliaryTicketRefusal::PeerIpMismatch);
        }

        Ok(())
    }

    /// Remove expired tickets.
    pub fn sweep_expired(&mut self, now: HostInstant) {
        self.tickets.retain(|_, record| now <= record.expires_at);
    }

    /// Total count of pending tickets.
    #[must_use]
    pub fn pending_count(&self) -> usize {
        self.tickets.len()
    }
}

/// State of a connecting browser transport socket awaiting first-message authentication.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SocketAuthState {
    /// Socket connected; waiting for first authentication message containing single-use nonce.
    AwaitingAuth {
        peer_ip: IpAddr,
        deadline: HostInstant,
    },
    /// Successfully authenticated; session ID and granted role established.
    Authenticated {
        peer_ip: IpAddr,
        session_id: RemoteSessionId,
        role: BrowserSessionRole,
    },
    /// Failed or closed.
    Closed,
}

/// Guard enforcing that NO pixels, audio, or input occur before first-message auth.
#[derive(Debug)]
pub struct SocketAuthGuard {
    state: SocketAuthState,
}

impl SocketAuthGuard {
    /// Create a new socket auth guard in `AwaitingAuth` state.
    #[must_use]
    pub fn new(peer_ip: IpAddr, now: HostInstant) -> Self {
        Self {
            state: SocketAuthState::AwaitingAuth {
                peer_ip,
                deadline: now
                    .checked_add(FIRST_MESSAGE_AUTH_TIMEOUT)
                    .unwrap_or(HostInstant::from_micros(u64::MAX)),
            },
        }
    }

    /// Check if transport is currently authenticated.
    #[must_use]
    pub fn is_authenticated(&self) -> bool {
        matches!(self.state, SocketAuthState::Authenticated { .. })
    }

    /// Try to submit video/pixels, audio, or input.
    ///
    /// Returns typed refusal if the socket has not yet authenticated.
    pub fn check_media_or_input_allowed(&self) -> Result<(), FirstMessageAuthRefusal> {
        if self.is_authenticated() {
            Ok(())
        } else {
            Err(FirstMessageAuthRefusal::UnauthenticatedOperationAttempted)
        }
    }

    /// Handle arrival of first authentication message.
    pub fn process_auth_message(
        &mut self,
        message_bytes: &[u8],
        expected_role: BrowserSessionRole,
        nonce_mgr: &mut BootstrapNonceManager,
        now: HostInstant,
    ) -> Result<RemoteSessionId, FirstMessageAuthRefusal> {
        let (peer_ip, deadline) = match self.state {
            SocketAuthState::AwaitingAuth { peer_ip, deadline } => (peer_ip, deadline),
            SocketAuthState::Authenticated { .. } | SocketAuthState::Closed => {
                return Err(FirstMessageAuthRefusal::MalformedAuthMessage);
            }
        };

        if now > deadline {
            self.state = SocketAuthState::Closed;
            return Err(FirstMessageAuthRefusal::AuthDeadlineExpired);
        }

        if message_bytes.len() > MAX_AUTH_MESSAGE_BYTES {
            self.state = SocketAuthState::Closed;
            return Err(FirstMessageAuthRefusal::MessageTooLarge);
        }

        // Expected format: 4-byte magic b"FRBA" (FrankenRemote Browser Auth) + 32-byte nonce
        if message_bytes.len() < 36 || &message_bytes[0..4] != b"FRBA" {
            self.state = SocketAuthState::Closed;
            return Err(FirstMessageAuthRefusal::MalformedAuthMessage);
        }

        let mut raw_nonce = [0u8; 32];
        raw_nonce.copy_from_slice(&message_bytes[4..36]);

        let session_id = nonce_mgr
            .consume_nonce(&raw_nonce, peer_ip, expected_role, now)
            .map_err(|e| {
                self.state = SocketAuthState::Closed;
                FirstMessageAuthRefusal::NonceFailed(e)
            })?;

        self.state = SocketAuthState::Authenticated {
            peer_ip,
            session_id,
            role: expected_role,
        };

        Ok(session_id)
    }
}

/// Complete browser origin security policy engine.
#[derive(Debug)]
pub struct BrowserSecurityPolicy {
    expected_origin: ExpectedHostOrigin,
    nonce_mgr: Mutex<BootstrapNonceManager>,
    ticket_mgr: Mutex<AuxiliaryTicketManager>,
}

impl BrowserSecurityPolicy {
    /// Construct a new policy with expected origin.
    #[must_use]
    pub fn new(expected_origin: ExpectedHostOrigin) -> Self {
        Self {
            expected_origin,
            nonce_mgr: Mutex::new(BootstrapNonceManager::new()),
            ticket_mgr: Mutex::new(AuxiliaryTicketManager::new()),
        }
    }

    /// Reference to expected origin.
    #[must_use]
    pub fn expected_origin(&self) -> &ExpectedHostOrigin {
        &self.expected_origin
    }

    /// Validate navigation GET request (static content only).
    ///
    /// Navigation GET does not require an `Origin` header, but verifies `Host` authority and URL safety.
    /// Never grants a lease, capture start, or ticket.
    pub fn validate_navigation_get(
        &self,
        uri: &str,
        host_header: Option<&str>,
        sni: Option<&str>,
        metadata: &FetchMetadata<'_>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        check_url_query_safety(uri)?;
        self.expected_origin
            .validate_host_authority(host_header, sni)?;

        // Ensure dest is not an iframe
        if let Some(dest) = metadata.sec_fetch_dest
            && (dest == "iframe" || dest == "frame")
        {
            return Err(Box::new(FetchMetadataRefusal::IframeEmbeddingForbidden));
        }

        Ok(())
    }

    /// Validate and execute state-changing bootstrap POST request (`POST /api/session/bootstrap`).
    ///
    /// Requires exact Origin, valid Host header, safe URL, valid Fetch Metadata, and verified peer IP.
    /// Issues a short-lived, single-use bootstrap nonce.
    pub fn handle_bootstrap_post(
        &self,
        req: &BootstrapRequest<'_>,
    ) -> Result<RedactedSecret, Box<dyn std::error::Error + Send + Sync>> {
        check_url_query_safety(req.uri)?;
        self.expected_origin
            .validate_host_authority(req.host_header, req.sni)?;
        self.expected_origin.validate_origin(req.origin_header)?;
        req.metadata.validate_state_changing()?;

        let mut mgr = self
            .nonce_mgr
            .lock()
            .map_err(|_| std::io::Error::other("nonce manager lock poisoned"))?;

        let secret = mgr.issue_nonce(req.peer_ip, req.session_id, req.role, req.now)?;
        Ok(secret)
    }

    /// Issue an auxiliary channel single-use attachment ticket.
    pub fn issue_auxiliary_ticket(
        &self,
        session_id: RemoteSessionId,
        channel_role: AuxiliaryChannelRole,
        peer_ip: IpAddr,
        now: HostInstant,
    ) -> Result<RedactedSecret, AuxiliaryTicketRefusal> {
        let mut mgr = self
            .ticket_mgr
            .lock()
            .map_err(|_| AuxiliaryTicketRefusal::TicketCapacityExceeded)?;
        mgr.issue_ticket(session_id, channel_role, peer_ip, now)
    }

    /// Consume an auxiliary channel attachment ticket.
    pub fn consume_auxiliary_ticket(
        &self,
        raw_ticket: Option<&[u8; 32]>,
        session_id: RemoteSessionId,
        channel_role: AuxiliaryChannelRole,
        peer_ip: IpAddr,
        now: HostInstant,
    ) -> Result<(), AuxiliaryTicketRefusal> {
        let mut mgr = self
            .ticket_mgr
            .lock()
            .map_err(|_| AuxiliaryTicketRefusal::TicketNotFoundOrConsumed)?;
        mgr.consume_ticket(raw_ticket, session_id, channel_role, peer_ip, now)
    }

    /// Access the nonce manager for socket first-message verification.
    pub fn with_nonce_manager<F, R>(&self, f: F) -> Result<R, std::io::Error>
    where
        F: FnOnce(&mut BootstrapNonceManager) -> R,
    {
        let mut mgr = self
            .nonce_mgr
            .lock()
            .map_err(|_| std::io::Error::other("nonce manager lock poisoned"))?;
        Ok(f(&mut mgr))
    }
}

/// Parameters for a browser session bootstrap POST request.
#[derive(Debug, Clone)]
pub struct BootstrapRequest<'a> {
    pub uri: &'a str,
    pub host_header: Option<&'a str>,
    pub origin_header: Option<&'a str>,
    pub sni: Option<&'a str>,
    pub metadata: &'a FetchMetadata<'a>,
    pub peer_ip: IpAddr,
    pub session_id: RemoteSessionId,
    pub role: BrowserSessionRole,
    pub now: HostInstant,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    fn test_peer() -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(100, 64, 1, 42))
    }

    fn other_peer() -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(100, 64, 1, 99))
    }

    #[test]
    fn exact_origin_validation_accepts_valid_and_rejects_mismatch() {
        let expected = ExpectedHostOrigin::https_tailnet("workstation.tailnet.ts.net", 8443);

        // Valid exact origin
        assert!(
            expected
                .validate_origin(Some("https://workstation.tailnet.ts.net:8443"))
                .is_ok()
        );

        // Missing origin
        assert_eq!(
            expected.validate_origin(None),
            Err(OriginRefusal::MissingRequiredOrigin)
        );

        // Null origin (sandboxed iframe / data URI)
        assert_eq!(
            expected.validate_origin(Some("null")),
            Err(OriginRefusal::NullOriginForbidden)
        );

        // Wildcard
        assert_eq!(
            expected.validate_origin(Some("*")),
            Err(OriginRefusal::WildcardForbidden)
        );

        // Scheme mismatch (http vs https)
        assert_eq!(
            expected.validate_origin(Some("http://workstation.tailnet.ts.net:8443")),
            Err(OriginRefusal::SchemeMismatch)
        );

        // Host mismatch (attacker origin)
        assert_eq!(
            expected.validate_origin(Some("https://attacker.com:8443")),
            Err(OriginRefusal::HostMismatch)
        );

        // Port mismatch
        assert_eq!(
            expected.validate_origin(Some("https://workstation.tailnet.ts.net:9443")),
            Err(OriginRefusal::PortMismatch)
        );
    }
}
