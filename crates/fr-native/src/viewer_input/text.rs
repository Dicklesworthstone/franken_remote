//! One original-view native text producer. Contexts and Send gestures are
//! one-use; a local editor never creates a ticket or renews an input deadline.
use super::{ClientInstant, Native, StopReason, Target, held::Snapshot};
use fr_core::input::MAX_COMMITTED_TEXT_BYTES;
use frd::session_startup::viewer_events::MAX_EVENT_AGE_US;
use std::ffi::{CStr, c_char, c_int, c_void};

pub(super) const SEND: u32 = 13;
pub(super) const RETIRED: u32 = 16;

#[derive(Default, Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Offer {
    serial: u64,
    ready: bool,
}

/// Native code only receives the non-reused serial. The exact original Source
/// and uninterrupted view remain in Rust until the Send is consumed.
pub(super) struct Context<C> {
    last_serial: u64,
    current: Option<(u64, C)>,
}
impl<C> Default for Context<C> {
    fn default() -> Self {
        Self {
            last_serial: 0,
            current: None,
        }
    }
}
impl<C: PartialEq> Context<C> {
    pub(super) fn offer(&mut self, context: Option<C>, ready: bool) -> Result<Offer, StopReason> {
        let Some(context) = context else {
            self.current = None;
            return Ok(Offer::default());
        };
        if !self
            .current
            .as_ref()
            .is_some_and(|(_, old)| *old == context)
        {
            self.last_serial = self
                .last_serial
                .checked_add(1)
                .ok_or(StopReason::NativeFailure)?;
            self.current = Some((self.last_serial, context));
        }
        Ok(Offer {
            serial: self.current.as_ref().expect("original context").0,
            ready,
        })
    }

    /// A stale callback cannot retire a newer editor. A matching Send consumes
    /// its context even when its bytes, timestamp or readiness later refuse.
    pub(super) fn take(&mut self, serial: u64) -> Option<C> {
        if serial == 0 || self.current.as_ref()?.0 != serial {
            return None;
        }
        self.current.take().map(|(_, context)| context)
    }
}

/// One bounded native value. No Clone/Debug: text is neither replayable nor
/// diagnostic data. The source makes its own bounded copy on admission.
pub(super) struct Commit {
    pub(super) serial: u64,
    length: usize,
    bytes: [u8; MAX_COMMITTED_TEXT_BYTES],
}
impl Commit {
    pub(super) fn validate(
        &self,
        sampled: Option<ClientInstant>,
        now: ClientInstant,
        held: bool,
    ) -> Option<(&str, ClientInstant, ClientInstant)> {
        let sampled = sampled?;
        let until = ClientInstant(sampled.0.checked_add(MAX_EVENT_AGE_US)?);
        if held || self.serial == 0 || self.length == 0 || now < sampled || now >= until {
            return None;
        }
        let text = std::str::from_utf8(self.bytes.get(..self.length)?).ok()?;
        Some((text, sampled, until))
    }
}
impl Drop for Commit {
    fn drop(&mut self) {
        self.bytes.fill(0);
    }
}

unsafe extern "C" {
    fn fr_viewer_input_text_prepare(handle: *mut c_void, display: *const c_char) -> c_int;
    fn fr_viewer_input_text_unavailable(handle: *mut c_void);
    fn fr_viewer_input_text_offer(handle: *mut c_void, serial: u64, ready: c_int) -> c_int;
    fn fr_viewer_input_text_take(
        handle: *mut c_void,
        serial: *mut u64,
        bytes: *mut u8,
        capacity: u32,
        length: *mut u32,
    ) -> c_int;
    fn fr_viewer_input_text_result(handle: *mut c_void, serial: u64, queued: c_int);
    fn fr_viewer_input_text_held(handle: *mut c_void) -> c_int;
    fn fr_viewer_input_text_reconcile(handle: *mut c_void, actual: *const Snapshot);
}

pub(super) fn prepare(native: &Native, display: &CStr) -> bool {
    if crate::xlib::initialize_input_method().is_err() {
        // SAFETY: the original live XCB owner publishes one fixed refusal;
        // this helper makes no Xlib call after its initialization failed.
        unsafe { fr_viewer_input_text_unavailable(native.0.as_ptr()) }
        return false;
    }
    // SAFETY: live unique native owner, borrowed NUL-terminated local display
    // name. Native preparation retains no Rust pointer. Unavailable IME leaves
    // physical input usable and never enables an alternative text mechanism.
    unsafe { fr_viewer_input_text_prepare(native.0.as_ptr(), display.as_ptr()) == 1 }
}

/// Publish before native callbacks, including callbacks during a server
/// barrier. A changed/zero serial clears any old native draft and pending Send.
pub(super) fn offer(
    native: &Native,
    target: &mut impl Target,
    held: bool,
) -> Result<(), StopReason> {
    let offer = target.text_offer(held)?;
    // SAFETY: the original native owner is confined to this thread. Only a
    // content-free serial and readiness bit cross; C retains no Rust pointer.
    if unsafe {
        fr_viewer_input_text_offer(native.0.as_ptr(), offer.serial, c_int::from(offer.ready))
    } == 1
    {
        Ok(())
    } else {
        Err(StopReason::NativeFailure)
    }
}

pub(super) fn take(native: &Native) -> Result<Option<Commit>, StopReason> {
    let mut commit = Commit {
        serial: 0,
        length: 0,
        bytes: [0; MAX_COMMITTED_TEXT_BYTES],
    };
    let mut length = 0;
    // SAFETY: the live original C owner writes at most this fixed output
    // capacity and retains none of the pointers. The pending value is consumed
    // once by C, and the returned length is independently checked below.
    let result = unsafe {
        fr_viewer_input_text_take(
            native.0.as_ptr(),
            &raw mut commit.serial,
            commit.bytes.as_mut_ptr(),
            u32::try_from(commit.bytes.len()).expect("fixed text bound"),
            &raw mut length,
        )
    };
    match result {
        0 => Ok(None),
        1 if usize::try_from(length).is_ok_and(|length| length <= commit.bytes.len()) => {
            commit.length = usize::try_from(length).expect("bounded native length");
            Ok(Some(commit))
        }
        _ => Err(StopReason::NativeFailure),
    }
}

pub(super) fn result(native: &Native, serial: u64, queued: bool) {
    // SAFETY: same live owner, content-free result of one original Send. C
    // must ignore a stale serial; queued never claims a host/OS effect.
    unsafe {
        fr_viewer_input_text_result(native.0.as_ptr(), serial, c_int::from(queued));
    }
}

/// This content-free native ledger schedules the existing bounded sampler. It
/// is not Send readiness: an editor's own Ctrl+Return keys must remain local.
pub(super) fn has_held(native: &Native) -> bool {
    // SAFETY: same live thread-confined native owner. This inspects fixed local
    // bitsets only and performs no server query or input operation.
    unsafe { fr_viewer_input_text_held(native.0.as_ptr()) != 0 }
}

/// Only a stable snapshot from the original native owner may clear a missing
/// release. C intersects existing local/forwarded ledgers, never adopts presses.
pub(super) fn reconcile(native: &Native, actual: &Snapshot) {
    // SAFETY: repr(C) fixed 64-byte key/button snapshot, borrowed synchronously
    // by the original native owner. No pointer or snapshot is retained by C.
    unsafe { fr_viewer_input_text_reconcile(native.0.as_ptr(), actual) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn native_commit(serial: u64, value: &[u8]) -> Commit {
        let mut bytes = [0; MAX_COMMITTED_TEXT_BYTES];
        let copied = value.len().min(bytes.len());
        bytes[..copied].copy_from_slice(&value[..copied]);
        Commit {
            serial,
            length: value.len(),
            bytes,
        }
    }

    #[test]
    fn compositor_gap_preserves_draft_but_suspension_invalidates_it() {
        let mut owner = Context::default();
        let original = owner.offer(Some(7), true).unwrap();
        assert!(original.ready);
        let gap = owner.offer(Some(7), false).unwrap();
        assert_eq!(gap.serial, original.serial);
        assert!(!gap.ready);
        assert_eq!(owner.offer(None, false).unwrap(), Offer::default());
        let resumed = owner.offer(Some(7), true).unwrap();
        assert!(resumed.serial > original.serial);
        assert!(owner.take(original.serial).is_none());
        assert_eq!(owner.take(resumed.serial), Some(7));
    }

    #[test]
    fn changed_view_and_consumed_send_never_reuse_an_editor_serial() {
        let mut owner = Context::default();
        let original = owner.offer(Some(7), true).unwrap();
        let changed = owner.offer(Some(8), true).unwrap();
        assert!(changed.serial > original.serial);
        assert!(owner.take(original.serial).is_none());
        assert_eq!(owner.take(changed.serial), Some(8));
        assert!(owner.take(changed.serial).is_none());
        let next = owner.offer(Some(8), true).unwrap();
        assert!(next.serial > changed.serial);
        assert!(owner.take(changed.serial).is_none());
        assert_eq!(owner.take(next.serial), Some(8));
    }

    #[test]
    fn cancelled_editor_cannot_reappear_in_a_reopened_same_view_draft() {
        let mut owner = Context::default();
        let cancelled = owner.offer(Some(7), true).unwrap();
        assert_eq!(owner.take(cancelled.serial), Some(7));
        let reopened = owner.offer(Some(7), true).unwrap();
        assert!(reopened.serial > cancelled.serial);
        assert!(owner.take(cancelled.serial).is_none());
        assert_eq!(owner.take(reopened.serial), Some(7));
    }

    #[test]
    fn editor_serial_exhaustion_refuses_instead_of_wrapping() {
        let mut owner = Context {
            last_serial: u64::MAX,
            current: None,
        };
        assert_eq!(owner.offer(Some(7), true), Err(StopReason::NativeFailure));
        assert!(owner.take(0).is_none());
    }

    #[test]
    fn preserves_complete_unicode_without_normalization_or_splitting() {
        let value = "e\u{301}é漢🙂\r\n\t\0";
        let commit = native_commit(1, value.as_bytes());
        let (text, sampled, until) = commit
            .validate(Some(ClientInstant(1_000)), ClientInstant(1_001), false)
            .unwrap();
        assert_eq!(text.as_bytes(), value.as_bytes());
        assert_eq!(sampled, ClientInstant(1_000));
        assert_eq!(until, ClientInstant(1_000 + MAX_EVENT_AGE_US));
        let boundary = "🙂".repeat(MAX_COMMITTED_TEXT_BYTES / 4);
        let exact = native_commit(1, boundary.as_bytes());
        assert_eq!(
            exact
                .validate(Some(ClientInstant(1)), ClientInstant(2), false)
                .unwrap()
                .0,
            boundary
        );
    }

    #[test]
    fn invalid_empty_and_oversized_values_refuse_whole() {
        for value in [
            Vec::new(),
            vec![0xff],
            vec![b'x'; MAX_COMMITTED_TEXT_BYTES + 1],
        ] {
            assert!(
                native_commit(1, &value)
                    .validate(Some(ClientInstant(1)), ClientInstant(2), false)
                    .is_none()
            );
        }
        let partial = "🙂".as_bytes()[..3].to_vec();
        assert!(
            native_commit(1, &partial)
                .validate(Some(ClientInstant(1)), ClientInstant(2), false)
                .is_none()
        );
    }

    #[test]
    fn send_retains_its_original_exclusive_deadline() {
        let commit = native_commit(1, b"one send");
        let sampled = Some(ClientInstant(1_000));
        assert!(
            commit
                .validate(sampled, ClientInstant(100_999), false)
                .is_some()
        );
        for now in [101_000, 101_001, 60_000_000] {
            assert!(
                commit
                    .validate(sampled, ClientInstant(now), false)
                    .is_none()
            );
        }
        assert!(commit.validate(None, ClientInstant(1_001), false).is_none());
        assert!(
            commit
                .validate(sampled, ClientInstant(999), false)
                .is_none()
        );
        assert!(
            commit
                .validate(sampled, ClientInstant(1_001), true)
                .is_none()
        );
        assert!(
            commit
                .validate(
                    Some(ClientInstant(u64::MAX)),
                    ClientInstant(u64::MAX),
                    false
                )
                .is_none()
        );
        assert!(
            native_commit(0, b"no context")
                .validate(sampled, ClientInstant(1_001), false)
                .is_none()
        );
    }
}
