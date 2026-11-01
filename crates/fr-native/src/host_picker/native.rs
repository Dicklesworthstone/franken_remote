use super::{Error, PathHint, Row, Shared, Status};
use std::{
    ffi::{CString, c_char, c_int, c_void},
    ptr::NonNull,
    thread,
    time::{Duration, Instant},
};

#[repr(C)]
pub(super) struct NativeRow {
    name: [u8; 255],
    stable_id: [u8; 129],
    path: u32,
}

pub(super) fn rows(rows: &[Row]) -> Vec<NativeRow> {
    rows.iter()
        .map(|row| {
            let mut native = NativeRow {
                name: [0; 255],
                stable_id: [0; 129],
                path: match row.path {
                    PathHint::Direct => 1,
                    PathHint::Relayed => 2,
                    PathHint::Unknown => 0,
                },
            };
            native.name[..row.name.len()].copy_from_slice(row.name.as_bytes());
            native.stable_id[..row.stable_id.len()].copy_from_slice(row.stable_id.as_bytes());
            native
        })
        .collect()
}

unsafe extern "C" {
    fn fr_host_picker_open(
        display: *const c_char,
        rows: *const NativeRow,
        count: u32,
        window: *mut u32,
    ) -> *mut c_void;
    fn fr_host_picker_step(handle: *mut c_void, ordinal: *mut u32, age_ms: *mut u32) -> c_int;
    fn fr_host_picker_close(handle: *mut c_void);
}

struct Native(NonNull<c_void>);
impl Drop for Native {
    fn drop(&mut self) {
        // SAFETY: this worker uniquely owns the live C handle. Close frees the
        // connection/window and never retains pointers or calls into Rust.
        unsafe { fr_host_picker_close(self.0.as_ptr()) };
    }
}

pub(super) fn run(
    shared: &Shared,
    display: &CString,
    rows: &[NativeRow],
) -> Result<Option<usize>, Error> {
    if !shared.live() {
        return Ok(None);
    }
    let mut window = 0;
    let count = u32::try_from(rows.len()).map_err(|_| Error::Capacity)?;
    // SAFETY: bounded, immutable repr(C) rows and the NUL-terminated display
    // outlive the handle on this worker; C only borrows them until close.
    let native = Native(
        NonNull::new(unsafe {
            fr_host_picker_open(display.as_ptr(), rows.as_ptr(), count, &raw mut window)
        })
        .ok_or(Error::NativeUnavailable)?,
    );
    if !shared.live() {
        return Ok(None);
    }
    shared.state().window = Some(window);
    loop {
        if !shared.live() {
            return Ok(None);
        }
        let observed = Instant::now();
        let (mut ordinal, mut age_ms) = (0, 0);
        // SAFETY: this thread alone owns native; the two output pointers are
        // valid for the call. C bounds its event batch before returning.
        let event =
            unsafe { fr_host_picker_step(native.0.as_ptr(), &raw mut ordinal, &raw mut age_ms) };
        if !shared.live() {
            return Ok(None);
        }
        match event {
            0 => {}
            1 => {
                let mut state = shared.state();
                if state.status == Status::Opening {
                    state.mapped = true;
                    state.status = Status::Mapped;
                }
            }
            2 => {
                // Sampling before the native call conservatively includes
                // query/preemption time; a late choice never gets a fresh age.
                let age =
                    Duration::from_millis(u64::from(age_ms)).saturating_add(observed.elapsed());
                if age < Duration::from_millis(250) {
                    let ordinal = usize::try_from(ordinal).map_err(|_| Error::NativeChanged)?;
                    if ordinal >= rows.len() {
                        return Err(Error::NativeChanged);
                    }
                    return Ok(Some(ordinal));
                }
            }
            3 => return Ok(None),
            4 => return Err(Error::NativeChanged),
            -2 => return Err(Error::EventFlood),
            _ => return Err(Error::NativeUnavailable),
        }
        thread::sleep(Duration::from_millis(8));
    }
}
