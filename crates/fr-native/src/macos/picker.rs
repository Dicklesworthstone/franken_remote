//! Nonmodal, main-thread native choices over one immutable local snapshot.
//!
//! `AppKit` receives bounded copied labels, never a peer address, input lease or
//! Rust callback. A selected row is resolved only against its original Rust
//! catalog, after the native owner has actually closed. Neither picker grants
//! authority: the host caller must revalidate the returned ID/name tuple, and
//! the display caller must retain its original approved selection exchange.
use core::{
    ffi::{c_char, c_int, c_void},
    marker::PhantomData,
    ptr::NonNull,
};
use fr_client::discovery::picker::{Candidate, Catalog as HostCatalog, MAX_CANDIDATES, PathHint};
use fr_wire::display::Catalog as DisplayCatalog;
use std::{
    fmt,
    rc::Rc,
    time::{Duration, Instant},
};

const MAX_LABEL: usize = 384;
const CHOICE_LIMIT: Duration = Duration::from_secs(120);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    EmptyCatalog,
    InvalidRows,
    NativeFailure,
    WrongThread,
    Cancelled,
    CatalogChanged,
    EventFlood,
    AlreadyUsed,
    ChoiceTimeout,
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for Error {}

struct Row {
    title: String,
    detail: String,
}
#[repr(C)]
struct RawRow {
    title: *const c_char,
    title_len: usize,
    detail: *const c_char,
    detail_len: usize,
}
unsafe extern "C" {
    fn fr_mac_picker_open(
        kind: u32,
        rows: *const RawRow,
        count: usize,
        out: *mut *mut c_void,
    ) -> c_int;
    fn fr_mac_picker_poll(handle: *mut c_void, row: *mut usize) -> c_int;
    fn fr_mac_picker_close(handle: *mut c_void) -> c_int;
}

fn native_error(code: c_int) -> Error {
    match code {
        -1 => Error::InvalidRows,
        -2 => Error::WrongThread,
        -4 => Error::Cancelled,
        -5 => Error::EventFlood,
        _ => Error::NativeFailure,
    }
}

/// Owns exactly one retained Objective-C object. The marker prevents moving
/// this owner off `AppKit`'s main thread, including while a future is suspended.
struct Native {
    handle: Option<NonNull<c_void>>,
    _main_thread: PhantomData<Rc<()>>,
}
impl Native {
    fn open(kind: u32, rows: &[Row]) -> Result<Self, Error> {
        if rows.is_empty() {
            return Err(Error::EmptyCatalog);
        }
        if rows.len() > MAX_CANDIDATES
            || rows.iter().any(|row| {
                [&row.title, &row.detail].into_iter().any(|label| {
                    label.is_empty()
                        || label.len() > MAX_LABEL
                        || !label
                            .bytes()
                            .all(|byte| byte == b' ' || byte.is_ascii_graphic())
                })
            })
        {
            return Err(Error::InvalidRows);
        }
        let raw: Vec<_> = rows
            .iter()
            .map(|row| RawRow {
                title: row.title.as_ptr().cast(),
                title_len: row.title.len(),
                detail: row.detail.as_ptr().cast(),
                detail_len: row.detail.len(),
            })
            .collect();
        let mut pointer = core::ptr::null_mut();
        // SAFETY: each bounded row and byte range remains borrowed throughout
        // this synchronous call; native code copies all text before returning,
        // retains no Rust pointer, and refuses a non-main-thread invocation.
        let code = unsafe { fr_mac_picker_open(kind, raw.as_ptr(), raw.len(), &raw mut pointer) };
        if code != 0 {
            return Err(native_error(code));
        }
        Ok(Self {
            handle: Some(NonNull::new(pointer).ok_or(Error::NativeFailure)?),
            _main_thread: PhantomData,
        })
    }
    fn poll(&mut self) -> Result<Option<usize>, Error> {
        let handle = self.handle.ok_or(Error::AlreadyUsed)?;
        let mut row = usize::MAX;
        // SAFETY: this is the original live retained main-thread owner; the
        // output points to a valid exclusive scalar. Native polling does not
        // retain it or invoke Rust, and drains at most 128 AppKit events.
        match unsafe { fr_mac_picker_poll(handle.as_ptr(), &raw mut row) } {
            0 => Ok(None),
            1 => Ok(Some(row)),
            code => Err(native_error(code)),
        }
    }
    fn close(&mut self) -> Result<(), Error> {
        let Some(handle) = self.handle else {
            return Ok(());
        };
        // SAFETY: this original retained owner remains main-thread confined.
        // Success retires delegates/targets/window and consumes one retain;
        // failure keeps that same retain valid for a subsequent cleanup call.
        let code = unsafe { fr_mac_picker_close(handle.as_ptr()) };
        if code != 0 {
            return Err(native_error(code));
        }
        self.handle = None;
        Ok(())
    }
}
impl Drop for Native {
    fn drop(&mut self) {
        // Explicit callers retain/report close errors. Drop cannot turn an
        // unconfirmed close into a choice or permit a replacement connection.
        let _ = self.close();
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Decision {
    Waiting,
    Chosen(usize),
    Ended(Error),
    Consumed,
}
impl Decision {
    fn publish(
        &mut self,
        close: impl FnOnce() -> Result<(), Error>,
    ) -> Result<Option<usize>, Error> {
        match *self {
            Self::Chosen(row) => {
                close()?;
                *self = Self::Consumed;
                Ok(Some(row))
            }
            Self::Ended(error) => Err(error),
            Self::Waiting => Ok(None),
            Self::Consumed => Err(Error::AlreadyUsed),
        }
    }
    fn cancel(&mut self) {
        if !matches!(self, Self::Consumed | Self::Ended(_)) {
            *self = Self::Ended(Error::Cancelled);
        }
    }
}
struct Picker {
    native: Native,
    rows: usize,
    decision: Decision,
    opened: Instant,
}
impl Picker {
    fn open(kind: u32, rows: &[Row]) -> Result<Self, Error> {
        let opened = Instant::now();
        Ok(Self {
            native: Native::open(kind, rows)?,
            rows: rows.len(),
            decision: Decision::Waiting,
            opened,
        })
    }
    fn poll(&mut self) -> Result<Option<usize>, Error> {
        if matches!(self.decision, Decision::Consumed) {
            return Err(Error::AlreadyUsed);
        }
        if !matches!(self.decision, Decision::Ended(_)) && self.opened.elapsed() >= CHOICE_LIMIT {
            self.decision = Decision::Ended(Error::ChoiceTimeout);
        }
        if let Decision::Ended(error) = self.decision {
            return Err(error);
        }
        if matches!(self.decision, Decision::Waiting) {
            match self.native.poll() {
                Ok(None) => return Ok(None),
                Ok(Some(row)) if row < self.rows => self.decision = Decision::Chosen(row),
                Ok(Some(_)) => self.decision = Decision::Ended(Error::InvalidRows),
                Err(error) => self.decision = Decision::Ended(error),
            }
        }
        // A failed close retains this exact choice and owner. A later poll
        // may retry cleanup, but never obtains a different row.
        self.decision.publish(|| self.native.close())
    }
    fn close(&mut self) -> Result<(), Error> {
        self.decision.cancel();
        self.native.close()
    }
}

/// A local discovery choice, before creating a remote session. Its result is
/// the exact original candidate, not an ordinal into a later discovery result.
pub struct HostPicker {
    original: HostCatalog,
    picker: Picker,
}
impl HostPicker {
    pub fn open(original: HostCatalog) -> Result<Self, Error> {
        let rows = host_rows(&original);
        Ok(Self {
            picker: Picker::open(1, &rows)?,
            original,
        })
    }
    /// Poll a bounded `AppKit` turn. Selection is returned once, only after
    /// positive native cleanup; the caller must recheck its exact ID/name.
    pub fn poll(&mut self) -> Result<Option<Candidate>, Error> {
        self.picker
            .poll()?
            .map(|row| {
                self.original
                    .get(row)
                    .cloned()
                    .map_err(|_| Error::InvalidRows)
            })
            .transpose()
    }
    /// Fence/cancel the surrounding operation before calling this method.
    /// An error preserves the exact native owner for a later cleanup attempt.
    pub fn close(&mut self) -> Result<(), Error> {
        self.picker.close()
    }
}

/// A choice whose aliases belong only to one approved, immutable host catalog.
/// The containing session retains authority, cancellation and lifetime checks.
pub struct DisplayPicker {
    original: DisplayCatalog,
    picker: Picker,
}
impl DisplayPicker {
    pub fn open(original: &DisplayCatalog) -> Result<Self, Error> {
        let rows = display_rows(original);
        Ok(Self {
            picker: Picker::open(2, &rows)?,
            original: *original,
        })
    }
    /// A different revision, ordering, handle or geometry invalidates the old
    /// choice. It never reinterprets a queued native row against a new catalog.
    pub fn poll(&mut self, current: &DisplayCatalog) -> Result<Option<u128>, Error> {
        if current != &self.original {
            self.picker.decision = Decision::Ended(Error::CatalogChanged);
            return Err(Error::CatalogChanged);
        }
        self.picker
            .poll()?
            .map(|row| {
                self.original
                    .displays()
                    .get(row)
                    .map(|display| display.handle)
                    .ok_or(Error::InvalidRows)
            })
            .transpose()
    }
    /// The original session must stop before this main-thread cleanup call.
    pub fn close(&mut self) -> Result<(), Error> {
        self.picker.close()
    }
}

fn host_rows(catalog: &HostCatalog) -> Vec<Row> {
    catalog
        .candidates()
        .iter()
        .map(|candidate| Row {
            title: candidate.certificate_name().to_owned(),
            detail: format!(
                "{}  |  {}",
                candidate.stable_id(),
                match candidate.path_hint() {
                    PathHint::Direct => "direct path reported",
                    PathHint::Relayed => "relayed path reported",
                    PathHint::Unknown => "path unknown",
                }
            ),
        })
        .collect()
}
fn display_rows(catalog: &DisplayCatalog) -> Vec<Row> {
    catalog
        .displays()
        .iter()
        .enumerate()
        .map(|(index, display)| Row {
            title: format!(
                "Display {}: {} x {} pixels at ({}, {})",
                index + 1,
                display.pixel_width,
                display.pixel_height,
                display.x,
                display.y
            ),
            detail: format!(
                "Logical {} x {}; scale {}/{}; rotation {} degrees",
                display.logical_width,
                display.logical_height,
                display.scale_numerator,
                display.scale_denominator,
                u32::from(display.rotation) * 90
            ),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use fr_core::{ids::DisplayGeometryGeneration, limits::ProtocolLimits};
    use fr_wire::display::Display;

    #[test]
    fn host_rows_preserve_full_identity_after_catalog_sorting() {
        let catalog = HostCatalog::new(vec![
            Candidate::new("n-z".into(), "zulu.tail.ts.net".into(), PathHint::Unknown).unwrap(),
            Candidate::new("n-a".into(), "alpha.tail.ts.net".into(), PathHint::Relayed).unwrap(),
        ])
        .unwrap();
        let rows = host_rows(&catalog);
        assert_eq!(rows[0].title, "alpha.tail.ts.net");
        assert_eq!(rows[0].detail, "n-a  |  relayed path reported");
        assert_eq!(catalog.get(0).unwrap().stable_id(), "n-a");
        let maximum = Candidate::new(
            "n".repeat(128),
            format!(
                "{}.{}.{}.{}",
                "a".repeat(63),
                "b".repeat(63),
                "c".repeat(63),
                "d".repeat(61)
            ),
            PathHint::Direct,
        )
        .unwrap();
        let maximum = host_rows(&HostCatalog::new(vec![maximum]).unwrap());
        assert_eq!(maximum[0].title.len(), 253);
        assert!(maximum[0].detail.len() <= MAX_LABEL);
    }

    #[test]
    fn display_rows_show_actual_geometry_without_rounding_aliases() {
        let display = Display {
            handle: u128::MAX,
            geometry: DisplayGeometryGeneration::INITIAL,
            x: -1920,
            y: 0,
            pixel_width: 1920,
            pixel_height: 1080,
            logical_width: 1280,
            logical_height: 720,
            scale_numerator: 3,
            scale_denominator: 2,
            rotation: 1,
        };
        let catalog = DisplayCatalog::new(1, &[display], &ProtocolLimits::ABSOLUTE).unwrap();
        let rows = display_rows(&catalog);
        assert_eq!(rows[0].title, "Display 1: 1920 x 1080 pixels at (-1920, 0)");
        assert_eq!(
            rows[0].detail,
            "Logical 1280 x 720; scale 3/2; rotation 90 degrees"
        );
        assert_eq!(catalog.displays()[0].handle, u128::MAX);
    }

    #[test]
    fn cleanup_failure_cannot_publish_or_replace_an_original_choice() {
        let catalog = HostCatalog::new(vec![
            Candidate::new("n-a".into(), "alpha.tail.ts.net".into(), PathHint::Direct).unwrap(),
            Candidate::new("n-z".into(), "zulu.tail.ts.net".into(), PathHint::Unknown).unwrap(),
        ])
        .unwrap();
        let mut decision = Decision::Chosen(1);
        assert_eq!(
            decision.publish(|| Err(Error::NativeFailure)),
            Err(Error::NativeFailure)
        );
        assert_eq!(decision, Decision::Chosen(1));
        let original = decision.publish(|| Ok(())).unwrap().unwrap();
        assert_eq!(catalog.get(original).unwrap().stable_id(), "n-z");
        assert_eq!(
            decision.publish(|| panic!("consumed native owner must not close again")),
            Err(Error::AlreadyUsed)
        );
    }

    #[test]
    fn cancellation_discards_a_decision_awaiting_cleanup_and_preserves_terminal_reason() {
        let mut decision = Decision::Chosen(0);
        assert_eq!(
            decision.publish(|| Err(Error::NativeFailure)),
            Err(Error::NativeFailure)
        );
        decision.cancel();
        assert_eq!(
            decision.publish(|| panic!("cancelled choice must not publish")),
            Err(Error::Cancelled)
        );
        let mut changed = Decision::Ended(Error::CatalogChanged);
        changed.cancel();
        assert_eq!(
            changed.publish(|| panic!("changed snapshot must not publish")),
            Err(Error::CatalogChanged)
        );
    }
}
