//! HTTP/3 and WebTransport framing helpers over Asupersync QUIC.

use asupersync::http::h3_native::{
    H3ConnectionConfig, H3Frame, H3QpackMode, H3ResponseHead,
    H3Settings, qpack_decode_request_field_section, qpack_encode_response_field_section,
};
use asupersync::net::quic_core::{decode_varint, encode_varint};

pub const H3_SETTING_ENABLE_CONNECT_PROTOCOL: u64 = 0x08;
pub const H3_SETTING_H3_DATAGRAM: u64 = 0x33;
pub const H3_SETTING_H3_DATAGRAM_DRAFT04: u64 = 0xffd277;
pub const H3_SETTING_WEBTRANS_DRAFT00: u64 = 0x2b603742;
pub const H3_SETTING_WEBTRANS_MAX_SESSIONS_DRAFT07: u64 = 0xc671706a;

pub const WT_UNI_STREAM_TYPE: u64 = 0x54;
pub const WT_BIDI_STREAM_TYPE: u64 = 0x41;

/// Encode the server's control stream prologue: stream type 0x00 + SETTINGS frame.
pub fn server_control_stream_bytes() -> Vec<u8> {
    let mut bytes = Vec::new();
    // Unidirectional stream type 0x00 (Control stream)
    encode_varint(0x00, &mut bytes).expect("encode stream type");

    let mut settings = H3Settings {
        enable_connect_protocol: Some(true),
        h3_datagram: Some(true),
        qpack_max_table_capacity: Some(0),
        qpack_blocked_streams: Some(0),
        ..H3Settings::default()
    };
    // Include WebTransport draft-02 setting (0x2b603742) required by Chrome
    settings.unknown.push(asupersync::http::h3_native::UnknownSetting {
        id: H3_SETTING_WEBTRANS_DRAFT00,
        value: 1,
    });
    // Include WebTransport draft-07+ max sessions setting (0xc671706a)
    settings.unknown.push(asupersync::http::h3_native::UnknownSetting {
        id: H3_SETTING_WEBTRANS_MAX_SESSIONS_DRAFT07,
        value: 16,
    });
    // Include Datagram draft-04 setting as well for broad compatibility
    settings.unknown.push(asupersync::http::h3_native::UnknownSetting {
        id: H3_SETTING_H3_DATAGRAM_DRAFT04,
        value: 1,
    });

    let frame = H3Frame::Settings(settings);
    frame.encode(&mut bytes).expect("encode settings frame");
    bytes
}

/// Parsed extended CONNECT request for WebTransport.
#[derive(Debug, Clone)]
pub struct WebTransportConnectRequest {
    pub stream_id: u64,
    pub path: String,
    pub origin: Option<String>,
    pub authority: Option<String>,
    pub draft: Option<String>,
}

/// Try to parse an extended CONNECT request from a request stream's buffered bytes.
pub fn parse_connect_request(
    stream_id: u64,
    bytes: &[u8],
) -> Result<Option<(WebTransportConnectRequest, usize)>, String> {
    let config = H3ConnectionConfig::default();
    let (frame, consumed) = match H3Frame::decode(bytes, &config) {
        Ok(res) => res,
        Err(asupersync::http::h3_native::H3NativeError::UnexpectedEof) => return Ok(None),
        Err(e) => return Err(format!("failed to decode H3 frame on stream {stream_id}: {e}")),
    };

    let field_block = match frame {
        H3Frame::Headers(block) => block,
        other => return Err(format!("expected HEADERS frame, got {other:?}")),
    };

    let request_head =
        qpack_decode_request_field_section(&field_block, H3QpackMode::StaticOnly, None)
            .map_err(|e| format!("QPACK decode failed: {e}"))?;

    let pseudo = &request_head.pseudo;
    if pseudo.method.as_deref() != Some("CONNECT") {
        return Err(format!("expected :method CONNECT, got {:?}", pseudo.method));
    }
    if pseudo.protocol.as_deref() != Some("webtransport") {
        return Err(format!(
            "expected :protocol webtransport, got {:?}",
            pseudo.protocol
        ));
    }

    let path = pseudo.path.clone().unwrap_or_else(|| "/".to_string());
    let authority = pseudo.authority.clone();
    let mut origin = None;
    let mut draft = None;

    for (name, val) in &request_head.headers {
        let lower = name.to_ascii_lowercase();
        if lower == "origin" {
            origin = Some(val.clone());
        } else if lower.starts_with("sec-webtransport-http3-draft") {
            draft = Some(val.clone());
        }
    }

    Ok(Some((
        WebTransportConnectRequest {
            stream_id,
            path,
            origin,
            authority,
            draft,
        },
        consumed,
    )))
}

/// Encode 200 OK response for WebTransport session establishment.
pub fn encode_connect_response_200(draft: Option<&str>) -> Vec<u8> {
    let mut fields = Vec::new();
    if let Some(d) = draft {
        fields.push(("sec-webtransport-http3-draft".to_string(), d.to_string()));
        fields.push(("sec-webtransport-http3-draft02".to_string(), "1".to_string()));
    } else {
        fields.push(("sec-webtransport-http3-draft02".to_string(), "1".to_string()));
    }
    let response_head = H3ResponseHead::new(200, fields).expect("valid 200 response");
    let block = qpack_encode_response_field_section(&response_head)
        .expect("encode 200 response field section");
    let frame = H3Frame::Headers(block);
    let mut wire = Vec::new();
    frame.encode(&mut wire).expect("encode headers frame");
    wire
}

/// Encode 403 Forbidden response for rejected origin.
pub fn encode_connect_response_403() -> Vec<u8> {
    let response_head = H3ResponseHead::new(403, vec![]).expect("valid 403 response");
    let block = qpack_encode_response_field_section(&response_head)
        .expect("encode 403 response field section");
    let frame = H3Frame::Headers(block);
    let mut wire = Vec::new();
    frame.encode(&mut wire).expect("encode headers frame");
    wire
}

/// Encode an RFC 9297 HTTP/3 Datagram with quarter_stream_id prefix.
pub fn encode_h3_datagram(session_stream_id: u64, payload: &[u8]) -> Vec<u8> {
    let quarter_stream_id = session_stream_id / 4;
    let mut out = Vec::with_capacity(payload.len() + 8);
    encode_varint(quarter_stream_id, &mut out).expect("encode quarter stream id");
    out.extend_from_slice(payload);
    out
}

/// Decode an RFC 9297 HTTP/3 Datagram, verifying session quarter_stream_id.
pub fn decode_h3_datagram(
    session_stream_id: u64,
    datagram_bytes: &[u8],
) -> Result<Vec<u8>, String> {
    let expected_quarter_stream_id = session_stream_id / 4;
    let (quarter_stream_id, n) = decode_varint(datagram_bytes)
        .map_err(|_| "invalid quarter_stream_id varint in datagram".to_string())?;
    if quarter_stream_id != expected_quarter_stream_id {
        return Err(format!(
            "datagram quarter_stream_id mismatch: expected {expected_quarter_stream_id}, got {quarter_stream_id}"
        ));
    }
    Ok(datagram_bytes[n..].to_vec())
}
