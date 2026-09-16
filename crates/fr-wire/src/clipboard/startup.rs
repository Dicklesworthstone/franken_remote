//! Optional running-session readiness, on the ORIGINAL reliable control stream.
//! Neither this message nor its consent bit grants input or native OS authority.
use crate::{HEADER_BYTES, Kind, Record, WireError, negotiation::ControlBinding, record::Writer};
use fr_core::{clipboard::Binding, limits::ProtocolLimits};

/// Selected in addition to native-clipboard-attachment and controller-text-clipboard.
/// Peers without this capability continue to use the explicit attachment API.
pub const CAPABILITY: &str = "native-clipboard-startup";
pub const VERSION: u16 = 1;
pub const RECORD_BYTES: usize = HEADER_BYTES + 16 * 4 + 4 + 1;

fn validate(parent: ControlBinding, scope: Binding, channel: u32) -> Result<(), WireError> {
    if parent.id == 0
        || parent.host_boot.as_raw() == 0
        || parent.os_session.as_raw() == 0
        || !scope.valid()
        || scope.session != parent.remote_session
        || channel == 0
        || channel == parent.id
    {
        return Err(WireError::InvalidBinding);
    }
    Ok(())
}
/// Send exactly once, only after this endpoint has completed the matching native
/// clipboard attachment. Sender direction comes from the authenticated control
/// route, not a payload flag. Local consent must be obtained independently.
pub fn encode(
    parent: ControlBinding,
    scope: Binding,
    channel: u32,
    consent: bool,
    limits: &ProtocolLimits,
    out: &mut [u8],
) -> Result<usize, WireError> {
    validate(parent, scope, channel)?;
    let mut w = Writer::record_bounded(
        out,
        limits.max_control_message_bytes() as usize,
        parent.id,
        Kind::ClipboardReady,
        RECORD_BYTES - HEADER_BYTES,
    )?;
    for id in [
        parent.host_boot.as_raw(),
        parent.os_session.as_raw(),
        scope.session.as_raw(),
        scope.lease.as_raw(),
    ] {
        w.put(&id.to_be_bytes())?;
    }
    w.u32(channel)?;
    w.u8(u8::from(consent))?;
    w.finish()
}
/// Only a matching, positively selected reliable control route may call this.
/// Validate the entire original host/OS/session/lease scope and attached channel.
/// Returns the peer's consent declaration, not permission to bypass local checks.
pub fn decode(
    bytes: &[u8],
    parent: ControlBinding,
    scope: Binding,
    channel: u32,
    limits: &ProtocolLimits,
) -> Result<bool, WireError> {
    validate(parent, scope, channel)?;
    let record = Record::decode_bounded(
        bytes,
        (limits.max_control_message_bytes() as usize).min(RECORD_BYTES),
        parent.id,
        None,
    )?;
    let mut r = record.reader(Kind::ClipboardReady)?;
    for id in [
        parent.host_boot.as_raw(),
        parent.os_session.as_raw(),
        scope.session.as_raw(),
        scope.lease.as_raw(),
    ] {
        if r.take(16)? != id.to_be_bytes() {
            return Err(WireError::InvalidBinding);
        }
    }
    if r.u32()? != channel {
        return Err(WireError::InvalidBinding);
    }
    let consent = match r.u8()? {
        0 => false,
        1 => true,
        _ => return Err(WireError::InvalidValue),
    };
    r.finish()?;
    Ok(consent)
}
