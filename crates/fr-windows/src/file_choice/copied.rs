//! Bounded Unicode `CF_HDROP` data, after its clipboard-owned bytes are copied.
//! No OS handle, pathname authority or file content crosses this decoder.

use super::{Error, MAX_PATH_UNITS, MAX_SELECTED_FILES, selection};

const HEADER_BYTES: usize = 20;
/// Includes the DROPFILES header, offset padding and allocator padding. The
/// selected paths have the separate, smaller aggregate UTF-16 bound below.
pub const MAX_COPIED_BYTES: usize = 128 * 1024;

/// Decode one complete, nonempty DROPFILES list without trusting native
/// string-scanning APIs. `pFiles` is a byte offset, and nonzero `fWide` means
/// UTF-16. Reject ANSI, partial paths, unsafe spellings and excess members as
/// one selection. `GlobalSize` may include allocator padding after the list's
/// double NUL; it is bounded above, but is not another selected pathname.
pub fn copied_paths(bytes: &[u8]) -> Result<Vec<Vec<u16>>, Error> {
    if bytes.len() < HEADER_BYTES || bytes.len() > MAX_COPIED_BYTES {
        return Err(Error::InvalidPath);
    }
    let offset = usize::try_from(u32::from_le_bytes(
        bytes[..4].try_into().map_err(|_| Error::InvalidPath)?,
    ))
    .map_err(|_| Error::InvalidPath)?;
    let wide = u32::from_le_bytes(bytes[16..20].try_into().map_err(|_| Error::InvalidPath)?);
    if wide == 0
        || offset < HEADER_BYTES
        || !offset.is_multiple_of(2)
        || offset.checked_add(4).is_none_or(|end| end > bytes.len())
    {
        return Err(Error::InvalidPath);
    }
    let mut units = bytes[offset..]
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| u16::from_le_bytes(*pair));
    let mut paths = Vec::with_capacity(MAX_SELECTED_FILES);
    let mut path = Vec::new();
    let mut selected_units = 0_usize;
    while let Some(unit) = units.next() {
        if unit == 0 && path.is_empty() {
            return if paths.is_empty() {
                Err(if units.next() == Some(0) {
                    Error::NoCopiedFiles
                } else {
                    Error::InvalidPath
                })
            } else {
                Ok(paths)
            };
        }
        selected_units = selected_units
            .checked_add(1)
            .filter(|count| *count <= MAX_PATH_UNITS)
            .ok_or(Error::InvalidPath)?;
        if paths.len() == MAX_SELECTED_FILES {
            return Err(Error::InvalidPath);
        }
        if unit == 0 {
            if !selection::file_path(&path) || paths.contains(&path) {
                return Err(Error::InvalidPath);
            }
            paths.push(std::mem::take(&mut path));
        } else {
            path.push(unit);
        }
    }
    Err(Error::InvalidPath)
}

#[cfg(test)]
#[path = "copied/tests.rs"]
mod tests;
