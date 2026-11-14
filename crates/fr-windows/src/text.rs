//! Bounded native text and one-use local draft custody. No platform calls,
//! session authority or synthetic physical keys live in this policy.
#![forbid(unsafe_code)]

use fr_core::input::MAX_COMMITTED_TEXT_BYTES;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// Every accepted UTF-8 scalar uses at least as many bytes as UTF-16 units.
/// This bounds the native EDIT allocation independently of the final byte check.
pub const MAX_UTF16_UNITS: usize = MAX_COMMITTED_TEXT_BYTES;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InvalidText {
    Empty,
    Encoding,
    Nul,
    TooLong,
}

/// Decode the complete native value, never replace bad surrogates, truncate a
/// suffix or turn one oversized commit into separately retryable chunks.
pub fn from_utf16(units: &[u16]) -> Result<String, InvalidText> {
    if units.is_empty() {
        return Err(InvalidText::Empty);
    }
    if units.len() > MAX_UTF16_UNITS {
        return Err(InvalidText::TooLong);
    }
    if units.contains(&0) {
        return Err(InvalidText::Nul);
    }
    let text = String::from_utf16(units).map_err(|_| InvalidText::Encoding)?;
    if text.len() > MAX_COMMITTED_TEXT_BYTES {
        text.into_bytes().fill(0);
        return Err(InvalidText::TooLong);
    }
    Ok(text)
}

/// Opening an editor captures exactly one original context. Send, cancellation,
/// native truncation and context loss all consume it; no method can replace it.
pub struct Draft<C> {
    context: Option<C>,
}

impl<C> Draft<C> {
    pub const fn new(context: C) -> Self {
        Self {
            context: Some(context),
        }
    }

    pub const fn context(&self) -> Option<&C> {
        self.context.as_ref()
    }

    pub fn take(&mut self) -> Option<C> {
        self.context.take()
    }

    pub fn invalidate(&mut self) {
        self.context = None;
    }
}

/// Independent of a borrowed native window or editor buffer. Nested lifecycle
/// callbacks invalidate an in-flight Send before they attempt native cleanup.
/// No reset: another editor must create its own new lifetime.
#[derive(Default)]
pub struct Lifetime {
    invalid: AtomicBool,
    dismissed: AtomicBool,
    revision: AtomicU64,
}

impl Lifetime {
    pub fn invalidate(&self) {
        self.invalid.store(true, Ordering::SeqCst);
    }

    pub fn dismiss(&self) {
        self.invalidate();
        self.dismissed.store(true, Ordering::SeqCst);
    }

    pub fn valid(&self) -> bool {
        !self.invalid.load(Ordering::SeqCst)
    }

    pub fn dismissed(&self) -> bool {
        self.dismissed.load(Ordering::SeqCst)
    }

    /// Native edit notifications are independent of the borrowed editor.
    /// Exhaustion invalidates permanently instead of wrapping to an old read.
    pub fn changed(&self) {
        if self
            .revision
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |value| {
                value.checked_add(1)
            })
            .is_err()
        {
            self.invalidate();
        }
    }

    pub fn revision(&self) -> u64 {
        self.revision.load(Ordering::SeqCst)
    }

    pub fn unchanged(&self, revision: u64) -> bool {
        self.valid() && self.revision() == revision
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn native(value: &str) -> Vec<u16> {
        value.encode_utf16().collect()
    }

    #[test]
    fn unicode_combining_marks_supplementary_scalars_and_lines_are_exact() {
        let text = "ASCII é e\u{301} 漢🙂\r\n\t";
        assert_eq!(from_utf16(&native(text)).unwrap(), text);
    }

    #[test]
    fn malformed_surrogates_are_refused_without_lossy_replacement() {
        for units in [
            vec![0xD800],
            vec![0xDC00],
            vec![0xD800, 65],
            vec![65, 0xDC00],
        ] {
            assert_eq!(from_utf16(&units), Err(InvalidText::Encoding));
        }
        assert_eq!(from_utf16(&[0xD83D, 0xDE42]).unwrap(), "🙂");
    }

    #[test]
    fn empty_and_embedded_nul_values_are_refused_whole() {
        assert_eq!(from_utf16(&[]), Err(InvalidText::Empty));
        assert_eq!(from_utf16(&native("before\0after")), Err(InvalidText::Nul));
    }

    #[test]
    fn native_units_and_utf8_bytes_have_separate_exact_bounds() {
        let ascii = "a".repeat(MAX_COMMITTED_TEXT_BYTES);
        assert_eq!(from_utf16(&native(&ascii)).unwrap(), ascii);
        assert_eq!(
            from_utf16(&vec![65; MAX_UTF16_UNITS + 1]),
            Err(InvalidText::TooLong)
        );
        let unicode = "🙂".repeat(MAX_COMMITTED_TEXT_BYTES / 4);
        assert_eq!(from_utf16(&native(&unicode)).unwrap(), unicode);
        assert_eq!(
            from_utf16(&native(&format!("{unicode}a"))),
            Err(InvalidText::TooLong)
        );
        assert_eq!(
            from_utf16(&vec![0x6F22; MAX_UTF16_UNITS]),
            Err(InvalidText::TooLong)
        );
    }

    #[test]
    fn each_explicit_send_consumes_the_original_context_once_even_when_refused() {
        let original = std::sync::Arc::new(());
        let mut draft = Draft::new(original.clone());
        assert!(std::sync::Arc::ptr_eq(draft.context().unwrap(), &original));
        let sent = draft.take().unwrap();
        assert!(std::sync::Arc::ptr_eq(&sent, &original));
        // Whether admission succeeded or refused, no next poll retries it.
        drop(sent);
        assert!(draft.context().is_none());
        assert!(draft.take().is_none());
    }

    #[test]
    fn nested_cancellation_fences_send_even_while_native_editor_is_borrowed() {
        let original = std::sync::Arc::new(Lifetime::default());
        let nested = original.clone();
        let window = std::cell::RefCell::new(Draft::new(7));
        let mut editor = window.borrow_mut();
        let captured = editor.take();
        assert_eq!(captured, Some(7));
        assert!(window.try_borrow_mut().is_err());
        nested.dismiss();
        assert!(
            !original.valid(),
            "native read cannot admit the captured Send"
        );
        assert!(
            original.dismissed(),
            "cleanup must finish on the next UI turn"
        );
        assert!(editor.take().is_none());
    }

    #[test]
    fn overflow_and_lifecycle_loss_remain_invalid_after_a_new_editor_opens() {
        let old = Lifetime::default();
        old.invalidate();
        assert!(!old.valid());
        assert!(
            !old.dismissed(),
            "an overflow may retain only a refusal notice"
        );
        let fresh = Lifetime::default();
        assert!(fresh.valid());
        old.dismiss();
        old.invalidate();
        assert!(old.dismissed());
        assert!(!old.valid());
        assert!(
            fresh.valid(),
            "an old callback cannot replace the new lifetime"
        );
    }

    #[test]
    fn any_reentrant_edit_refuses_the_native_read_even_if_length_is_unchanged() {
        let lifetime = Lifetime::default();
        lifetime.changed();
        let revision = lifetime.revision();
        assert!(lifetime.unchanged(revision));
        let buffer = std::cell::RefCell::new("old".to_owned());
        let _read = buffer.borrow_mut();
        assert!(buffer.try_borrow_mut().is_err());
        // A same-length edit followed by an undo cannot restore the snapshot.
        lifetime.changed();
        lifetime.changed();
        assert!(!lifetime.unchanged(revision));
        assert!(lifetime.valid(), "ordinary editing before Send stays local");
        lifetime.invalidate();
        assert!(!lifetime.unchanged(lifetime.revision()));
    }

    #[test]
    fn exhausted_native_revision_never_wraps_into_an_admissible_old_read() {
        let lifetime = Lifetime::default();
        lifetime.revision.store(u64::MAX, Ordering::SeqCst);
        lifetime.changed();
        assert_eq!(lifetime.revision(), u64::MAX);
        assert!(!lifetime.valid());
        assert!(!lifetime.unchanged(u64::MAX));
        lifetime.changed();
        assert_eq!(lifetime.revision(), u64::MAX);
        assert!(!lifetime.unchanged(0));
    }

    #[test]
    fn cancellation_or_truncation_cannot_revive_an_old_draft() {
        let mut draft = Draft::new(7);
        draft.invalidate();
        draft.invalidate();
        assert!(draft.context().is_none());
        assert!(draft.take().is_none());
        let mut fresh = Draft::new(7);
        assert_eq!(fresh.take(), Some(7));
        assert!(draft.take().is_none());
    }
}
