//! The bounded Explorer-style `OPENFILENAMEW` result, before any file opens.

use super::{Error, MAX_PATH_UNITS};

pub const MAX_SELECTED_FILES: usize = 8;
const SEPARATOR: u16 = 0x005c;

/// Decode the exact native selection, retaining spelling and selection order.
/// `file_offset` is `OPENFILENAMEW::nFileOffset`, in UTF-16 code units: either
/// the basename in a single full path, or the first name after a directory.
/// Every returned path is unterminated. Both the native buffer and all expanded
/// paths together are bounded; no malformed suffix or excess file is ignored.
pub fn selected_paths(units: &[u16], file_offset: usize) -> Result<Vec<Vec<u16>>, Error> {
    if units.len() > MAX_PATH_UNITS {
        return Err(Error::InvalidPath);
    }
    let end = units
        .iter()
        .position(|unit| *unit == 0)
        .ok_or(Error::InvalidPath)?;
    if file_offset <= end {
        let path = super::path_units(units)?;
        let name_start = path
            .iter()
            .rposition(|unit| *unit == SEPARATOR)
            .map(|index| index + 1)
            .ok_or(Error::InvalidPath)?;
        if file_offset != name_start || !file_path(path) {
            return Err(Error::InvalidPath);
        }
        return Ok(vec![path.to_vec()]);
    }
    let root = &units[..end];
    if file_offset != end + 1 || !directory(root) {
        return Err(Error::InvalidPath);
    }
    let mut remaining = &units[file_offset..];
    let mut paths = Vec::with_capacity(MAX_SELECTED_FILES);
    let mut expanded_units = 0_usize;
    loop {
        let end = remaining
            .iter()
            .position(|unit| *unit == 0)
            .ok_or(Error::InvalidPath)?;
        let name = &remaining[..end];
        if name.is_empty() {
            // A single choice has the full-path form. Require the extra NUL
            // and reject a hidden suffix instead of sending a valid prefix.
            if paths.len() < 2 || remaining.iter().any(|unit| *unit != 0) {
                return Err(Error::InvalidPath);
            }
            return Ok(paths);
        }
        if paths.len() == MAX_SELECTED_FILES || !component(name) {
            return Err(Error::InvalidPath);
        }
        let separator = usize::from(root.last() != Some(&SEPARATOR));
        let path_units = root.len() + separator + name.len();
        expanded_units = expanded_units
            .checked_add(path_units + 1)
            .filter(|total| *total <= MAX_PATH_UNITS)
            .ok_or(Error::InvalidPath)?;
        let mut path = Vec::with_capacity(path_units);
        path.extend_from_slice(root);
        if separator != 0 {
            path.push(SEPARATOR);
        }
        path.extend_from_slice(name);
        if paths.contains(&path) {
            return Err(Error::InvalidPath);
        }
        paths.push(path);
        remaining = &remaining[end + 1..];
    }
}

/// Shared by the native dialog and the clipboard's complete absolute paths.
/// This only validates spelling; the existing retained-handle owner still
/// decides whether the path names an admissible regular local file.
pub(super) fn file_path(path: &[u16]) -> bool {
    path.iter()
        .rposition(|unit| *unit == SEPARATOR)
        .is_some_and(|separator| {
            directory(&path[..=separator]) && component(&path[separator + 1..])
        })
}

/// Match the ordinary drive paths accepted by the retained-handle boundary.
/// UNC, device namespaces, drive-relative paths and normalization-sensitive
/// components never become a different pathname through concatenation.
fn directory(path: &[u16]) -> bool {
    if path.len() < 3
        || !u8::try_from(path[0]).is_ok_and(|drive| drive.is_ascii_alphabetic())
        || path[1] != u16::from(b':')
        || path[2] != SEPARATOR
    {
        return false;
    }
    if path.len() == 3 {
        return true;
    }
    let suffix = &path[3..];
    let suffix = suffix.strip_suffix(&[SEPARATOR]).unwrap_or(suffix);
    !suffix.is_empty() && suffix.split(|unit| *unit == SEPARATOR).all(component)
}

fn component(units: &[u16]) -> bool {
    let Ok(name) = String::from_utf16(units) else {
        return false;
    };
    if name.is_empty()
        || name.ends_with(['.', ' '])
        || name.chars().any(|value| {
            value < ' ' || matches!(value, '\\' | '/' | ':' | '*' | '?' | '"' | '<' | '>' | '|')
        })
    {
        return false;
    }
    let stem = name
        .split('.')
        .next()
        .unwrap_or_default()
        .trim_end_matches(' ');
    let upper = stem.to_ascii_uppercase();
    !matches!(upper.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        && !["COM", "LPT"].into_iter().any(|prefix| {
            upper.strip_prefix(prefix).is_some_and(|number| {
                matches!(
                    number,
                    "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
                )
            })
        })
}

#[cfg(test)]
#[path = "selection/tests.rs"]
mod tests;
