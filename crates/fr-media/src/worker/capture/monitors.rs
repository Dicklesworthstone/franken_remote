//! Private display discovery on the same capture worker that owns the X connection.
//! These bounded records carry no fabricated network binding, OS names or paths.
use super::super::{CONFIG_BYTES, Configuration, Error};
use fr_core::{ids::DisplayGeometryGeneration, limits::ProtocolLimits};
use fr_wire::display::{Catalog, Display, ENTRY_BYTES, MAX_DISPLAYS, Select};

pub const CATALOG_MAX_BYTES: usize = 9 + ENTRY_BYTES * MAX_DISPLAYS;
pub const SELECTED_CONFIG_BYTES: usize = CONFIG_BYTES + 24;

pub fn encode_catalog(catalog: Catalog, limits: &ProtocolLimits) -> Result<Vec<u8>, Error> {
    Catalog::new(catalog.revision(), catalog.displays(), limits).map_err(|_| Error::Malformed)?;
    let length = 9 + catalog.displays().len() * ENTRY_BYTES;
    if length > limits.max_control_message_bytes() as usize {
        return Err(Error::ResourceLimit);
    }
    let mut out = Vec::new();
    out.try_reserve_exact(length)
        .map_err(|_| Error::Allocation)?;
    out.extend_from_slice(&catalog.revision().to_be_bytes());
    out.push(u8::try_from(catalog.displays().len()).map_err(|_| Error::ResourceLimit)?);
    for d in catalog.displays() {
        out.extend_from_slice(&d.handle.to_be_bytes());
        out.extend_from_slice(&d.geometry.as_raw().to_be_bytes());
        out.extend_from_slice(&d.x.to_be_bytes());
        out.extend_from_slice(&d.y.to_be_bytes());
        for n in [
            d.pixel_width,
            d.pixel_height,
            d.logical_width,
            d.logical_height,
            d.scale_numerator,
            d.scale_denominator,
        ] {
            out.extend_from_slice(&n.to_be_bytes());
        }
        out.push(d.rotation);
    }
    Ok(out)
}
pub fn decode_catalog(bytes: &[u8], limits: &ProtocolLimits) -> Result<Catalog, Error> {
    if bytes.len() < 9
        || bytes.len() > CATALOG_MAX_BYTES
        || bytes.len() > limits.max_control_message_bytes() as usize
    {
        return Err(Error::ResourceLimit);
    }
    let revision = u64::from_be_bytes(bytes[..8].try_into().map_err(|_| Error::Malformed)?);
    let count = usize::from(bytes[8]);
    if count > MAX_DISPLAYS || bytes.len() != 9 + count * ENTRY_BYTES {
        return Err(Error::Malformed);
    }
    // Count and total size are checked before reserving even this bounded vector.
    let mut entries = Vec::new();
    entries
        .try_reserve_exact(count)
        .map_err(|_| Error::Allocation)?;
    for b in bytes[9..].as_chunks::<ENTRY_BYTES>().0 {
        let unsigned = |offset| -> Result<u32, Error> {
            Ok(u32::from_be_bytes(
                b[offset..offset + 4]
                    .try_into()
                    .map_err(|_| Error::Malformed)?,
            ))
        };
        entries.push(Display {
            handle: u128::from_be_bytes(b[..16].try_into().map_err(|_| Error::Malformed)?),
            geometry: DisplayGeometryGeneration::from_raw(u64::from_be_bytes(
                b[16..24].try_into().map_err(|_| Error::Malformed)?,
            )),
            x: i32::from_be_bytes(b[24..28].try_into().map_err(|_| Error::Malformed)?),
            y: i32::from_be_bytes(b[28..32].try_into().map_err(|_| Error::Malformed)?),
            pixel_width: unsigned(32)?,
            pixel_height: unsigned(36)?,
            logical_width: unsigned(40)?,
            logical_height: unsigned(44)?,
            scale_numerator: unsigned(48)?,
            scale_denominator: unsigned(52)?,
            rotation: b[56],
        });
    }
    Catalog::new(revision, &entries, limits).map_err(|_| Error::Malformed)
}
/// Retain the selected catalog revision and handle with the exact codec setup.
/// Full-display geometry is checked against the worker-owned catalog before FFI.
pub fn encode_configuration(
    config: Configuration,
    selected: Select,
    catalog: Catalog,
) -> Result<Vec<u8>, Error> {
    let display = catalog
        .selected(selected)
        .map_err(|_| Error::GeometryChanged)?;
    if config.width != display.pixel_width || config.height != display.pixel_height {
        return Err(Error::GeometryChanged);
    }
    let mut body = config.encode()?;
    body.try_reserve_exact(24).map_err(|_| Error::Allocation)?;
    body.extend_from_slice(&selected.revision.to_be_bytes());
    body.extend_from_slice(&selected.handle.to_be_bytes());
    Ok(body)
}
pub fn decode_configuration(
    bytes: &[u8],
    catalog: Catalog,
) -> Result<(Configuration, Display), Error> {
    if bytes.len() != SELECTED_CONFIG_BYTES {
        return Err(Error::Malformed);
    }
    let config = Configuration::decode(&bytes[..CONFIG_BYTES])?;
    let revision = u64::from_be_bytes(
        bytes[CONFIG_BYTES..CONFIG_BYTES + 8]
            .try_into()
            .map_err(|_| Error::Malformed)?,
    );
    let handle = u128::from_be_bytes(
        bytes[CONFIG_BYTES + 8..]
            .try_into()
            .map_err(|_| Error::Malformed)?,
    );
    if revision != catalog.revision() {
        return Err(Error::GeometryChanged);
    }
    let selection = catalog
        .selection(handle)
        .map_err(|_| Error::GeometryChanged)?;
    let expected = encode_configuration(config, selection, catalog)?;
    if expected != bytes {
        return Err(Error::Malformed);
    }
    Ok((
        config,
        catalog
            .selected(selection)
            .map_err(|_| Error::GeometryChanged)?,
    ))
}
