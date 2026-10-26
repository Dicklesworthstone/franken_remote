//! Bounded local file-drop intent. No file is opened on the window thread.
//! The original session's upload intake separately checks authority and budgets.
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

pub(crate) const MAX_BYTES: usize = 32 * 1024;
pub(crate) const MAX_PATHS: usize = 8;
pub(crate) const MAX_AGE: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Refusal {
    Invalid,
    TooLarge,
    TooMany,
    Remote,
}

fn hex(byte: u8) -> Result<u8, Refusal> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        b'A'..=b'F' => Ok(byte - b'A' + 10),
        _ => Err(Refusal::Invalid),
    }
}

/// No DNS or remote filesystem lookup. Only explicit local file URIs are
/// accepted; relative paths, queries, fragments, NUL and ambiguous escapes refuse.
pub(crate) fn paths(bytes: &[u8]) -> Result<Vec<PathBuf>, Refusal> {
    if bytes.is_empty() || bytes.len() > MAX_BYTES {
        return Err(Refusal::TooLarge);
    }
    let text = std::str::from_utf8(bytes).map_err(|_| Refusal::Invalid)?;
    let mut paths = Vec::new();
    for raw in text.split('\n') {
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if paths.len() == MAX_PATHS {
            return Err(Refusal::TooMany);
        }
        let uri = line.strip_prefix("file://").ok_or(Refusal::Invalid)?;
        let slash = uri.find('/').ok_or(Refusal::Invalid)?;
        let (host, encoded) = uri.split_at(slash);
        if !host.is_empty() && !host.eq_ignore_ascii_case("localhost") {
            return Err(Refusal::Remote);
        }
        if encoded.contains(['?', '#']) {
            return Err(Refusal::Invalid);
        }
        let mut decoded = Vec::with_capacity(encoded.len());
        let mut source = encoded.bytes();
        while let Some(byte) = source.next() {
            let byte = if byte == b'%' {
                let high = hex(source.next().ok_or(Refusal::Invalid)?)?;
                let low = hex(source.next().ok_or(Refusal::Invalid)?)?;
                let value = high * 16 + low;
                if value == b'/' {
                    return Err(Refusal::Invalid);
                }
                value
            } else {
                byte
            };
            if byte < 32 || byte == 127 {
                return Err(Refusal::Invalid);
            }
            decoded.push(byte);
        }
        let decoded = String::from_utf8(decoded).map_err(|_| Refusal::Invalid)?;
        if decoded.len() > 4096 || decoded.starts_with("//") {
            return Err(Refusal::Invalid);
        }
        let path = PathBuf::from(decoded);
        if !path.is_absolute() || path.file_name().is_none() || paths.contains(&path) {
            return Err(Refusal::Invalid);
        }
        paths.push(path);
    }
    if paths.is_empty() {
        return Err(Refusal::Invalid);
    }
    Ok(paths)
}

pub(crate) struct Dropped {
    pub(crate) paths: Vec<PathBuf>,
    pub(crate) captured: Instant,
}
impl std::fmt::Debug for Dropped {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalFileDrop")
            .field("selections", &self.paths.len())
            .finish_non_exhaustive()
    }
}
/// A previously accepted intent that cannot run. The count is sufficient for
/// indexed completion; retiring a gesture never keeps or opens its paths.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Retired {
    pub(crate) count: usize,
    pub(crate) reason: &'static str,
}
#[derive(Debug)]
enum Slot {
    Pending(Dropped),
    Retired(Retired),
}
#[derive(Default)]
struct State {
    ready: bool,
    closed: bool,
    slot: Option<Slot>,
}
impl State {
    fn retire(&mut self, reason: &'static str) {
        if let Some(Slot::Pending(dropped)) = &self.slot {
            let count = dropped.paths.len();
            // Replacing the slot immediately drops every path. A later close
            // must preserve the first recorded retirement reason.
            self.slot = Some(Slot::Retired(Retired { count, reason }));
        }
    }
}

/// One selection of at most eight paths OR one content-free retirement count.
/// A close drops paths immediately, wins over later readiness updates and never
/// transfers intent to a replacement window or a new control lease.
#[derive(Clone, Default)]
pub(crate) struct Inbox(Arc<Mutex<State>>);
impl Inbox {
    pub(crate) fn is_closed(&self) -> bool {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .closed
    }
    pub(crate) fn ready(&self) -> bool {
        let state = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.ready && !state.closed && state.slot.is_none()
    }
    pub(crate) fn set_ready(&self, ready: bool) {
        let mut state = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.ready = ready && !state.closed && !matches!(state.slot, Some(Slot::Retired(_)));
        if !state.ready {
            state.retire("file_selection_stopped");
        }
    }
    pub(crate) fn close(&self) {
        let mut state = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.closed = true;
        state.ready = false;
        state.retire("file_selection_stopped");
    }
    pub(crate) fn publish(&self, paths: Vec<PathBuf>, captured: Instant) -> bool {
        let mut state = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !state.ready
            || state.closed
            || state.slot.is_some()
            || paths.is_empty()
            || paths.len() > MAX_PATHS
            || captured > Instant::now()
            || captured.elapsed() >= MAX_AGE
        {
            return false;
        }
        state.slot = Some(Slot::Pending(Dropped { paths, captured }));
        true
    }
    pub(crate) fn take(&self) -> Option<Dropped> {
        let mut state = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !state.ready || state.closed {
            state.retire("file_selection_stopped");
            return None;
        }
        if matches!(&state.slot, Some(Slot::Pending(pending)) if pending.captured.elapsed() >= MAX_AGE)
        {
            state.ready = false;
            state.retire("file_selection_expired");
            return None;
        }
        let Some(Slot::Pending(_)) = &state.slot else {
            return None;
        };
        state.ready = false;
        match state.slot.take() {
            Some(Slot::Pending(pending)) => Some(pending),
            _ => unreachable!("the same mutex guards the original pending selection"),
        }
    }
    pub(crate) fn take_retired(&self) -> Option<Retired> {
        let mut state = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match state.slot.take()? {
            Slot::Retired(retired) => Some(retired),
            pending @ Slot::Pending(_) => {
                state.slot = Some(pending);
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_uri_list_preserves_explicit_names_and_order() {
        assert_eq!(
            paths(
                b"# selected locally\r\nfile:///tmp/a%20b.txt\r\nfile://localhost/tmp/%CE%BB.txt\n"
            )
            .unwrap(),
            [PathBuf::from("/tmp/a b.txt"), PathBuf::from("/tmp/λ.txt")]
        );
        assert_eq!(
            paths(b"file:///tmp/a+b").unwrap(),
            [PathBuf::from("/tmp/a+b")]
        );
    }

    #[test]
    fn refuses_remote_ambiguous_invalid_and_duplicate_selections() {
        for bytes in [
            b"https://example.com/private".as_slice(),
            b"file://remote/tmp/a",
            b"file://user@localhost/tmp/a",
            b"file:///tmp/a?query",
            b"file:///tmp/a#fragment",
            b"file:///tmp/a%00b",
            b"file:///tmp/%2Fetc",
            b"file:///tmp/%zz",
            b"file:///tmp/%F",
            b"file:relative",
            b"file:////server/a",
            b"file:///tmp/%ff",
            b"file:///tmp/a\rhidden",
            b"file:///",
            b"# comments only\r\n",
            b"file:///tmp/a\nfile:///tmp/a",
        ] {
            assert!(paths(bytes).is_err(), "invalid URI list was accepted");
        }
    }

    #[test]
    fn all_allocation_is_bounded_by_bytes_and_selection_count() {
        assert_eq!(paths(&vec![b'a'; MAX_BYTES + 1]), Err(Refusal::TooLarge));
        let eight = (0..MAX_PATHS)
            .map(|n| format!("file:///tmp/{n}\n"))
            .collect::<String>();
        assert_eq!(paths(eight.as_bytes()).unwrap().len(), MAX_PATHS);
        assert_eq!(
            paths(format!("{eight}file:///tmp/extra").as_bytes()),
            Err(Refusal::TooMany)
        );
    }

    #[test]
    fn original_drop_is_single_use_and_a_close_cannot_be_rearmed() {
        let inbox = Inbox::default();
        let files = || vec![PathBuf::from("/tmp/explicit")];
        assert!(!inbox.publish(files(), Instant::now()));
        inbox.set_ready(true);
        assert!(inbox.publish(files(), Instant::now()));
        assert!(!inbox.publish(files(), Instant::now()));
        assert!(inbox.take().is_some());
        assert!(inbox.take().is_none());
        assert!(!inbox.ready());
        inbox.set_ready(true);
        assert!(!inbox.publish(files(), Instant::now() - MAX_AGE));
        inbox.close();
        inbox.set_ready(true);
        assert!(!inbox.publish(files(), Instant::now()));
        assert!(!inbox.ready());
    }

    #[test]
    fn accepted_drop_retirement_preserves_only_count_until_collected_once() {
        for closed in [false, true] {
            let inbox = Inbox::default();
            let files = || {
                vec![
                    PathBuf::from("/tmp/private-first-name"),
                    PathBuf::from("/tmp/private-second-name"),
                ]
            };
            inbox.set_ready(true);
            assert!(inbox.publish(files(), Instant::now()));
            if closed {
                inbox.close();
            } else {
                inbox.set_ready(false);
            }
            {
                let state = inbox.0.lock().unwrap();
                assert!(matches!(state.slot, Some(Slot::Retired(_))));
                let debug = format!("{:?}", state.slot);
                assert!(!debug.contains("private") && !debug.contains("/tmp"));
            }
            assert!(inbox.take().is_none());
            inbox.set_ready(true);
            assert!(
                !inbox.ready(),
                "retirement must be collected before rearming"
            );
            assert!(!inbox.publish(files(), Instant::now()));
            assert_eq!(
                inbox.take_retired(),
                Some(Retired {
                    count: 2,
                    reason: "file_selection_stopped",
                })
            );
            assert!(inbox.take_retired().is_none());
            assert!(inbox.take().is_none());
            inbox.set_ready(true);
            assert_eq!(inbox.ready(), !closed);
        }
    }

    #[test]
    fn expiry_of_an_accepted_drop_keeps_its_count_and_first_reason() {
        let inbox = Inbox::default();
        inbox.set_ready(true);
        assert!(inbox.publish(
            vec![PathBuf::from("/tmp/private-expiring-name")],
            Instant::now(),
        ));
        // Advance only the captured fixture timestamp; exercise the production
        // expiry branch without waiting on a scheduler or filesystem.
        {
            let mut state = inbox.0.lock().unwrap();
            let Some(Slot::Pending(dropped)) = &mut state.slot else {
                panic!("the accepted gesture must occupy the original slot");
            };
            assert!(!format!("{dropped:?}").contains("private-expiring-name"));
            dropped.captured = Instant::now() - MAX_AGE;
        }
        assert!(inbox.take().is_none());
        inbox.set_ready(true);
        assert!(!inbox.ready());
        inbox.close();
        assert_eq!(
            inbox.take_retired(),
            Some(Retired {
                count: 1,
                reason: "file_selection_expired",
            })
        );
        assert!(inbox.take_retired().is_none());
        assert!(!inbox.ready());
    }

    #[test]
    fn unaccepted_or_already_collected_drops_do_not_create_duplicate_results() {
        let inbox = Inbox::default();
        let files = || vec![PathBuf::from("/tmp/explicit")];
        inbox.set_ready(true);
        assert!(!inbox.publish(files(), Instant::now() - MAX_AGE));
        assert!(inbox.take_retired().is_none());
        assert!(!inbox.publish(Vec::new(), Instant::now()));
        assert!(!inbox.publish(
            vec![PathBuf::from("/tmp/explicit"); MAX_PATHS + 1],
            Instant::now()
        ));
        assert!(inbox.publish(files(), Instant::now()));
        assert!(
            inbox.take_retired().is_none(),
            "inspection cannot consume pending work"
        );
        assert!(inbox.take().is_some());
        inbox.close();
        assert!(inbox.take_retired().is_none());
    }
}
