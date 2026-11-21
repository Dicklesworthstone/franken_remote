//! Bounded RFC 6455 binary WebSocket framing for `frd` browser ingress.
//!
//! Enforces FrankenRemote WSS profile invariants (`AGENTS.md` §§5, 6; Plan §12.5):
//! - Compression extensions (`RSV1`/`RSV2`/`RSV3` nonzero) are strictly refused.
//! - Text frames (opcode `0x01`) are refused on binary WSS channels.
//! - Client-to-server frames MUST be masked per RFC 6455 §5.1.
//! - Payload ceiling (`max_payload_bytes`) is checked BEFORE allocation.

use asupersync::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use core::fmt;
use ring::digest::{SHA1_FOR_LEGACY_USE_ONLY, digest};

const WS_GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

/// Standard RFC 4648 base64 encoder (zero extra dependencies, `#![forbid(unsafe_code)]`).
#[must_use]
pub fn base64_encode(input: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = u32::from(chunk[0]);
        let b1 = if chunk.len() > 1 {
            u32::from(chunk[1])
        } else {
            0
        };
        let b2 = if chunk.len() > 2 {
            u32::from(chunk[2])
        } else {
            0
        };
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(char::from(TABLE[((triple >> 18) & 0x3F) as usize]));
        out.push(char::from(TABLE[((triple >> 12) & 0x3F) as usize]));
        if chunk.len() > 1 {
            out.push(char::from(TABLE[((triple >> 6) & 0x3F) as usize]));
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(char::from(TABLE[(triple & 0x3F) as usize]));
        } else {
            out.push('=');
        }
    }
    out
}

/// Compute RFC 6455 `Sec-WebSocket-Accept` from `Sec-WebSocket-Key`.
#[must_use]
pub fn compute_accept_key(client_key: &str) -> String {
    let mut input = String::with_capacity(client_key.len() + WS_GUID.len());
    input.push_str(client_key.trim());
    input.push_str(WS_GUID);
    let sha1 = digest(&SHA1_FOR_LEGACY_USE_ONLY, input.as_bytes());
    base64_encode(sha1.as_ref())
}

/// Decoded inbound RFC 6455 WebSocket frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WsFrame {
    Binary(Vec<u8>),
    Ping(Vec<u8>),
    Pong(Vec<u8>),
    Close,
}

/// Typed RFC 6455 framing errors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WsFrameError {
    Eof,
    Io,
    CompressionForbidden,
    UnmaskedClientFrame,
    TextFrameForbidden,
    FragmentedFrameUnsupported,
    UnsupportedOpcode(u8),
    PayloadTooLarge,
}

impl fmt::Display for WsFrameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Eof => f.write_str("WebSocket stream closed"),
            Self::Io => f.write_str("WebSocket I/O error"),
            Self::CompressionForbidden => {
                f.write_str("WebSocket RSV bits nonzero: compression forbidden")
            }
            Self::UnmaskedClientFrame => f.write_str("client WebSocket frame must be masked"),
            Self::TextFrameForbidden => {
                f.write_str("text WebSocket frames forbidden on WSS channel")
            }
            Self::FragmentedFrameUnsupported => f.write_str("fragmented WebSocket frame refused"),
            Self::UnsupportedOpcode(op) => write!(f, "unsupported WebSocket opcode 0x{op:02x}"),
            Self::PayloadTooLarge => f.write_str("WebSocket frame exceeds bounded ceiling"),
        }
    }
}

impl std::error::Error for WsFrameError {}

/// Read one bounded RFC 6455 frame. `require_masked = true` for client->server frames.
pub async fn read_ws_frame<R: AsyncRead + Unpin>(
    stream: &mut R,
    max_payload_bytes: usize,
    require_masked: bool,
) -> Result<WsFrame, WsFrameError> {
    let mut header = [0u8; 2];
    read_exact_checked(stream, &mut header).await?;

    let fin = (header[0] & 0x80) != 0;
    let rsv = header[0] & 0x70;
    let opcode = header[0] & 0x0F;
    let masked = (header[1] & 0x80) != 0;
    let len_code = header[1] & 0x7F;

    if rsv != 0 {
        return Err(WsFrameError::CompressionForbidden);
    }
    if !fin {
        return Err(WsFrameError::FragmentedFrameUnsupported);
    }
    if require_masked && !masked {
        return Err(WsFrameError::UnmaskedClientFrame);
    }
    if opcode == 0x01 {
        return Err(WsFrameError::TextFrameForbidden);
    }

    let payload_len: usize = match len_code {
        0..=125 => usize::from(len_code),
        126 => {
            let mut ext = [0u8; 2];
            read_exact_checked(stream, &mut ext).await?;
            usize::from(u16::from_be_bytes(ext))
        }
        127 => {
            let mut ext = [0u8; 8];
            read_exact_checked(stream, &mut ext).await?;
            let raw = u64::from_be_bytes(ext);
            usize::try_from(raw).map_err(|_| WsFrameError::PayloadTooLarge)?
        }
        _ => return Err(WsFrameError::PayloadTooLarge),
    };

    // Control frames (0x08..=0x0A) are bounded to 125 bytes by RFC 6455 §5.5.
    let effective_limit = if opcode >= 0x08 {
        max_payload_bytes.min(125)
    } else {
        max_payload_bytes
    };
    if payload_len > effective_limit {
        return Err(WsFrameError::PayloadTooLarge);
    }

    let mask_key = if masked {
        let mut key = [0u8; 4];
        read_exact_checked(stream, &mut key).await?;
        Some(key)
    } else {
        None
    };

    let mut payload = Vec::new();
    payload
        .try_reserve_exact(payload_len)
        .map_err(|_| WsFrameError::PayloadTooLarge)?;
    payload.resize(payload_len, 0);
    if payload_len > 0 {
        read_exact_checked(stream, &mut payload).await?;
    }
    if let Some(key) = mask_key {
        for (idx, byte) in payload.iter_mut().enumerate() {
            *byte ^= key[idx % 4];
        }
    }

    match opcode {
        0x02 => Ok(WsFrame::Binary(payload)),
        0x08 => Ok(WsFrame::Close),
        0x09 => Ok(WsFrame::Ping(payload)),
        0x0A => Ok(WsFrame::Pong(payload)),
        other => Err(WsFrameError::UnsupportedOpcode(other)),
    }
}

/// Write a single unmasked server-to-client (or masked client-to-server) RFC 6455 frame.
pub async fn write_ws_frame<W: AsyncWrite + Unpin>(
    stream: &mut W,
    opcode: u8,
    payload: &[u8],
    mask_key: Option<[u8; 4]>,
) -> std::io::Result<()> {
    let mut header = Vec::with_capacity(14);
    header.push(0x80 | (opcode & 0x0F));
    let mask_bit = if mask_key.is_some() { 0x80 } else { 0x00 };
    if payload.len() <= 125 {
        #[allow(clippy::cast_possible_truncation)]
        header.push(mask_bit | (payload.len() as u8));
    } else if let Ok(len16) = u16::try_from(payload.len()) {
        header.push(mask_bit | 126);
        header.extend_from_slice(&len16.to_be_bytes());
    } else {
        let len64 = u64::try_from(payload.len()).map_err(|_| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "frame length overflow")
        })?;
        header.push(mask_bit | 127);
        header.extend_from_slice(&len64.to_be_bytes());
    }
    if let Some(key) = mask_key {
        header.extend_from_slice(&key);
    }
    stream.write_all(&header).await?;
    if let Some(key) = mask_key {
        let mut masked = Vec::with_capacity(payload.len());
        for (idx, &byte) in payload.iter().enumerate() {
            masked.push(byte ^ key[idx % 4]);
        }
        stream.write_all(&masked).await?;
    } else {
        stream.write_all(payload).await?;
    }
    stream.flush().await
}

/// Write an unmasked server binary frame (opcode `0x02`).
pub async fn write_server_binary<W: AsyncWrite + Unpin>(
    stream: &mut W,
    payload: &[u8],
) -> std::io::Result<()> {
    write_ws_frame(stream, 0x02, payload, None).await
}

/// Write an unmasked server Close frame (opcode `0x08`) with a 2-byte close code.
pub async fn write_server_close<W: AsyncWrite + Unpin>(
    stream: &mut W,
    code: u16,
) -> std::io::Result<()> {
    write_ws_frame(stream, 0x08, &code.to_be_bytes(), None).await
}

async fn read_exact_checked<R: AsyncRead + Unpin>(
    stream: &mut R,
    buf: &mut [u8],
) -> Result<(), WsFrameError> {
    let mut offset = 0;
    while offset < buf.len() {
        let n = stream
            .read(&mut buf[offset..])
            .await
            .map_err(|_| WsFrameError::Io)?;
        if n == 0 {
            return Err(WsFrameError::Eof);
        }
        offset += n;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc6455_sec_websocket_accept_vector_matches_spec() {
        // RFC 6455 §1.3 normative test vector.
        assert_eq!(
            compute_accept_key("dGhlIHNhbXBsZSBub25jZQ=="),
            "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
        );
    }
}
