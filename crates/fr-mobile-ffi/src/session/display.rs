//! A native display choice belongs to one approved catalog in one attempt.
//! The UI may wait without blocking the session's network/expiry supervision.
#![forbid(unsafe_code)]

use fr_wire::display::{Catalog, Display};
use std::sync::Arc;

pub const MAX_CHOICES: usize = fr_wire::display::MAX_DISPLAYS;

/// Re-evaluated against the current approved catalog on every attempt.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum Choice {
    /// Preserve the mobile client's default: desktop origin, else first row.
    #[default]
    Primary,
    /// Refuse unless the approved catalog contains exactly one display.
    Only,
    /// Select this exact host-owned alias only if the new catalog contains it.
    Handle(u128),
    /// Wait for a new native choice from this attempt's complete catalog.
    Choose,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    NoDisplays,
    Ambiguous,
    MissingDisplay,
    CatalogChanged,
    StaleRequest,
    AlreadySelected,
    Closed,
}

/// Fixed-size approved metadata and a private, non-reusable local identity.
/// Clones retain the same request; they cannot be rebound to another attempt.
/// No Debug: the opaque host aliases are not diagnostic output.
#[derive(Clone)]
pub struct Request {
    identity: Arc<()>,
    catalog: Catalog,
}

impl Request {
    pub fn displays(&self) -> &[Display] {
        self.catalog.displays()
    }

    fn same(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.identity, &other.identity) && self.catalog == other.catalog
    }
}

/// One bounded request/reply, shared only through the original session lock.
#[derive(Default)]
pub(super) struct Selection {
    active: bool,
    original: Option<Request>,
    answer: Option<u128>,
    failure: Option<Error>,
}

impl Selection {
    pub(super) fn begin(&mut self) {
        self.retire();
        self.active = true;
        self.failure = None;
    }

    pub(super) fn retire(&mut self) {
        self.active = false;
        self.original = None;
        self.answer = None;
    }

    pub(super) const fn failure(&self) -> Option<Error> {
        self.failure
    }

    pub(super) fn request(&self) -> Option<Request> {
        (self.active && self.answer.is_none())
            .then(|| self.original.clone())
            .flatten()
    }

    pub(super) fn current(&self, request: &Request) -> bool {
        self.active
            && self.answer.is_none()
            && self
                .original
                .as_ref()
                .is_some_and(|original| original.same(request))
    }

    pub(super) fn submit(&mut self, request: &Request, handle: u128) -> Result<(), Error> {
        if !self.active
            || !self
                .original
                .as_ref()
                .is_some_and(|original| original.same(request))
        {
            return Err(Error::StaleRequest);
        }
        if self.answer.is_some() {
            return Err(Error::AlreadySelected);
        }
        if request.catalog.find(handle).is_none() {
            return Err(Error::MissingDisplay);
        }
        self.answer = Some(handle);
        Ok(())
    }

    pub(super) fn cancel(&mut self, request: &Request) -> Result<(), Error> {
        if !self.current(request) {
            return Err(Error::StaleRequest);
        }
        self.retire();
        Ok(())
    }

    /// Called only with the original approved catalog supplied by frd.
    /// Returning None keeps driving the existing startup and its deadlines.
    pub(super) fn poll(
        &mut self,
        choice: Choice,
        catalog: &Catalog,
    ) -> Result<Option<u128>, Error> {
        let result = self.poll_inner(choice, catalog);
        if let Err(error) = result {
            self.failure.get_or_insert(error);
            self.retire();
        } else if matches!(result, Ok(Some(_))) {
            self.retire();
        }
        result
    }

    fn poll_inner(&mut self, choice: Choice, catalog: &Catalog) -> Result<Option<u128>, Error> {
        if !self.active {
            return Err(Error::Closed);
        }
        if self
            .original
            .as_ref()
            .is_some_and(|request| request.catalog != *catalog)
        {
            return Err(Error::CatalogChanged);
        }
        let displays = catalog.displays();
        if displays.is_empty() {
            return Err(Error::NoDisplays);
        }
        match choice {
            Choice::Primary => Ok(displays
                .iter()
                .find(|display| display.x == 0 && display.y == 0)
                .or_else(|| displays.first())
                .map(|display| display.handle)),
            Choice::Only => match displays {
                [display] => Ok(Some(display.handle)),
                _ => Err(Error::Ambiguous),
            },
            Choice::Handle(handle) => catalog
                .find(handle)
                .map(|display| Some(display.handle))
                .ok_or(Error::MissingDisplay),
            Choice::Choose => {
                self.original.get_or_insert_with(|| Request {
                    identity: Arc::new(()),
                    catalog: *catalog,
                });
                Ok(self.answer.take())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fr_core::{ids::DisplayGeometryGeneration, limits::ProtocolLimits};

    fn display(handle: u128, x: i32, y: i32) -> Display {
        Display {
            handle,
            geometry: DisplayGeometryGeneration::INITIAL,
            x,
            y,
            pixel_width: 1920,
            pixel_height: 1080,
            logical_width: 1920,
            logical_height: 1080,
            scale_numerator: 1,
            scale_denominator: 1,
            rotation: 0,
        }
    }

    fn catalog(revision: u64, entries: &[Display]) -> Catalog {
        Catalog::new(revision, entries, &ProtocolLimits::ABSOLUTE).unwrap()
    }

    fn started() -> Selection {
        let mut selection = Selection::default();
        selection.begin();
        selection
    }

    #[test]
    fn primary_preserves_origin_then_first_while_explicit_modes_refuse_ambiguity() {
        let two = catalog(1, &[display(7, -1920, -80), display(19, 0, 0)]);
        assert_eq!(started().poll(Choice::Primary, &two), Ok(Some(19)));
        assert_eq!(started().poll(Choice::Only, &two), Err(Error::Ambiguous));
        assert_eq!(started().poll(Choice::Handle(7), &two), Ok(Some(7)));
        assert_eq!(
            started().poll(Choice::Handle(8), &two),
            Err(Error::MissingDisplay)
        );
        let one = catalog(2, &[display(u128::MAX, -1920, 32)]);
        assert_eq!(started().poll(Choice::Primary, &one), Ok(Some(u128::MAX)));
        assert_eq!(started().poll(Choice::Only, &one), Ok(Some(u128::MAX)));
    }

    #[test]
    fn empty_catalog_never_falls_back_or_exposes_a_picker() {
        let empty = catalog(1, &[]);
        for choice in [
            Choice::Primary,
            Choice::Only,
            Choice::Handle(1),
            Choice::Choose,
        ] {
            let mut selection = started();
            assert_eq!(selection.poll(choice, &empty), Err(Error::NoDisplays));
            assert!(selection.request().is_none());
            assert_eq!(selection.failure(), Some(Error::NoDisplays));
        }
    }

    #[test]
    fn native_choice_uses_the_original_catalog_and_is_consumed_once() {
        let catalog = catalog(1, &[display(7, -1920, 0), display(19, 0, 0)]);
        let mut selection = started();
        assert_eq!(selection.poll(Choice::Choose, &catalog), Ok(None));
        let request = selection.request().unwrap();
        assert_eq!(request.displays(), catalog.displays());
        assert_eq!(selection.poll(Choice::Choose, &catalog), Ok(None));
        assert!(selection.current(&request));
        assert_eq!(selection.submit(&request, 99), Err(Error::MissingDisplay));
        assert_eq!(selection.submit(&request, 7), Ok(()));
        assert!(!selection.current(&request));
        assert!(selection.request().is_none());
        assert_eq!(selection.submit(&request, 19), Err(Error::AlreadySelected));
        assert_eq!(selection.poll(Choice::Choose, &catalog), Ok(Some(7)));
        assert_eq!(selection.submit(&request, 7), Err(Error::StaleRequest));
        assert_eq!(selection.poll(Choice::Choose, &catalog), Err(Error::Closed));
    }

    #[test]
    fn another_owner_or_attempt_cannot_reuse_identical_catalog_handles() {
        let catalog = catalog(1, &[display(7, 0, 0)]);
        let mut first = started();
        first.poll(Choice::Choose, &catalog).unwrap();
        let old = first.request().unwrap();
        let mut second = started();
        second.poll(Choice::Choose, &catalog).unwrap();
        assert_eq!(second.submit(&old, 7), Err(Error::StaleRequest));
        first.begin();
        first.poll(Choice::Choose, &catalog).unwrap();
        assert_eq!(first.submit(&old, 7), Err(Error::StaleRequest));
        assert_eq!(first.cancel(&old), Err(Error::StaleRequest));
        let fresh = first.request().unwrap();
        assert_eq!(first.submit(&fresh, 7), Ok(()));
        assert_eq!(first.poll(Choice::Choose, &catalog), Ok(Some(7)));
    }

    #[test]
    fn replacement_catalog_invalidates_even_an_already_queued_choice() {
        let original = catalog(1, &[display(7, 0, 0)]);
        let mut changed_geometry = display(7, -1920, 0);
        changed_geometry.geometry = DisplayGeometryGeneration::from_raw(1);
        for replacement in [
            catalog(2, original.displays()),
            catalog(1, &[changed_geometry]),
            catalog(1, &[display(19, 0, 0)]),
        ] {
            let mut selection = started();
            selection.poll(Choice::Choose, &original).unwrap();
            let request = selection.request().unwrap();
            selection.submit(&request, 7).unwrap();
            assert_eq!(
                selection.poll(Choice::Choose, &replacement),
                Err(Error::CatalogChanged)
            );
            assert!(selection.request().is_none());
            assert_eq!(selection.submit(&request, 7), Err(Error::StaleRequest));
            assert_eq!(selection.failure(), Some(Error::CatalogChanged));
        }
    }

    #[test]
    fn cancel_or_retirement_permanently_invalidates_the_original_request() {
        let catalog = catalog(1, &[display(7, 0, 0)]);
        let mut selection = started();
        selection.poll(Choice::Choose, &catalog).unwrap();
        let request = selection.request().unwrap();
        assert_eq!(selection.cancel(&request), Ok(()));
        assert_eq!(selection.submit(&request, 7), Err(Error::StaleRequest));
        assert_eq!(selection.poll(Choice::Choose, &catalog), Err(Error::Closed));
        selection.begin();
        selection.poll(Choice::Choose, &catalog).unwrap();
        let request = selection.request().unwrap();
        selection.retire();
        assert_eq!(selection.submit(&request, 7), Err(Error::StaleRequest));
    }

    #[test]
    fn maximum_catalog_preserves_every_approved_row_without_numeric_truncation() {
        let entries: Vec<_> = (0..MAX_CHOICES)
            .map(|index| {
                display(
                    u128::MAX - index as u128,
                    i32::try_from(index).unwrap() * -1920,
                    0,
                )
            })
            .collect();
        let catalog = catalog(u64::MAX, &entries);
        let mut selection = started();
        selection.poll(Choice::Choose, &catalog).unwrap();
        let request = selection.request().unwrap();
        assert_eq!(request.displays(), entries.as_slice());
        let last = entries.last().unwrap().handle;
        selection.submit(&request, last).unwrap();
        assert_eq!(selection.poll(Choice::Choose, &catalog), Ok(Some(last)));
    }
}
