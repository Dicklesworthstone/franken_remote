//! Owned committed text for bounded cross-thread input handoff. It is neither
//! IME preedit state nor a clipboard operation, and it never splits a commit.

/// Maximum complete UTF-8 bytes in one committed-text action.
pub const MAX_COMMITTED_TEXT_BYTES: usize = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextError {
    Empty,
    TooLong,
    Allocation,
}

/// One unchanged, validated UTF-8 commit with bounded owned storage. Neither
/// Debug nor Clone is implemented: input is not diagnostic data or replay work.
/// Drop clears the initialized bytes as best-effort cleanup, not a guarantee
/// against compiler optimization, allocator copies or platform retention.
pub struct CommittedText {
    bytes: Vec<u8>,
}
impl CommittedText {
    pub fn new(text: &str) -> Result<Self, TextError> {
        if text.is_empty() {
            return Err(TextError::Empty);
        }
        if text.len() > MAX_COMMITTED_TEXT_BYTES {
            return Err(TextError::TooLong);
        }
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(text.len())
            .map_err(|_| TextError::Allocation)?;
        // Charge retained capacity, not just initialized payload length.
        if bytes.capacity() > MAX_COMMITTED_TEXT_BYTES {
            return Err(TextError::Allocation);
        }
        bytes.extend_from_slice(text.as_bytes());
        Ok(Self { bytes })
    }
    pub fn as_str(&self) -> &str {
        // The only constructor copies a str and no mutable buffer is exposed.
        std::str::from_utf8(&self.bytes).expect("committed text remains UTF-8")
    }
    fn clear(&mut self) {
        self.bytes.fill(0);
    }
}
impl Drop for CommittedText {
    fn drop(&mut self) {
        self.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_unicode_and_combining_sequences_without_normalization() {
        let value = "e\u{301}é漢🙂\r\n\t";
        let text = CommittedText::new(value).unwrap();
        assert_eq!(text.as_str().as_bytes(), value.as_bytes());
        assert!(text.bytes.capacity() <= MAX_COMMITTED_TEXT_BYTES);
    }
    #[test]
    fn owns_the_original_commit_not_the_platform_buffer() {
        let mut platform = String::from("original 漢🙂");
        let text = CommittedText::new(&platform).unwrap();
        platform.clear();
        platform.push_str("next composition");
        assert_eq!(text.as_str(), "original 漢🙂");
    }
    #[test]
    fn refuses_empty_composition_instead_of_minting_an_action() {
        assert!(matches!(CommittedText::new(""), Err(TextError::Empty)));
    }
    #[test]
    fn accepts_the_exact_ascii_byte_limit() {
        let value = "a".repeat(MAX_COMMITTED_TEXT_BYTES);
        let text = CommittedText::new(&value).unwrap();
        assert_eq!(text.as_str(), value);
        assert!(text.bytes.capacity() <= MAX_COMMITTED_TEXT_BYTES);
    }
    #[test]
    fn refuses_an_oversized_commit_without_truncating() {
        let value = "a".repeat(MAX_COMMITTED_TEXT_BYTES + 1);
        assert!(matches!(
            CommittedText::new(&value),
            Err(TextError::TooLong)
        ));
    }
    #[test]
    fn unicode_limit_is_bytes_not_characters() {
        let mut value = "🙂".repeat(MAX_COMMITTED_TEXT_BYTES / 4);
        let text = CommittedText::new(&value).unwrap();
        assert_eq!(text.as_str(), value);
        assert_eq!(text.as_str().len(), MAX_COMMITTED_TEXT_BYTES);
        value.push('a');
        assert!(matches!(
            CommittedText::new(&value),
            Err(TextError::TooLong)
        ));
    }
    #[test]
    fn small_commit_does_not_retain_an_oversized_callers_capacity() {
        let mut value = String::with_capacity(MAX_COMMITTED_TEXT_BYTES * 8);
        value.push('é');
        let text = CommittedText::new(&value).unwrap();
        assert_eq!(text.as_str(), "é");
        assert!(text.bytes.capacity() <= MAX_COMMITTED_TEXT_BYTES);
    }
    #[test]
    fn cleanup_clears_every_initialized_byte() {
        let mut text = CommittedText::new("private 漢🙂").unwrap();
        let len = text.bytes.len();
        text.clear();
        assert_eq!(text.bytes.len(), len);
        assert!(text.bytes.iter().all(|byte| *byte == 0));
    }
}
