//! Bounded HTTP/1.1 request parser and response writer for `frd` browser ingress.
//!
//! Enforces anti-smuggling invariants before any application logic runs:
//! - Hard ceiling on request header block (`MAX_HTTP_HEADER_BYTES = 8 KiB`).
//! - Hard ceiling on header count (`MAX_HTTP_HEADERS = 48`).
//! - Hard ceiling on POST body size (`MAX_HTTP_BODY_BYTES = 4 KiB`).
//! - Rejects `Transfer-Encoding` (no chunked smuggling on small control endpoints).
//! - Rejects duplicate `Content-Length`, obs-fold continuations, NUL bytes, and
//!   malformed request lines before allocation.

use asupersync::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use core::fmt;

/// Maximum bytes read for an HTTP/1.1 request line + headers.
pub const MAX_HTTP_HEADER_BYTES: usize = 8 * 1024;
/// Maximum number of HTTP headers accepted in one request.
pub const MAX_HTTP_HEADERS: usize = 48;
/// Maximum HTTP request body accepted (bootstrap JSON is < 128 B).
pub const MAX_HTTP_BODY_BYTES: usize = 4 * 1024;

/// Parsed, bounded HTTP/1.1 request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpRequest {
    pub method: String,
    pub target: String,
    pub host: Option<String>,
    pub origin: Option<String>,
    pub sec_fetch_site: Option<String>,
    pub sec_fetch_mode: Option<String>,
    pub sec_fetch_dest: Option<String>,
    pub upgrade: Option<String>,
    pub connection: Option<String>,
    pub sec_websocket_key: Option<String>,
    pub sec_websocket_version: Option<String>,
    pub sec_websocket_extensions: Option<String>,
    pub body: Vec<u8>,
}

impl HttpRequest {
    /// Request path component before any `?query` suffix.
    #[must_use]
    pub fn path(&self) -> &str {
        self.target
            .split_once('?')
            .map_or(self.target.as_str(), |(path, _)| path)
    }

    /// True if headers request an RFC 6455 WebSocket upgrade.
    #[must_use]
    pub fn is_websocket_upgrade(&self) -> bool {
        let upgrade_ok = self
            .upgrade
            .as_deref()
            .is_some_and(|v| v.eq_ignore_ascii_case("websocket"));
        let conn_ok = self.connection.as_deref().is_some_and(|v| {
            v.split(',')
                .any(|tok| tok.trim().eq_ignore_ascii_case("upgrade"))
        });
        let version_ok = self
            .sec_websocket_version
            .as_deref()
            .is_some_and(|v| v.trim() == "13");
        upgrade_ok && conn_ok && version_ok && self.sec_websocket_key.is_some()
    }
}

/// Typed HTTP/1.1 parsing errors (all mapped to 400/413/431).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HttpParseError {
    Eof,
    Io,
    HeadersTooLarge,
    TooManyHeaders,
    BodyTooLarge,
    MalformedRequestLine,
    MalformedHeader,
    SmugglingRejected,
    UnsupportedVersion,
}

impl fmt::Display for HttpParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Eof => "connection closed before complete HTTP request",
            Self::Io => "transport I/O error reading HTTP request",
            Self::HeadersTooLarge => "HTTP headers exceeded 8 KiB ceiling",
            Self::TooManyHeaders => "HTTP header count exceeded ceiling",
            Self::BodyTooLarge => "HTTP request body exceeded 4 KiB ceiling",
            Self::MalformedRequestLine => "malformed HTTP/1.1 request line",
            Self::MalformedHeader => "malformed HTTP header",
            Self::SmugglingRejected => {
                "ambiguous HTTP framing rejected (Transfer-Encoding or duplicate Content-Length)"
            }
            Self::UnsupportedVersion => "HTTP/1.1 required",
        })
    }
}

impl std::error::Error for HttpParseError {}

/// Read one bounded HTTP/1.1 request from an async stream.
pub async fn read_http_request<R: AsyncRead + Unpin>(
    stream: &mut R,
) -> Result<HttpRequest, HttpParseError> {
    let mut raw = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    let header_end = loop {
        if let Some(pos) = find_header_end(&raw) {
            break pos;
        }
        if raw.len() >= MAX_HTTP_HEADER_BYTES {
            return Err(HttpParseError::HeadersTooLarge);
        }
        let max_read = (MAX_HTTP_HEADER_BYTES - raw.len()).min(chunk.len());
        let n = stream
            .read(&mut chunk[..max_read])
            .await
            .map_err(|_| HttpParseError::Io)?;
        if n == 0 {
            return Err(HttpParseError::Eof);
        }
        raw.extend_from_slice(&chunk[..n]);
    };

    let head_bytes = &raw[..header_end];
    if head_bytes.contains(&0) {
        return Err(HttpParseError::MalformedHeader);
    }
    let head = core::str::from_utf8(head_bytes).map_err(|_| HttpParseError::MalformedHeader)?;
    let mut lines = head.split("\r\n");
    let request_line = lines.next().ok_or(HttpParseError::MalformedRequestLine)?;
    let mut rl_parts = request_line.split(' ');
    let method = rl_parts
        .next()
        .filter(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_uppercase()))
        .ok_or(HttpParseError::MalformedRequestLine)?
        .to_owned();
    let target = rl_parts
        .next()
        .filter(|s| !s.is_empty() && s.starts_with('/'))
        .ok_or(HttpParseError::MalformedRequestLine)?
        .to_owned();
    let version = rl_parts
        .next()
        .ok_or(HttpParseError::MalformedRequestLine)?;
    if rl_parts.next().is_some() {
        return Err(HttpParseError::MalformedRequestLine);
    }
    if version != "HTTP/1.1" {
        return Err(HttpParseError::UnsupportedVersion);
    }

    let mut host = None;
    let mut origin = None;
    let mut sec_fetch_site = None;
    let mut sec_fetch_mode = None;
    let mut sec_fetch_dest = None;
    let mut upgrade = None;
    let mut connection = None;
    let mut sec_websocket_key = None;
    let mut sec_websocket_version = None;
    let mut sec_websocket_extensions = None;
    let mut content_length: Option<usize> = None;
    let mut header_count = 0usize;

    for line in lines {
        if line.is_empty() {
            continue;
        }
        header_count += 1;
        if header_count > MAX_HTTP_HEADERS {
            return Err(HttpParseError::TooManyHeaders);
        }
        // Reject obs-fold (leading whitespace continuation).
        if line.starts_with(' ') || line.starts_with('\t') {
            return Err(HttpParseError::SmugglingRejected);
        }
        let (name, value) = line
            .split_once(':')
            .ok_or(HttpParseError::MalformedHeader)?;
        if name.is_empty()
            || name
                .bytes()
                .any(|b| b.is_ascii_whitespace() || b.is_ascii_control())
        {
            return Err(HttpParseError::MalformedHeader);
        }
        let val = value.trim();
        if val.bytes().any(|b| b.is_ascii_control() && b != b'\t') {
            return Err(HttpParseError::MalformedHeader);
        }

        if name.eq_ignore_ascii_case("transfer-encoding") {
            return Err(HttpParseError::SmugglingRejected);
        } else if name.eq_ignore_ascii_case("content-length") {
            if content_length.is_some() {
                return Err(HttpParseError::SmugglingRejected);
            }
            let parsed = val
                .parse::<usize>()
                .map_err(|_| HttpParseError::MalformedHeader)?;
            if parsed > MAX_HTTP_BODY_BYTES {
                return Err(HttpParseError::BodyTooLarge);
            }
            content_length = Some(parsed);
        } else if name.eq_ignore_ascii_case("host") {
            if host.is_some() {
                return Err(HttpParseError::SmugglingRejected);
            }
            host = Some(val.to_owned());
        } else if name.eq_ignore_ascii_case("origin") {
            if origin.is_some() {
                return Err(HttpParseError::SmugglingRejected);
            }
            origin = Some(val.to_owned());
        } else if name.eq_ignore_ascii_case("sec-fetch-site") {
            sec_fetch_site = Some(val.to_owned());
        } else if name.eq_ignore_ascii_case("sec-fetch-mode") {
            sec_fetch_mode = Some(val.to_owned());
        } else if name.eq_ignore_ascii_case("sec-fetch-dest") {
            sec_fetch_dest = Some(val.to_owned());
        } else if name.eq_ignore_ascii_case("upgrade") {
            upgrade = Some(val.to_owned());
        } else if name.eq_ignore_ascii_case("connection") {
            connection = Some(val.to_owned());
        } else if name.eq_ignore_ascii_case("sec-websocket-key") {
            sec_websocket_key = Some(val.to_owned());
        } else if name.eq_ignore_ascii_case("sec-websocket-version") {
            sec_websocket_version = Some(val.to_owned());
        } else if name.eq_ignore_ascii_case("sec-websocket-extensions") {
            sec_websocket_extensions = Some(val.to_owned());
        }
    }

    let expected_body = content_length.unwrap_or(0);
    let already_read_body = &raw[header_end + 4..];
    if already_read_body.len() > MAX_HTTP_BODY_BYTES || already_read_body.len() > expected_body {
        return Err(HttpParseError::SmugglingRejected);
    }

    let mut body = Vec::with_capacity(expected_body);
    body.extend_from_slice(already_read_body);
    while body.len() < expected_body {
        let need = (expected_body - body.len()).min(chunk.len());
        let n = stream
            .read(&mut chunk[..need])
            .await
            .map_err(|_| HttpParseError::Io)?;
        if n == 0 {
            return Err(HttpParseError::Eof);
        }
        body.extend_from_slice(&chunk[..n]);
    }

    Ok(HttpRequest {
        method,
        target,
        host,
        origin,
        sec_fetch_site,
        sec_fetch_mode,
        sec_fetch_dest,
        upgrade,
        connection,
        sec_websocket_key,
        sec_websocket_version,
        sec_websocket_extensions,
        body,
    })
}

fn find_header_end(bytes: &[u8]) -> Option<usize> {
    bytes.windows(4).position(|w| w == b"\r\n\r\n")
}

/// Write a complete HTTP/1.1 response with mandatory browser security headers.
pub async fn write_http_response<W: AsyncWrite + Unpin>(
    stream: &mut W,
    status: u16,
    reason: &str,
    content_type: &str,
    csp: &str,
    body: &[u8],
) -> std::io::Result<()> {
    let header = format!(
        "HTTP/1.1 {status} {reason}\r\n\
         Content-Type: {content_type}\r\n\
         Content-Length: {}\r\n\
         Content-Security-Policy: {csp}\r\n\
         X-Content-Type-Options: nosniff\r\n\
         X-Frame-Options: DENY\r\n\
         Referrer-Policy: no-referrer\r\n\
         Cache-Control: no-cache, no-store, must-revalidate\r\n\
         Connection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(header.as_bytes()).await?;
    stream.write_all(body).await?;
    stream.flush().await
}

/// Write an HTTP/1.1 `101 Switching Protocols` response completing an RFC 6455 upgrade.
/// Deliberately omits `Sec-WebSocket-Extensions` (`permessage-deflate` is forbidden).
pub async fn write_ws_upgrade_response<W: AsyncWrite + Unpin>(
    stream: &mut W,
    accept_key: &str,
) -> std::io::Result<()> {
    let response = format!(
        "HTTP/1.1 101 Switching Protocols\r\n\
         Upgrade: websocket\r\n\
         Connection: Upgrade\r\n\
         Sec-WebSocket-Accept: {accept_key}\r\n\r\n"
    );
    stream.write_all(response.as_bytes()).await?;
    stream.flush().await
}
