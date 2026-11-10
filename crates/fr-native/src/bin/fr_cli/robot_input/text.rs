//! One bounded, explicit text selection for the original local input owner.
//! No Debug, source-path retention, normalization, clipboard fallback or retry.
use crate::Failure;
use fr_core::input::CommittedText;
#[cfg(any(target_os = "linux", target_os = "macos", test))]
use fr_core::input::MAX_COMMITTED_TEXT_BYTES;
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::io::Read;
use std::{fmt, sync::Arc};

/// Clones share the one immutable, cleared-on-drop core buffer. They do not
/// copy content into receipt history; the ledger retains only a fingerprint.
#[derive(Clone)]
pub(crate) struct Text(Arc<CommittedText>);

impl Text {
    pub fn new(value: &str) -> Result<Self, ()> {
        CommittedText::new(value)
            .map(|text| Self(Arc::new(text)))
            .map_err(|_| ())
    }
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

impl PartialEq for Text {
    fn eq(&self, other: &Self) -> bool {
        self.as_str() == other.as_str()
    }
}
impl Eq for Text {}
impl Serialize for Text {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}
impl<'de> Deserialize<'de> for Text {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl de::Visitor<'_> for Visitor {
            type Value = Text;
            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a nonempty bounded Unicode commit")
            }
            fn visit_str<E: de::Error>(self, value: &str) -> Result<Text, E> {
                Text::new(value).map_err(|()| E::custom("invalid committed-text length"))
            }
        }
        deserializer.deserialize_str(Visitor)
    }
}

fn refused() -> Failure {
    Failure::new(
        "invalid_text_file",
        "--text-file requires one readable regular file containing 1..4096 bytes of valid UTF-8. Symlinks, special files, empty input and oversized commits are refused; no text was sent.",
        2,
    )
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(super) fn read(path: &str) -> Result<Text, Failure> {
    use rustix::fs::{Mode, OFlags, open};
    // Opening a FIFO must not wait for its writer. NOFOLLOW pins the user's
    // explicit final component; metadata is checked on the opened descriptor.
    let fd = open(
        path,
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
        Mode::empty(),
    )
    .map_err(|_| refused())?;
    let mut file = std::fs::File::from(fd);
    let metadata = file.metadata().map_err(|_| refused())?;
    if !metadata.is_file()
        || metadata.len() == 0
        || metadata.len() > MAX_COMMITTED_TEXT_BYTES as u64
    {
        return Err(refused());
    }
    // The extra byte detects growth without truncation. Retained payload
    // capacity is bounded by CommittedText, including non-ASCII boundaries.
    let mut bytes = [0; MAX_COMMITTED_TEXT_BYTES + 1];
    let mut used = 0;
    let result = (|| {
        loop {
            if used == bytes.len() {
                return Err(refused());
            }
            let count = file.read(&mut bytes[used..]).map_err(|_| refused())?;
            if count == 0 {
                break;
            }
            used += count;
        }
        let value = std::str::from_utf8(&bytes[..used]).map_err(|_| refused())?;
        Text::new(value).map_err(|()| refused())
    })();
    bytes.fill(0);
    result
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub(super) fn read(_path: &str) -> Result<Text, Failure> {
    Err(refused())
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub(crate) fn directory() -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let directory = std::env::temp_dir().join(format!(
            "fr-committed-text-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&directory).unwrap();
        directory
    }

    #[test]
    fn exact_core_bound_and_unicode_survive_local_serialization_without_normalization() {
        for source in [
            "e\u{301}é漢🙂\r\n\t".to_owned(),
            "🙂".repeat(MAX_COMMITTED_TEXT_BYTES / 4),
        ] {
            let original = Text::new(&source).unwrap();
            let encoded = serde_json::to_vec(&original).unwrap();
            let decoded: Text = serde_json::from_slice(&encoded).unwrap();
            assert_eq!(decoded.as_str().as_bytes(), source.as_bytes());
        }
        for source in [String::new(), "x".repeat(MAX_COMMITTED_TEXT_BYTES + 1)] {
            assert!(Text::new(&source).is_err());
            assert!(
                serde_json::from_str::<Text>(&serde_json::to_string(&source).unwrap()).is_err()
            );
        }
    }

    #[test]
    fn sharing_a_request_keeps_one_original_core_buffer() {
        let text = Text::new("private fixture").unwrap();
        let clone = text.clone();
        assert!(Arc::ptr_eq(&text.0, &clone.0));
        drop(text);
        assert_eq!(clone.as_str(), "private fixture");
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn selected_file_keeps_exact_utf8_and_refuses_invalid_or_oversized_content() {
        let directory = directory();
        for (index, bytes) in [
            "e\u{301}é漢🙂\r\n\t\0".as_bytes().to_vec(),
            "🙂".repeat(MAX_COMMITTED_TEXT_BYTES / 4).into_bytes(),
            Vec::new(),
            vec![0xff],
            vec![b'x'; MAX_COMMITTED_TEXT_BYTES + 1],
        ]
        .into_iter()
        .enumerate()
        {
            let path = directory.join(format!("fixture-{index}"));
            std::fs::write(&path, &bytes).unwrap();
            let result = read(path.to_str().unwrap());
            if index < 2 {
                assert_eq!(result.unwrap().as_str().as_bytes(), bytes);
            } else {
                let error = result.err().expect("invalid files must not be truncated");
                assert_eq!(error.code, "invalid_text_file");
                assert!(!error.unknown_effect);
                assert!(!format!("{error:?}").contains("fixture-"));
            }
        }
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn selected_file_refuses_links_directories_missing_files_and_unconnected_fifos() {
        use rustix::fs::{CWD, Mode, mkfifoat};
        let directory = directory();
        let regular = directory.join("regular");
        std::fs::write(&regular, "private fixture").unwrap();
        let link = directory.join("link");
        std::os::unix::fs::symlink(&regular, &link).unwrap();
        let missing = directory.join("missing");
        let dangling = directory.join("dangling");
        std::os::unix::fs::symlink(&missing, &dangling).unwrap();
        let fifo = directory.join("fifo");
        mkfifoat(CWD, &fifo, Mode::RUSR | Mode::WUSR).unwrap();
        for path in [&link, &dangling, &directory, &missing, &fifo] {
            let error = read(path.to_str().unwrap())
                .err()
                .expect("not a regular file");
            assert_eq!(error.code, "invalid_text_file");
            assert!(!error.unknown_effect);
            assert!(!format!("{error:?}").contains(path.to_str().unwrap()));
        }
    }
}
