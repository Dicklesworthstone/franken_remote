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
    #[cfg(all(target_os = "linux", feature = "linux-desktop"))]
    Choose(Instant),
    #[cfg(all(target_os = "linux", feature = "linux-desktop"))]
    Choosing,
}
#[cfg(all(target_os = "linux", feature = "linux-desktop"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ChoiceStatus {
    Idle,
    Choosing,
    Returning,
    Cancelled,
    Unavailable,
    CleanupIncomplete,
}
#[cfg(all(target_os = "linux", feature = "linux-desktop"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ChoiceSnapshot {
    pub(crate) ready: bool,
    pub(crate) status: ChoiceStatus,
}
#[derive(Default)]
struct State {
    ready: bool,
    closed: bool,
    slot: Option<Slot>,
    #[cfg(all(target_os = "linux", feature = "linux-desktop"))]
    choice: Option<ChoiceStatus>,
}
impl State {
    fn retire(&mut self, reason: &'static str) {
        if let Some(Slot::Pending(dropped)) = &self.slot {
            let count = dropped.paths.len();
            // Replacing the slot immediately drops every path. A later close
            // must preserve the first recorded retirement reason.
            self.slot = Some(Slot::Retired(Retired { count, reason }));
        }
        #[cfg(all(target_os = "linux", feature = "linux-desktop"))]
        if matches!(self.slot, Some(Slot::Choose(_))) {
            // A request to open a chooser has no selected objects to retire.
            self.slot = None;
            self.choice = Some(ChoiceStatus::Unavailable);
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
            pending => {
                state.slot = Some(pending);
                None
            }
        }
    }
}

#[cfg(all(target_os = "linux", feature = "linux-desktop"))]
impl Inbox {
    /// Install only on the original ordinary upload owner. A standing folder
    /// job has no such intake and never enables this action.
    pub(crate) fn enable_choice(&self) -> bool {
        let mut state = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.closed || state.choice.is_some() {
            return false;
        }
        state.choice = Some(ChoiceStatus::Idle);
        true
    }
    pub(crate) fn choice(&self) -> Option<ChoiceSnapshot> {
        let state = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.choice.map(|status| ChoiceSnapshot {
            ready: state.ready && !state.closed && state.slot.is_none(),
            status,
        })
    }
    /// Reserve the same one slot as Xdnd, before any native dialog opens. A
    /// second click or drop cannot replace this original local interaction.
    pub(crate) fn request_choice(&self) -> bool {
        let mut state = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !state.ready || state.closed || state.slot.is_some() || state.choice.is_none() {
            return false;
        }
        state.slot = Some(Slot::Choose(Instant::now()));
        state.choice = Some(ChoiceStatus::Choosing);
        true
    }
    pub(crate) fn take_choice(&self) -> bool {
        let mut state = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(Slot::Choose(captured)) = state.slot else {
            return false;
        };
        if !state.ready || state.closed || captured.elapsed() >= MAX_AGE {
            state.retire("file_selection_expired");
            return false;
        }
        state.ready = false;
        state.slot = Some(Slot::Choosing);
        true
    }
    pub(crate) fn returning_choice(&self) {
        let mut state = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if matches!(state.slot, Some(Slot::Choosing)) {
            state.choice = Some(ChoiceStatus::Returning);
        }
    }
    /// Called only after the original picker returned from close. Unconfirmed
    /// native cleanup permanently fences this owner's chooser AND drop slot.
    pub(crate) fn finish_choice(&self, outcome: ChoiceStatus) {
        let mut state = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !matches!(state.slot, Some(Slot::Choosing)) {
            return;
        }
        state.choice = Some(outcome);
        state.slot = None;
        state.ready = false;
        if outcome == ChoiceStatus::CleanupIncomplete {
            state.closed = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(all(target_os = "linux", feature = "linux-desktop"))]
    #[test]
    fn chooser_and_drop_share_one_original_admission_slot() {
        let inbox = Inbox::default();
        let files = || vec![PathBuf::from("/tmp/explicit")];
        inbox.set_ready(true);
        assert!(!inbox.request_choice(), "a drop-only owner has no chooser");
        assert!(inbox.enable_choice());
        assert!(!inbox.enable_choice());
        assert!(inbox.request_choice());
        assert!(!inbox.ready());
        assert!(!inbox.publish(files(), Instant::now()));
        assert!(!inbox.request_choice());
        assert!(inbox.take().is_none());
        assert!(inbox.take_retired().is_none());
        assert!(inbox.take_choice());
        assert!(!inbox.take_choice());
        // Readiness cannot release a live dialog's reservation.
        inbox.set_ready(true);
        assert!(!inbox.ready());
        assert!(!inbox.request_choice());
        assert!(!inbox.publish(files(), Instant::now()));
        inbox.returning_choice();
        assert_eq!(inbox.choice().unwrap().status, ChoiceStatus::Returning);
        inbox.set_ready(true);
        assert!(!inbox.ready(), "input resumption still owns the same slot");
        inbox.finish_choice(ChoiceStatus::Cancelled);
        assert!(inbox.take().is_none());
        assert!(inbox.take_retired().is_none());
        assert!(!inbox.is_closed(), "declining has no authority effect");
        inbox.set_ready(true);
        assert!(inbox.choice().unwrap().ready);
        assert_eq!(inbox.choice().unwrap().status, ChoiceStatus::Cancelled);
        assert!(inbox.publish(files(), Instant::now()));
        assert!(!inbox.request_choice());
        assert_eq!(inbox.take().unwrap().paths, files());
    }

    #[cfg(all(target_os = "linux", feature = "linux-desktop"))]
    #[test]
    fn chooser_cleanup_failure_cannot_be_rearmed_or_invent_a_batch() {
        let inbox = Inbox::default();
        assert!(inbox.enable_choice());
        inbox.set_ready(true);
        assert!(inbox.request_choice());
        assert!(inbox.take_choice());
        inbox.finish_choice(ChoiceStatus::CleanupIncomplete);
        inbox.set_ready(true);
        inbox.finish_choice(ChoiceStatus::Idle);
        assert!(inbox.is_closed());
        assert!(!inbox.ready());
        assert!(!inbox.enable_choice());
        assert!(!inbox.request_choice());
        assert_eq!(
            inbox.choice().unwrap().status,
            ChoiceStatus::CleanupIncomplete
        );
        assert!(inbox.take().is_none());
        assert!(inbox.take_retired().is_none());
    }

    #[cfg(all(target_os = "linux", feature = "linux-desktop"))]
    #[test]
    fn stale_or_closed_chooser_click_does_not_open_or_select_anything() {
        for close in [false, true] {
            let inbox = Inbox::default();
            assert!(inbox.enable_choice());
            inbox.set_ready(true);
            assert!(inbox.request_choice());
            if close {
                inbox.close();
            } else {
                inbox.0.lock().unwrap().slot =
                    Some(Slot::Choose(Instant::now().checked_sub(MAX_AGE).unwrap()));
            }
            assert!(!inbox.take_choice());
            assert!(inbox.take().is_none());
            assert!(inbox.take_retired().is_none());
            assert_eq!(inbox.choice().unwrap().status, ChoiceStatus::Unavailable);
            inbox.set_ready(true);
            assert_eq!(inbox.request_choice(), !close);
        }
    }

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
            .collect::<Vec<_>>()
            .concat();
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
        assert!(!inbox.publish(files(), Instant::now().checked_sub(MAX_AGE).unwrap()));
        assert!(!inbox.is_closed());
        inbox.close();
        assert!(inbox.is_closed());
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
            dropped.captured = Instant::now().checked_sub(MAX_AGE).unwrap();
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
        assert!(!inbox.publish(files(), Instant::now().checked_sub(MAX_AGE).unwrap()));
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
