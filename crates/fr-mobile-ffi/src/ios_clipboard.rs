//! Real `UIKit` text clipboard for the existing native clipboard worker. `UIKit`
//! calls are synchronous main-queue operations with original authority checks;
//! no native thread or borrowed buffer is detached while a call is pending.
use crate::ios_clipboard_policy::{Metadata, Privacy, Revisions};
use asupersync::cx::Cx;
use core::ffi::{c_char, c_void};
use fr_core::{clipboard::PlatformError, limits::ProtocolLimits};
use frd::{
    clipboard_quic::{NativeAccess, NativeOpenError},
    native_clipboard::Control,
    windows_clipboard::{Clipboard, Failure, Native},
};
use std::{
    ffi::CString,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

#[derive(Clone, Copy, Default)]
#[repr(C)]
struct NativeMetadata {
    count: u64,
    text: u32,
    own: u32,
}
impl NativeMetadata {
    fn checked(self) -> Result<Metadata, Failure> {
        if self.text > 1 || self.own > 1 {
            return Err(Failure::Unavailable);
        }
        Ok(Metadata {
            count: self.count,
            text: self.text != 0,
            own: self.own != 0,
        })
    }
}
impl From<Metadata> for NativeMetadata {
    fn from(value: Metadata) -> Self {
        Self {
            count: value.count,
            text: u32::from(value.text),
            own: u32::from(value.own),
        }
    }
}

struct Gate<'a>(&'a (dyn Fn() -> bool + Sync));

unsafe extern "C" fn check(context: *const c_void) -> bool {
    // SAFETY: each synchronous bridge call borrows this exact Gate until its
    // main-queue block completes; no block or pointer escapes the call.
    let gate = unsafe { &*context.cast::<Gate<'_>>() };
    // A failed platform guard must refuse the native call, never unwind through
    // Objective-C or dispatch_sync. The guard itself performs no native work.
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| (gate.0)())).unwrap_or(false)
}

unsafe extern "C" {
    fn fr_ios_clipboard_metadata(
        owned: *const c_char,
        gate: unsafe extern "C" fn(*const c_void) -> bool,
        context: *const c_void,
        out: *mut NativeMetadata,
    ) -> i32;
    fn fr_ios_clipboard_read(
        owned: *const c_char,
        expected: NativeMetadata,
        gate: unsafe extern "C" fn(*const c_void) -> bool,
        context: *const c_void,
        out: *mut u8,
        capacity: usize,
        length: *mut usize,
    ) -> i32;
    fn fr_ios_clipboard_write(
        owned: *const c_char,
        next: *const c_char,
        expected: NativeMetadata,
        text: *const u8,
        length: usize,
        gate: unsafe extern "C" fn(*const c_void) -> bool,
        context: *const c_void,
        out: *mut NativeMetadata,
        submitted: *mut u32,
    ) -> i32;
}

fn status(code: i32) -> Result<(), Failure> {
    match code {
        0 => Ok(()),
        2 => Err(Failure::NoText),
        3 => Err(Failure::Limit),
        4 => Err(Failure::Changed),
        _ => Err(Failure::Unavailable),
    }
}

pub(crate) struct Text(Vec<u8>);
impl AsRef<str> for Text {
    fn as_ref(&self) -> &str {
        std::str::from_utf8(&self.0).expect("validated native UTF-8")
    }
}
impl Drop for Text {
    fn drop(&mut self) {
        self.0.fill(0);
    }
}

pub(crate) struct System {
    privacy: Privacy,
    source: Control,
    access: Arc<NativeAccess>,
    failed: Arc<AtomicBool>,
    revisions: Revisions,
    prefix: String,
    marker: Option<CString>,
    serial: u64,
}

impl System {
    fn owned(&self) -> *const c_char {
        self.marker
            .as_ref()
            .map_or(std::ptr::null(), |value| value.as_ptr())
    }

    fn allowed(&self) -> bool {
        !self.failed.load(Ordering::Acquire)
            && self.privacy.live()
            && self.source.status().enabled
            && self.access.check(None)
    }

    fn metadata(&self) -> Result<Metadata, Failure> {
        let allowed = || self.allowed();
        let gate = Gate(&allowed);
        let mut metadata = NativeMetadata::default();
        // SAFETY: all pointers are valid for the complete synchronous native
        // call, including its main-queue handoff. Output has the C layout above.
        status(unsafe {
            fr_ios_clipboard_metadata(
                self.owned(),
                check,
                std::ptr::from_ref(&gate).cast(),
                &mut metadata,
            )
        })?;
        metadata.checked()
    }

    fn fail(&self) {
        self.failed.store(true, Ordering::Release);
        self.source.stop();
    }
}

impl Native for System {
    type Text = Text;

    fn sequence(&mut self) -> u32 {
        match self
            .metadata()
            .and_then(|value| self.revisions.observe(value).map_err(|()| Failure::Changed))
        {
            Ok(revision) => revision,
            Err(_) => {
                self.fail();
                self.revisions.revision()
            }
        }
    }

    fn has_text(&mut self) -> bool {
        self.revisions.current().is_some_and(|value| value.text)
    }

    fn read(
        &mut self,
        limit: usize,
        expected: u32,
        allowed: &(dyn Fn() -> bool + Sync),
    ) -> Result<Text, Failure> {
        if self.revisions.revision() != expected {
            return Err(Failure::Changed);
        }
        let metadata = self.revisions.current().ok_or(Failure::Unavailable)?;
        let mut text = Text(Vec::new());
        text.0
            .try_reserve_exact(limit)
            .map_err(|_| Failure::Limit)?;
        text.0.resize(limit, 0);
        let mut length = 0;
        let gate = Gate(allowed);
        // SAFETY: writable bounded buffer, exact metadata layout and borrowed
        // gate remain live until the original UIKit operation actually returns.
        status(unsafe {
            fr_ios_clipboard_read(
                self.owned(),
                metadata.into(),
                check,
                std::ptr::from_ref(&gate).cast(),
                text.0.as_mut_ptr(),
                limit,
                &mut length,
            )
        })?;
        if length > limit || std::str::from_utf8(&text.0[..length]).is_err() {
            return Err(Failure::Invalid);
        }
        text.0.truncate(length);
        Ok(text)
    }

    fn write(
        &mut self,
        text: &str,
        expected: u32,
        allowed: &(dyn Fn() -> bool + Sync),
    ) -> Result<u32, (Failure, bool)> {
        if self.revisions.revision() != expected {
            return Err((Failure::Changed, false));
        }
        let metadata = self
            .revisions
            .current()
            .ok_or((Failure::Unavailable, false))?;
        self.serial = self.serial.checked_add(1).ok_or((Failure::Limit, false))?;
        let marker = CString::new(format!("{}.{}", self.prefix, self.serial))
            .map_err(|_| (Failure::Invalid, false))?;
        let gate = Gate(allowed);
        let mut next = NativeMetadata::default();
        let mut submitted = 0;
        // SAFETY: UTF-8 and marker buffers plus the guard are borrowed only by
        // this synchronous call; the mutation flag survives an ObjC exception.
        let result = unsafe {
            fr_ios_clipboard_write(
                self.owned(),
                marker.as_ptr(),
                metadata.into(),
                text.as_ptr(),
                text.len(),
                check,
                std::ptr::from_ref(&gate).cast(),
                &mut next,
                &mut submitted,
            )
        };
        status(result).map_err(|error| (error, submitted != 0))?;
        let revision = self
            .revisions
            .published(next.checked().map_err(|error| (error, true))?)
            .map_err(|()| (Failure::Changed, true))?;
        self.marker = Some(marker);
        Ok(revision)
    }

    fn close(&mut self) {
        self.failed.store(true, Ordering::Release);
    }
}

pub(crate) fn open(
    source: Control,
    access: NativeAccess,
    privacy: Privacy,
    entropy: &Cx,
) -> Result<Clipboard<System>, NativeOpenError> {
    // The first control grant can precede the SwiftUI task's first turn. This
    // one original setup wait performs no native access and retains the same
    // authority; it cannot revive a hidden/stopped privacy surface.
    let until = std::time::Instant::now() + std::time::Duration::from_millis(250);
    while !privacy.live() {
        if privacy.stopped()
            || !source.status().enabled
            || !access.check(None)
            || std::time::Instant::now() >= until
        {
            return Err(NativeOpenError::Native(PlatformError::Unavailable));
        }
        std::thread::park_timeout(std::time::Duration::from_millis(5));
    }
    let mut nonce = [0; 16];
    entropy.random_bytes(&mut nonce);
    if nonce == [0; 16] {
        return Err(NativeOpenError::Native(PlatformError::Unavailable));
    }
    let prefix = format!(
        "org.frankenremote.clipboard.{:032x}",
        u128::from_be_bytes(nonce)
    );
    let access = Arc::new(access);
    let failed = Arc::new(AtomicBool::new(false));
    let native = System {
        privacy: privacy.clone(),
        source: source.clone(),
        access: access.clone(),
        failed: failed.clone(),
        revisions: Revisions::default(),
        prefix,
        marker: None,
        serial: 0,
    };
    Clipboard::open(native, &ProtocolLimits::ABSOLUTE, move |until| {
        if failed.load(Ordering::Acquire) || !privacy.live() {
            source.stop();
            return false;
        }
        source.status().enabled && access.check(until)
    })
}
