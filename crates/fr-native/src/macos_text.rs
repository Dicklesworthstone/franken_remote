//! Bounded native text handoff. A local editor is not an input authority:
//! the original session must still match this opaque context and admit the
//! unchanged commit through its one ordered event source.
#![forbid(unsafe_code)]

use fr_core::input::{CommittedText, MAX_COMMITTED_TEXT_BYTES, TextError};
use std::time::Duration;

pub(crate) const MAX_GESTURE_AGE: Duration = Duration::from_millis(100);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Refusal {
    Context,
    Timestamp,
    Expired,
    Encoding,
    Text(TextError),
}

/// One explicit native Send, consumed once. No Debug or Clone: typed text is
/// neither diagnostics nor a replayable command. A fresh clock sample never
/// replaces the original `NSEvent` timestamp.
pub(crate) struct Commit {
    pub(crate) context: u64,
    pub(crate) captured: Duration,
    pub(crate) text: CommittedText,
}

impl Commit {
    pub(crate) fn from_native(
        context: u64,
        captured: f64,
        bytes: &[u8],
        now: Duration,
    ) -> Result<Self, Refusal> {
        if context == 0 {
            return Err(Refusal::Context);
        }
        let captured = Duration::try_from_secs_f64(captured)
            .ok()
            .filter(|at| !at.is_zero())
            .ok_or(Refusal::Timestamp)?;
        let age = now.checked_sub(captured).ok_or(Refusal::Timestamp)?;
        if age >= MAX_GESTURE_AGE {
            return Err(Refusal::Expired);
        }
        if bytes.len() > MAX_COMMITTED_TEXT_BYTES {
            return Err(Refusal::Text(TextError::TooLong));
        }
        let value = std::str::from_utf8(bytes).map_err(|_| Refusal::Encoding)?;
        let text = CommittedText::new(value).map_err(Refusal::Text)?;
        Ok(Self {
            context,
            captured,
            text,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn capture(value: &[u8]) -> Result<Commit, Refusal> {
        Commit::from_native(7, 1.0, value, Duration::from_millis(1001))
    }

    #[test]
    fn retains_exact_unicode_and_the_original_gesture() {
        let value = "e\u{301}é漢🙂\r\n\t\0";
        let commit = capture(value.as_bytes()).unwrap();
        assert_eq!(commit.context, 7);
        assert_eq!(commit.captured, Duration::from_secs(1));
        assert_eq!(commit.text.as_str().as_bytes(), value.as_bytes());
    }

    #[test]
    fn owns_its_complete_payload_before_native_storage_is_cleared() {
        let mut native = "original 漢🙂".as_bytes().to_vec();
        let commit = capture(&native).unwrap();
        native.fill(0);
        assert_eq!(commit.text.as_str(), "original 漢🙂");
    }

    #[test]
    fn never_truncates_supplementary_or_oversized_text() {
        let exact = "🙂".repeat(MAX_COMMITTED_TEXT_BYTES / 4);
        assert_eq!(capture(exact.as_bytes()).unwrap().text.as_str(), exact);
        let oversized = format!("{exact}x");
        assert!(matches!(
            capture(oversized.as_bytes()),
            Err(Refusal::Text(TextError::TooLong))
        ));
        assert!(matches!(
            capture(&exact.as_bytes()[..exact.len() - 1]),
            Err(Refusal::Encoding)
        ));
    }

    #[test]
    fn rejects_empty_invalid_and_unbound_native_payloads() {
        assert!(matches!(capture(b""), Err(Refusal::Text(TextError::Empty))));
        assert!(matches!(capture(&[0xff]), Err(Refusal::Encoding)));
        assert!(matches!(
            Commit::from_native(0, 1.0, b"x", Duration::from_secs(1)),
            Err(Refusal::Context)
        ));
    }

    #[test]
    fn stale_send_cannot_acquire_a_new_capture_time() {
        for now in [1100, 1101, 60_000] {
            assert!(matches!(
                Commit::from_native(7, 1.0, b"x", Duration::from_millis(now)),
                Err(Refusal::Expired)
            ));
        }
        assert!(Commit::from_native(7, 1.0, b"x", Duration::from_millis(1099)).is_ok());
    }

    #[test]
    fn invalid_future_and_rollback_timestamps_refuse() {
        for captured in [0.0, -1.0, f64::NAN, f64::INFINITY, 1.001] {
            assert!(matches!(
                Commit::from_native(7, captured, b"x", Duration::from_secs(1)),
                Err(Refusal::Timestamp)
            ));
        }
    }
}
