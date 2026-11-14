//! One native common-file dialog on its own thread. The viewer keeps drawing
//! while the modal OS picker owns local input. The worker holds no authority;
//! it only pins one explicitly selected descriptor after native cleanup.
use crate::file_choice::{Choice, Error, MAX_PATH_UNITS, path_units};
use fr_mobile_ffi::session::{PreparedUpload, Upload};
use std::{
    cell::{Cell, RefCell},
    ffi::OsString,
    os::windows::ffi::OsStringExt,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
};
use windows::{
    Win32::{
        Foundation::{HWND, LPARAM, WPARAM},
        System::SystemInformation::GetTickCount64,
        UI::{
            Controls::{
                Dialogs::{
                    CDN_FILEOK, CommDlgExtendedError, GetOpenFileNameW, OFN_DONTADDTORECENT,
                    OFN_ENABLEHOOK, OFN_EXPLORER, OFN_FILEMUSTEXIST, OFN_HIDEREADONLY,
                    OFN_NOCHANGEDIR, OFN_NODEREFERENCELINKS, OFN_PATHMUSTEXIST, OPENFILENAMEW,
                },
                NMHDR,
            },
            WindowsAndMessaging::{
                DWLP_MSGRESULT, GWLP_USERDATA, GetMessageTime, GetParent, GetWindowLongPtrW,
                IDCANCEL, KillTimer, PostMessageW, SetTimer, SetWindowLongPtrW,
                WINDOW_LONG_PTR_INDEX, WM_COMMAND, WM_INITDIALOG, WM_NCDESTROY, WM_NOTIFY,
                WM_TIMER,
            },
        },
    },
    core::{PWSTR, w},
};

const TIMER: usize = 0x4653;
static RUNNING: AtomicBool = AtomicBool::new(false);
// A late native return stays owned even after the UI's original cleanup deadline
// fails. No later chooser can start until this exact worker is actually joined.
static RETIRING: Mutex<Option<JoinHandle<Result<Selected, Error>>>> = Mutex::new(None);

pub(super) fn ticks() -> u64 {
    // SAFETY: a scalar system-uptime read; includes suspend, unlike frame clocks.
    unsafe { GetTickCount64() }
}

pub(super) struct Selected {
    choice: Choice,
    prepared: PreparedUpload,
}
impl Selected {
    pub(super) fn submit(
        mut self,
        upload: &Upload,
        stop: &AtomicBool,
        mut ready: impl FnMut() -> bool,
    ) -> Result<(), Error> {
        self.choice.check(ticks())?;
        upload
            .submit_if(self.prepared, || {
                // A source/context lock wait spends both original choice clocks.
                // Read the final native clock only after readiness returns.
                ready() && !stop.load(Ordering::SeqCst) && self.choice.finish(ticks()).is_ok()
            })
            .map_err(|_| Error::Selection)
    }
}

pub(super) struct Picker {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<Result<Selected, Error>>>,
}
impl Picker {
    pub(super) fn open(opened: u64) -> Result<Self, Error> {
        collect_retiring();
        if RUNNING
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return Err(Error::Cleanup);
        }
        let choice = match Choice::new(opened) {
            Ok(choice) => choice,
            Err(error) => {
                RUNNING.store(false, Ordering::SeqCst);
                return Err(error);
            }
        };
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        if let Ok(thread) = thread::Builder::new()
            .name("fr-file-choice".into())
            .spawn(move || choose(choice, worker_stop))
        {
            Ok(Self {
                stop,
                thread: Some(thread),
            })
        } else {
            RUNNING.store(false, Ordering::SeqCst);
            Err(Error::Native)
        }
    }

    pub(super) fn cancellation(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.stop)
    }

    pub(super) fn cancel(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }

    /// Join only after actual completion; no blocking native call runs here.
    pub(super) fn poll(&mut self) -> Option<Result<Selected, Error>> {
        if !self.thread.as_ref()?.is_finished() {
            return None;
        }
        let result = self.thread.take()?.join().unwrap_or(Err(Error::Native));
        RUNNING.store(false, Ordering::SeqCst);
        Some(match result {
            Err(error) => Err(error),
            Ok(_) if self.stop.load(Ordering::SeqCst) => Err(Error::Cancelled),
            Ok(selected) => Ok(selected),
        })
    }
}
impl Drop for Picker {
    fn drop(&mut self) {
        self.cancel();
        if let Some(thread) = self.thread.take() {
            if thread.is_finished() {
                let _ = thread.join();
                RUNNING.store(false, Ordering::SeqCst);
            } else {
                // RUNNING prevents any other Picker from owning a worker.
                *RETIRING
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(thread);
            }
        }
    }
}
fn collect_retiring() {
    let mut slot = RETIRING
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if slot.as_ref().is_some_and(JoinHandle::is_finished) {
        if let Some(thread) = slot.take() {
            let _ = thread.join();
        }
        RUNNING.store(false, Ordering::SeqCst);
    }
}

struct Native {
    choice: RefCell<Choice>,
    stop: Arc<AtomicBool>,
    destroyed: Cell<bool>,
    initialized: Cell<bool>,
}
impl Native {
    fn check(&self) -> Result<(), Error> {
        if self.stop.load(Ordering::SeqCst) {
            return self.choice.borrow_mut().refuse(Error::Cancelled);
        }
        self.choice.borrow_mut().check(ticks())
    }
}

fn choose(choice: Choice, stop: Arc<AtomicBool>) -> Result<Selected, Error> {
    let native = Box::new(Native {
        choice: RefCell::new(choice),
        stop,
        destroyed: Cell::new(false),
        initialized: Cell::new(false),
    });
    native.check()?;
    let mut path = vec![0_u16; MAX_PATH_UNITS];
    let mut dialog = OPENFILENAMEW {
        lStructSize: u32::try_from(size_of::<OPENFILENAMEW>()).map_err(|_| Error::Native)?,
        lpstrFile: PWSTR(path.as_mut_ptr()),
        nMaxFile: u32::try_from(path.len()).map_err(|_| Error::Native)?,
        lpstrTitle: w!("Choose one file to send to the current remote host"),
        Flags: OFN_EXPLORER
            | OFN_ENABLEHOOK
            | OFN_FILEMUSTEXIST
            | OFN_PATHMUSTEXIST
            | OFN_NOCHANGEDIR
            | OFN_NODEREFERENCELINKS
            | OFN_DONTADDTORECENT
            | OFN_HIDEREADONLY,
        lCustData: LPARAM((&raw const *native) as isize),
        lpfnHook: Some(hook),
        ..OPENFILENAMEW::default()
    };
    // SAFETY: the dialog and exact bounded UTF-16 buffer remain exclusive and
    // live through this synchronous call. The hook's Native pointer is stable;
    // only this worker thread reads it, and no callback survives the return.
    let selected = unsafe { GetOpenFileNameW(&raw mut dialog) }.as_bool();
    if native.initialized.get() && !native.destroyed.get() {
        return Err(Error::Cleanup);
    }
    native.check()?;
    if !selected {
        // SAFETY: this thread's immediately preceding common-dialog result.
        return Err(if unsafe { CommDlgExtendedError() }.0 == 0 {
            Error::Cancelled
        } else {
            Error::Native
        });
    }
    if !native.destroyed.get() {
        return Err(Error::Cleanup);
    }
    if !native.choice.borrow().selected() {
        return Err(Error::InvalidPath);
    }
    let path = PathBuf::from(OsString::from_wide(path_units(&path)?));
    // The existing Windows custody boundary denies reparse leaves, alternate
    // streams and conflicting writers; it duplicates this actual disk handle.
    let prepared = Upload::prepare(&path).map_err(|_| Error::Selection)?;
    native.check()?;
    Ok(Selected {
        choice: native.choice.into_inner(),
        prepared,
    })
}

extern "system" fn hook(hwnd: HWND, message: u32, _wparam: WPARAM, lparam: LPARAM) -> usize {
    if message == WM_INITDIALOG {
        if lparam.0 == 0 {
            return 0;
        }
        // SAFETY: the documented initialization argument is the live struct
        // passed to GetOpenFileNameW; lCustData is our stable Native allocation.
        let pointer = unsafe { (*(lparam.0 as *const OPENFILENAMEW)).lCustData.0 };
        // SAFETY: this hook child window belongs to this worker and stores only
        // our pointer; the last callback clears it before Native is released.
        unsafe {
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, pointer);
        }
        if pointer != 0 {
            // SAFETY: the original allocation passed to this initialization.
            let native = unsafe { &*(pointer as *const Native) };
            native.initialized.set(true);
            if native
                .check()
                .and_then(|()| native.choice.borrow_mut().shown(ticks()))
                .is_err()
            {
                close_dialog(hwnd);
            }
        }
        // SAFETY: a window-owned timer dispatches only on this dialog thread.
        if unsafe { SetTimer(Some(hwnd), TIMER, 50, None) } == 0 {
            if pointer != 0 {
                // SAFETY: checked pointer from the initialization struct above.
                let native = unsafe { &*(pointer as *const Native) };
                let _ = native.choice.borrow_mut().refuse(Error::Native);
            }
            close_dialog(hwnd);
        }
        return 0;
    }
    // SAFETY: a value installed only by WM_INITDIALOG on this same hook window.
    let pointer = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) };
    if pointer == 0 {
        return 0;
    }
    // SAFETY: Native outlives the synchronous dialog and all hook callbacks.
    let native = unsafe { &*(pointer as *const Native) };
    match message {
        WM_TIMER => {
            if native.check().is_err() {
                close_dialog(hwnd);
            }
        }
        WM_NOTIFY if lparam.0 != 0 => {
            // SAFETY: all native notification messages start with NMHDR.
            // Reading only that header also handles unrelated control notices.
            let notice = unsafe { &*(lparam.0 as *const NMHDR) };
            if notice.code == CDN_FILEOK {
                // SAFETY: original message timestamp on this dialog thread.
                let message = unsafe { GetMessageTime() }.cast_unsigned();
                let accepted = native
                    .check()
                    .and_then(|()| native.choice.borrow_mut().accept(message, ticks()));
                if let Err(error) = accepted {
                    let _ = native.choice.borrow_mut().refuse(error);
                    // SAFETY: veto this File OK callback before asking its
                    // own parent dialog to close. No path can be accepted.
                    unsafe {
                        SetWindowLongPtrW(
                            hwnd,
                            WINDOW_LONG_PTR_INDEX(DWLP_MSGRESULT.cast_signed()),
                            1,
                        );
                    }
                    close_dialog(hwnd);
                    return 1;
                }
            }
        }
        WM_NCDESTROY => {
            native.destroyed.set(true);
            // SAFETY: retire this window's timer and pointer before final
            // destruction; no late callback may borrow Native afterward.
            unsafe {
                let _ = KillTimer(Some(hwnd), TIMER);
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            }
        }
        _ => {}
    }
    0
}
fn close_dialog(child: HWND) {
    // SAFETY: the hook child and parent belong to this current modal operation.
    // Posting cancellation is nonblocking and retains native ownership until
    // the original GetOpenFileNameW call and worker have actually completed.
    unsafe {
        if let Ok(parent) = GetParent(child) {
            let _ = PostMessageW(
                Some(parent),
                WM_COMMAND,
                WPARAM(usize::try_from(IDCANCEL.0).unwrap_or(2)),
                LPARAM(0),
            );
        }
    }
}
