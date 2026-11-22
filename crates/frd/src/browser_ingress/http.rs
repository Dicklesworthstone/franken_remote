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
            Self::SmugglingRejected => "ambiguous HTTP framing or repeated policy header rejected",
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

    let (mut request, expected_body) = parse_http_head(&raw[..header_end])?;
    let already_read_body = &raw[header_end + 4..];
    if already_read_body.len() > expected_body {
        return Err(HttpParseError::SmugglingRejected);
    }
    request.body = Vec::with_capacity(expected_body);
    request.body.extend_from_slice(already_read_body);
    while request.body.len() < expected_body {
        let need = (expected_body - request.body.len()).min(chunk.len());
        let n = stream
            .read(&mut chunk[..need])
            .await
            .map_err(|_| HttpParseError::Io)?;
        if n == 0 {
            return Err(HttpParseError::Eof);
        }
        request.body.extend_from_slice(&chunk[..n]);
    }
    Ok(request)
}

fn parse_request_line(line: &str) -> Result<(String, String), HttpParseError> {
    let mut parts = line.split(' ');
    let method = parts
        .next()
        .filter(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_uppercase()))
        .ok_or(HttpParseError::MalformedRequestLine)?;
    let target = parts
        .next()
        .filter(|s| s.starts_with('/') && s.bytes().all(|b| b.is_ascii_graphic()))
        .ok_or(HttpParseError::MalformedRequestLine)?;
    let version = parts.next().ok_or(HttpParseError::MalformedRequestLine)?;
    if parts.next().is_some() {
        return Err(HttpParseError::MalformedRequestLine);
    }
    if version != "HTTP/1.1" {
        return Err(HttpParseError::UnsupportedVersion);
    }
    Ok((method.to_owned(), target.to_owned()))
}

/// Only HTTP token bytes may name a field. In particular, punctuation that
/// another parser might interpret as a delimiter cannot hide a policy header.
fn header_name_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte)
}

fn singleton(field: &mut Option<String>, value: &str) -> Result<(), HttpParseError> {
    // Every retained field has an ASCII grammar. Keeping Unicode lookalike
    // whitespace could turn a denied Fetch Metadata value into an unknown one.
    if !value.is_ascii() {
        return Err(HttpParseError::MalformedHeader);
    }
    if field.is_some() {
        return Err(HttpParseError::SmugglingRejected);
    }
    *field = Some(value.to_owned());
    Ok(())
}

/// Parse policy-relevant fields exactly once, before allocating a body or
/// consulting Origin, fetch metadata, bootstrap or WebSocket admission.
fn parse_http_head(bytes: &[u8]) -> Result<(HttpRequest, usize), HttpParseError> {
    let head = core::str::from_utf8(bytes).map_err(|_| HttpParseError::MalformedHeader)?;
    let mut lines = head.split("\r\n");
    let request_line = lines.next().ok_or(HttpParseError::MalformedRequestLine)?;
    let (method, target) = parse_request_line(request_line)?;
    let mut request = HttpRequest {
        method,
        target,
        host: None,
        origin: None,
        sec_fetch_site: None,
        sec_fetch_mode: None,
        sec_fetch_dest: None,
        upgrade: None,
        connection: None,
        sec_websocket_key: None,
        sec_websocket_version: None,
        sec_websocket_extensions: None,
        body: Vec::new(),
    };
    let mut content_length = None;
    for (index, line) in lines.enumerate() {
        if index >= MAX_HTTP_HEADERS {
            return Err(HttpParseError::TooManyHeaders);
        }
        if line.starts_with(' ') || line.starts_with('\t') {
            return Err(HttpParseError::SmugglingRejected);
        }
        let (name, value) = line
            .split_once(':')
            .ok_or(HttpParseError::MalformedHeader)?;
        if name.is_empty() || !name.bytes().all(header_name_byte) {
            return Err(HttpParseError::MalformedHeader);
        }
        // Check the original bytes: trim() would silently erase CR, LF,
        // vertical tabs and other malformed input at the ends of a value.
        if value.bytes().any(|b| b.is_ascii_control() && b != b'\t') {
            return Err(HttpParseError::MalformedHeader);
        }
        let value = value.trim_matches([' ', '\t']);
        match name.to_ascii_lowercase().as_str() {
            "transfer-encoding" => return Err(HttpParseError::SmugglingRejected),
            "content-length" => {
                if content_length.is_some() {
                    return Err(HttpParseError::SmugglingRejected);
                }
                if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
                    return Err(HttpParseError::MalformedHeader);
                }
                let length = value
                    .parse::<usize>()
                    .map_err(|_| HttpParseError::MalformedHeader)?;
                if length > MAX_HTTP_BODY_BYTES {
                    return Err(HttpParseError::BodyTooLarge);
                }
                content_length = Some(length);
            }
            "host" => singleton(&mut request.host, value)?,
            "origin" => singleton(&mut request.origin, value)?,
            "sec-fetch-site" => singleton(&mut request.sec_fetch_site, value)?,
            "sec-fetch-mode" => singleton(&mut request.sec_fetch_mode, value)?,
            "sec-fetch-dest" => singleton(&mut request.sec_fetch_dest, value)?,
            "upgrade" => singleton(&mut request.upgrade, value)?,
            "connection" => singleton(&mut request.connection, value)?,
            "sec-websocket-key" => singleton(&mut request.sec_websocket_key, value)?,
            "sec-websocket-version" => singleton(&mut request.sec_websocket_version, value)?,
            "sec-websocket-extensions" => singleton(&mut request.sec_websocket_extensions, value)?,
            _ => {}
        }
    }
    Ok((request, content_length.unwrap_or(0)))
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
