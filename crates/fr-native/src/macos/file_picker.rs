//! Main-thread native file selection before opening a remote session.
//! `AppKit` retains no Rust pointers; the caller opens the chosen descriptors
//! only after this original native owner confirms cleanup.
pub use crate::macos_file_choice::Error;
use crate::{file_drop::MAX_BYTES, macos_file_choice::Choice};
use std::{
    ffi::{c_int, c_void},
    marker::PhantomData,
    path::PathBuf,
    ptr::NonNull,
    rc::Rc,
    time::Instant,
};

unsafe extern "C" {
    fn fr_mac_file_picker_open(out: *mut *mut c_void) -> c_int;
    fn fr_mac_file_picker_poll(
        handle: *mut c_void,
        bytes: *mut u8,
        capacity: usize,
        length: *mut usize,
        age: *mut f64,
    ) -> c_int;
    fn fr_mac_file_picker_close(handle: *mut c_void) -> c_int;
}

fn native_error(code: c_int) -> Error {
    match code {
        -1 => Error::InvalidSelection,
        -2 => Error::WrongThread,
        -4 => Error::Cancelled,
        -5 => Error::EventFlood,
        -6 => Error::Expired,
        _ => Error::NativeFailure,
    }
}

pub struct FilePicker {
    handle: Option<NonNull<c_void>>,
    choice: Choice,
    bytes: Box<[u8]>,
    _main_thread: PhantomData<Rc<()>>,
}
impl FilePicker {
    pub fn open() -> Result<Self, Error> {
        let opened = Instant::now();
        let mut handle = std::ptr::null_mut();
        // SAFETY: output is an exclusive scalar; native code retains no Rust
        // pointer and refuses a non-main-thread call before creating its owner.
        let result = unsafe { fr_mac_file_picker_open(&raw mut handle) };
        if result != 0 {
            return Err(native_error(result));
        }
        Ok(Self {
            handle: Some(NonNull::new(handle).ok_or(Error::NativeFailure)?),
            choice: Choice::new(opened),
            bytes: vec![0_u8; MAX_BYTES].into_boxed_slice(),
            _main_thread: PhantomData,
        })
    }
    pub fn poll(&mut self) -> Result<Option<Vec<PathBuf>>, Error> {
        self.choice.check(Instant::now())?;
        if self.choice.waiting() {
            let handle = self.handle.ok_or(Error::AlreadyUsed)?;
            let (mut length, mut age) = (0, 0.0);
            let before = Instant::now();
            // SAFETY: owner remains live and main-thread confined; the bounded
            // output and scalars are exclusive for this call and never retained.
            let result = unsafe {
                fr_mac_file_picker_poll(
                    handle.as_ptr(),
                    self.bytes.as_mut_ptr(),
                    self.bytes.len(),
                    &raw mut length,
                    &raw mut age,
                )
            };
            if result == 0 {
                return Ok(None);
            }
            if result != 1 {
                self.choice.refuse(native_error(result))?;
            }
            if length > self.bytes.len() {
                self.choice.refuse(Error::InvalidSelection)?;
            }
            // Project native age from BEFORE the foreign call. Preemption or
            // a slow AppKit turn can only shorten this original selection's
            // lifetime, never grant it a fresh two seconds on Rust receipt.
            self.choice.select(&self.bytes[..length], age, before)?;
        }
        let handle = &mut self.handle;
        self.choice.finish(Instant::now, || close_native(handle))
    }
    pub fn close(&mut self) -> Result<(), Error> {
        let _ = self.choice.refuse(Error::Cancelled);
        close_native(&mut self.handle)
    }
}
fn close_native(handle: &mut Option<NonNull<c_void>>) -> Result<(), Error> {
    let Some(pointer) = *handle else {
        return Ok(());
    };
    // SAFETY: this unique retained native owner never leaves the main thread.
    // Success consumes its retain; failure leaves the same handle owned here.
    if unsafe { fr_mac_file_picker_close(pointer.as_ptr()) } != 0 {
        return Err(Error::CleanupIncomplete);
    }
    *handle = None;
    Ok(())
}
impl Drop for FilePicker {
    fn drop(&mut self) {
        let _ = self.close();
    }
}
