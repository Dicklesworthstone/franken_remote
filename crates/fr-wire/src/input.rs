//! Allocation-free v0 input records over the existing FRD0 envelope.
//! See `PROTOCOL_INPUT.md`. Parsing does not grant input authority. The receiver
//! passes the authenticated direction, never a sender-supplied role flag.
use crate::record::{Reader, Writer};
use crate::{HEADER_BYTES, Kind, Record, WireError};
use fr_core::{
    ids::{
        CodecConfigurationGeneration, DisplayGeometryGeneration, InputLeaseId, InputTicketId,
        RecoveryGeneration, RemoteSessionId, ViewportMappingGeneration,
    },
    input::{
        DesktopPoint, InputCredentials, InputEvent, InputRequest, InputView, KeyTransition,
        PhysicalKey, PointerButton, PointerMode, ScrollUnit,
    },
    limits::ProtocolLimits,
};

/// Session, lease, ticket (16 bytes each), four generations and sequence (u64).
pub const INPUT_PREFIX_BYTES: usize = 88;
/// Per-action UTF-8 ceiling. Whole clipboard transfers are a different channel.
pub const MAX_TEXT_BYTES: usize = 4096;
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputDirection {
    ViewerToHost,
    HostToViewer,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputDelivery {
    Reliable,
    Datagram,
}

fn kind(event: InputEvent<'_>) -> Kind {
    match event {
        InputEvent::Key { .. } => Kind::Key,
        InputEvent::Button { .. } => Kind::Button,
        InputEvent::Pointer { .. } => Kind::Pointer,
        InputEvent::Relative { .. } => Kind::Relative,
        InputEvent::Scroll { .. } => Kind::Scroll,
        InputEvent::Text(_) => Kind::Text,
        InputEvent::Mode { .. } => Kind::InputMode,
    }
}
fn validate_route(
    direction: InputDirection,
    delivery: InputDelivery,
    kind: Kind,
) -> Result<(), WireError> {
    if direction != InputDirection::ViewerToHost {
        return Err(WireError::WrongRole);
    }
    if delivery == InputDelivery::Datagram && kind != Kind::Pointer {
        return Err(WireError::WrongChannel);
    }
    Ok(())
}
fn point(r: &mut Reader<'_>) -> Result<DesktopPoint, WireError> {
    Ok(DesktopPoint {
        x: r.u32()?.cast_signed(),
        y: r.u32()?.cast_signed(),
    })
}
fn put_point(w: &mut Writer<'_>, p: DesktopPoint) -> Result<(), WireError> {
    w.u32(p.x.cast_unsigned())?;
    w.u32(p.y.cast_unsigned())
}
fn opaque(r: &mut Reader<'_>) -> Result<u128, WireError> {
    Ok(u128::from_be_bytes(
        r.take(16)?.try_into().map_err(|_| WireError::Truncated)?,
    ))
}
fn boolean(r: &mut Reader<'_>) -> Result<bool, WireError> {
    match r.u8()? {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(WireError::InvalidValue),
    }
}

/// The caller additionally enforces its transport's negotiated datagram limit.
/// No heap allocation occurs, even for truncated or oversized declarations.
pub fn decode_input<'a>(
    bytes: &'a [u8],
    limits: &ProtocolLimits,
    binding: u32,
    direction: InputDirection,
    delivery: InputDelivery,
) -> Result<InputRequest<'a>, WireError> {
    if direction != InputDirection::ViewerToHost {
        return Err(WireError::WrongRole);
    }
    let record = Record::decode_bounded(
        bytes,
        limits.max_control_message_bytes() as usize,
        binding,
        None,
    )?;
    let k = record.kind();
    validate_route(direction, delivery, k)?;
    let mut r = record.reader(k)?;
    let credentials = InputCredentials {
        session: RemoteSessionId::from_raw(opaque(&mut r)?),
        lease: InputLeaseId::from_raw(opaque(&mut r)?),
        ticket: InputTicketId::from_raw(opaque(&mut r)?),
        view: InputView {
            geometry: DisplayGeometryGeneration::from_raw(r.u64()?),
            viewport: ViewportMappingGeneration::from_raw(r.u64()?),
            configuration: CodecConfigurationGeneration::from_raw(r.u64()?),
            recovery: RecoveryGeneration::from_raw(r.u64()?),
        },
    };
    let sequence = r.u64()?;
    let event = match k {
        Kind::Key => InputEvent::Key {
            key: PhysicalKey::new(r.u16()?).ok_or(WireError::InvalidValue)?,
            transition: match r.u8()? {
                0 => KeyTransition::Release,
                1 => KeyTransition::Press,
                2 => KeyTransition::Repeat,
                _ => return Err(WireError::InvalidValue),
            },
        },
        Kind::Button => InputEvent::Button {
            button: match r.u8()? {
                1 => PointerButton::Primary,
                2 => PointerButton::Secondary,
                3 => PointerButton::Middle,
                4 => PointerButton::Back,
                5 => PointerButton::Forward,
                _ => return Err(WireError::InvalidValue),
            },
            pressed: boolean(&mut r)?,
            position: point(&mut r)?,
            barrier: r.u64()?,
        },
        Kind::Pointer => InputEvent::Pointer {
            position: point(&mut r)?,
        },
        Kind::Relative => InputEvent::Relative {
            mode_epoch: r.u64()?,
            cumulative_x: r.u64()?.cast_signed(),
            cumulative_y: r.u64()?.cast_signed(),
        },
        Kind::Scroll => InputEvent::Scroll {
            position: point(&mut r)?,
            barrier: r.u64()?,
            x: r.u32()?.cast_signed(),
            y: r.u32()?.cast_signed(),
            unit: match r.u8()? {
                0 => ScrollUnit::Pixels,
                1 => ScrollUnit::Lines,
                _ => return Err(WireError::InvalidValue),
            },
        },
        Kind::Text => {
            let text = r.data()?;
            if text.is_empty() || text.len() > MAX_TEXT_BYTES {
                return Err(WireError::ResourceLimit);
            }
            InputEvent::Text(core::str::from_utf8(text).map_err(|_| WireError::InvalidValue)?)
        }
        Kind::InputMode => InputEvent::Mode {
            mode: match r.u8()? {
                0 => PointerMode::Absolute,
                1 => PointerMode::Relative,
                _ => return Err(WireError::InvalidValue),
            },
            epoch: r.u64()?,
        },
        _ => return Err(WireError::UnsupportedKind),
    };
    r.finish()?;
    Ok(InputRequest {
        credentials,
        sequence,
        event,
    })
}

pub fn encode_input(
    request: InputRequest<'_>,
    out: &mut [u8],
    limits: &ProtocolLimits,
    binding: u32,
    direction: InputDirection,
    delivery: InputDelivery,
) -> Result<usize, WireError> {
    let k = kind(request.event);
    validate_route(direction, delivery, k)?;
    let size = match request.event {
        InputEvent::Key { .. } => 3,
        InputEvent::Button { .. } => 18,
        InputEvent::Pointer { .. } => 8,
        InputEvent::Relative { .. } => 24,
        InputEvent::Scroll { .. } => 25,
        InputEvent::Mode { .. } => 9,
        InputEvent::Text(text) => {
            if text.is_empty() || text.len() > MAX_TEXT_BYTES {
                return Err(WireError::ResourceLimit);
            }
            4 + text.len()
        }
    };
    let mut w = Writer::record_bounded(
        out,
        limits.max_control_message_bytes() as usize,
        binding,
        k,
        INPUT_PREFIX_BYTES + size,
    )?;
    let c = request.credentials;
    for id in [c.session.as_raw(), c.lease.as_raw(), c.ticket.as_raw()] {
        w.put(&id.to_be_bytes())?;
    }
    for generation in [
        c.view.geometry.as_raw(),
        c.view.viewport.as_raw(),
        c.view.configuration.as_raw(),
        c.view.recovery.as_raw(),
        request.sequence,
    ] {
        w.u64(generation)?;
    }
    match request.event {
        InputEvent::Key { key, transition } => {
            w.u16(key.usage())?;
            w.u8(transition as u8)?;
        }
        InputEvent::Button {
            button,
            pressed,
            position,
            barrier,
        } => {
            w.u8(button as u8)?;
            w.u8(u8::from(pressed))?;
            put_point(&mut w, position)?;
            w.u64(barrier)?;
        }
        InputEvent::Pointer { position } => put_point(&mut w, position)?,
        InputEvent::Relative {
            mode_epoch,
            cumulative_x,
            cumulative_y,
        } => {
            w.u64(mode_epoch)?;
            w.u64(cumulative_x.cast_unsigned())?;
            w.u64(cumulative_y.cast_unsigned())?;
        }
        InputEvent::Scroll {
            position,
            barrier,
            x,
            y,
            unit,
        } => {
            put_point(&mut w, position)?;
            w.u64(barrier)?;
            w.u32(x.cast_unsigned())?;
            w.u32(y.cast_unsigned())?;
            w.u8(unit as u8)?;
        }
        InputEvent::Text(text) => w.data(text.as_bytes())?,
        InputEvent::Mode { mode, epoch } => {
            w.u8(mode as u8)?;
            w.u64(epoch)?;
        }
    }
    w.finish()
}

/// Maximum record size for the implemented text action (including FRD0 header).
pub const MAX_INPUT_RECORD_BYTES: usize = HEADER_BYTES + INPUT_PREFIX_BYTES + 4 + MAX_TEXT_BYTES;
