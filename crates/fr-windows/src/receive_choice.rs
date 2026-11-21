//! One local receive-folder choice for the next connection from this picker.
//! It is deliberately separate from remembered settings and is consumed even
//! when the user chooses view-only or the child process cannot start.
#![forbid(unsafe_code)]

use crate::file_choice::{CHOICE_MS, Error, path_units};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Request {
    generation: u64,
    started: u64,
}

/// A selected pathname, not file access or remote authority. The existing
/// receive owner must still open and pin this directory before connecting.
pub(crate) struct Destination(String);

impl Destination {
    pub(crate) fn into_arguments(self) -> [String; 2] {
        ["--receive".to_owned(), self.0]
    }
}

#[derive(Default)]
pub(crate) struct Choice {
    generation: u64,
    pending: Option<Request>,
    destination: Option<Destination>,
}

impl Choice {
    /// A replacement dialog retires any earlier consent immediately. A
    /// cancelled replacement never silently restores the earlier folder.
    pub(crate) fn begin(&mut self, now: u64) -> Result<Request, Error> {
        if self.pending.is_some() {
            return Err(Error::Used);
        }
        self.clear();
        self.generation = self.generation.checked_add(1).ok_or(Error::Used)?;
        let request = Request {
            generation: self.generation,
            started: now,
        };
        self.pending = Some(request);
        Ok(request)
    }

    /// Only the original dialog may publish its bounded, exact pathname.
    /// Cancellation, clock failure and expiry leave file receiving disabled.
    pub(crate) fn finish(
        &mut self,
        request: Request,
        path: Option<&[u16]>,
        now: u64,
    ) -> Result<(), Error> {
        if self.pending != Some(request) {
            return Err(Error::Cancelled);
        }
        self.pending = None;
        let elapsed = now.checked_sub(request.started).ok_or(Error::Clock)?;
        if elapsed >= CHOICE_MS {
            return Err(Error::Expired);
        }
        let Some(path) = path else {
            return Ok(());
        };
        let path = String::from_utf16(path_units(path)?).map_err(|_| Error::InvalidPath)?;
        self.destination = Some(Destination(path));
        Ok(())
    }

    pub(crate) fn clear(&mut self) {
        self.pending = None;
        self.destination = None;
    }

    /// The path is shown only in this local picker; it is never serialized
    /// into settings, completion reports or the session log.
    pub(crate) fn path(&self) -> Option<&str> {
        self.destination
            .as_ref()
            .map(|destination| destination.0.as_str())
    }

    /// Spend the choice exactly once. View-only cannot retain it for a later
    /// connection, and a pending dialog cannot publish after this handoff.
    pub(crate) fn take(&mut self, control: bool) -> Option<Destination> {
        self.pending = None;
        self.destination.take().filter(|_| control)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file_choice::MAX_PATH_UNITS;

    fn path(value: &str) -> Vec<u16> {
        value.encode_utf16().chain([0]).collect()
    }

    fn selected() -> Choice {
        let mut choice = Choice::default();
        let request = choice.begin(10).unwrap();
        choice
            .finish(request, Some(&path(r"C:\Receive here\日本語")), 20)
            .unwrap();
        choice
    }

    #[test]
    fn a_folder_is_used_once_and_is_not_remembered_by_another_picker() {
        let mut choice = selected();
        assert_eq!(choice.path(), Some(r"C:\Receive here\日本語"));
        assert_eq!(
            choice.take(true).unwrap().into_arguments(),
            ["--receive", r"C:\Receive here\日本語"]
        );
        assert!(choice.take(true).is_none());
        assert!(choice.path().is_none());
        assert!(Choice::default().take(true).is_none());
    }

    #[test]
    fn view_only_spends_the_choice_instead_of_deferring_it() {
        let mut choice = selected();
        assert!(choice.take(false).is_none());
        assert!(choice.take(true).is_none());
        assert!(choice.path().is_none());
    }

    #[test]
    fn cancelling_a_replacement_does_not_restore_the_old_folder() {
        let mut choice = selected();
        let request = choice.begin(30).unwrap();
        assert!(choice.path().is_none());
        choice.finish(request, None, 40).unwrap();
        assert!(choice.take(true).is_none());
    }

    #[test]
    fn clearing_or_handing_off_a_picker_fences_a_late_native_result() {
        for handoff in [false, true] {
            let mut choice = Choice::default();
            let request = choice.begin(10).unwrap();
            if handoff {
                assert!(choice.take(true).is_none());
            } else {
                choice.clear();
            }
            assert_eq!(
                choice.finish(request, Some(&path(r"C:\late")), 20),
                Err(Error::Cancelled)
            );
            assert!(choice.path().is_none());
        }
    }

    #[test]
    fn an_old_result_cannot_clear_or_replace_a_newer_destination() {
        let mut choice = Choice::default();
        let old = choice.begin(10).unwrap();
        choice.clear();
        let current = choice.begin(20).unwrap();
        assert_eq!(choice.begin(21), Err(Error::Used));
        choice.finish(current, Some(&path(r"C:\new")), 30).unwrap();
        assert_eq!(
            choice.finish(old, Some(&path(r"C:\old")), 31),
            Err(Error::Cancelled)
        );
        assert_eq!(choice.path(), Some(r"C:\new"));
    }

    #[test]
    fn native_dialog_lifetime_and_exact_utf16_are_enforced() {
        for (path, now, error) in [
            (path(r"C:\ok"), 9, Error::Clock),
            (path(r"C:\ok"), 10 + CHOICE_MS, Error::Expired),
            (vec![0], 20, Error::InvalidPath),
            (vec![u16::from(b'C')], 20, Error::InvalidPath),
            (vec![0xd800, 0], 20, Error::InvalidPath),
            (path("C:\\one\0C:\\two"), 20, Error::InvalidPath),
            (
                vec![u16::from(b'x'); MAX_PATH_UNITS + 1],
                20,
                Error::InvalidPath,
            ),
        ] {
            let mut choice = Choice::default();
            let request = choice.begin(10).unwrap();
            assert_eq!(choice.finish(request, Some(&path), now), Err(error));
            assert!(choice.take(true).is_none());
        }
    }
}
