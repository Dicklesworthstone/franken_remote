//! Checked counts and byte totals for one actual retained-handle selection.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Error {
    Count,
    Directory(usize),
    Bytes(usize),
}

/// Directory sizes are unknown until an authorized walk and cannot masquerade
/// as zero-byte regular files. An overflowing total never becomes a truncated
/// report or a partly accepted selection.
pub(super) fn check(
    sizes: impl ExactSizeIterator<Item = Option<u64>>,
    maximum: usize,
) -> Result<(u8, u64), Error> {
    let count = sizes.len();
    let selected = u8::try_from(count)
        .ok()
        .filter(|value| *value != 0 && count <= maximum)
        .ok_or(Error::Count)?;
    let mut bytes = 0_u64;
    let mut seen = 0;
    for (index, size) in sizes.enumerate() {
        if index >= count {
            return Err(Error::Count);
        }
        bytes = bytes
            .checked_add(size.ok_or(Error::Directory(index))?)
            .ok_or(Error::Bytes(index))?;
        seen = index + 1;
    }
    if seen != count {
        return Err(Error::Count);
    }
    Ok((selected, bytes))
}

#[cfg(test)]
mod tests {
    use super::{Error, check};

    #[test]
    fn selection_count_is_bounded_before_any_size_is_consumed() {
        assert_eq!(check(std::iter::empty(), 8), Err(Error::Count));
        assert_eq!(check([Some(1)].into_iter(), 0), Err(Error::Count));
        assert_eq!(check([Some(1); 9].into_iter(), 8), Err(Error::Count));
        assert_eq!(check([Some(1); 256].into_iter(), 256), Err(Error::Count));
        assert_eq!(check([Some(1); 8].into_iter(), 8), Ok((8, 8)));
    }

    #[test]
    fn empty_files_count_as_selections_and_all_regular_file_sizes_are_added() {
        assert_eq!(check([Some(0)].into_iter(), 8), Ok((1, 0)));
        assert_eq!(check([Some(0); 8].into_iter(), 8), Ok((8, 0)));
        assert_eq!(
            check([Some(3), Some(0), Some(5), Some(100)].into_iter(), 8),
            Ok((4, 108))
        );
    }

    #[test]
    fn a_directory_refuses_the_entire_selection_at_its_actual_index() {
        for index in 0..8 {
            let mut sizes = [Some(10); 8];
            sizes[index] = None;
            assert_eq!(check(sizes.into_iter(), 8), Err(Error::Directory(index)));
        }
    }

    #[test]
    fn a_byte_total_overflow_refuses_the_entire_selection_without_saturating() {
        assert_eq!(
            check([Some(u64::MAX), Some(0)].into_iter(), 8),
            Ok((2, u64::MAX))
        );
        assert_eq!(
            check([Some(u64::MAX), Some(1)].into_iter(), 8),
            Err(Error::Bytes(1))
        );
        assert_eq!(
            check([Some(u64::MAX - 1), Some(1), Some(1)].into_iter(), 8),
            Err(Error::Bytes(2))
        );
    }
}
