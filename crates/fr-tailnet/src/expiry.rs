//! Strict RFC3339 timestamps from Go's `KeyExpiry`; no permissive date guessing.
use crate::Error;

/// None is the documented Go zero time (non-expiring), not an invalid date.
pub(crate) fn unix_micros(s: &str) -> Result<Option<u64>, Error> {
    if s == "0001-01-01T00:00:00Z" {
        return Ok(None);
    }
    let b = s.as_bytes();
    if b.len() < 20
        || b.get(4) != Some(&b'-')
        || b.get(7) != Some(&b'-')
        || b.get(10) != Some(&b'T')
        || b.get(13) != Some(&b':')
        || b.get(16) != Some(&b':')
    {
        return Err(Error::MalformedMetadata);
    }
    let number = |range: std::ops::Range<usize>| -> Result<i64, Error> {
        b.get(range)
            .ok_or(Error::MalformedMetadata)?
            .iter()
            .try_fold(0i64, |n, c| {
                if !c.is_ascii_digit() {
                    return Err(Error::MalformedMetadata);
                }
                Ok(n * 10 + i64::from(c - b'0'))
            })
    };
    let (year, month, day) = (number(0..4)?, number(5..7)?, number(8..10)?);
    let (hour, minute, second) = (number(11..13)?, number(14..16)?, number(17..19)?);
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let days = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => 0,
    };
    if year == 0 || day < 1 || day > days || hour > 23 || minute > 59 || second > 59 {
        return Err(Error::MalformedMetadata);
    }
    let mut cursor = 19;
    let mut fraction = 0u64;
    if b.get(cursor) == Some(&b'.') {
        cursor += 1;
        let begin = cursor;
        while b.get(cursor).is_some_and(u8::is_ascii_digit) {
            if cursor - begin >= 9 {
                return Err(Error::MalformedMetadata);
            }
            if cursor - begin < 6 {
                fraction = fraction * 10 + u64::from(b[cursor] - b'0');
            }
            cursor += 1;
        }
        if begin == cursor {
            return Err(Error::MalformedMetadata);
        }
        for _ in cursor - begin..6 {
            fraction *= 10;
        }
    }
    let offset = match b.get(cursor..) {
        Some(b"Z") => 0,
        Some(tz) if tz.len() == 6 && (tz[0] == b'+' || tz[0] == b'-') && tz[3] == b':' => {
            let h = number(cursor + 1..cursor + 3)?;
            let m = number(cursor + 4..cursor + 6)?;
            if h > 23 || m > 59 {
                return Err(Error::MalformedMetadata);
            }
            (h * 3600 + m * 60) * if tz[0] == b'+' { 1 } else { -1 }
        }
        _ => return Err(Error::MalformedMetadata),
    };
    let y = year - i64::from(month <= 2);
    let era = y / 400;
    let yoe = y - era * 400;
    let mp = month + if month > 2 { -3 } else { 9 };
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let seconds = days * 86400 + hour * 3600 + minute * 60 + second - offset;
    // Past pre-epoch dates are expired, never treated as non-expiring.
    if seconds < 0 {
        return Err(Error::KeyExpired);
    }
    Ok(Some(
        u64::try_from(seconds).map_err(|_| Error::Clock)? * 1_000_000 + fraction,
    ))
}
