//! One native file-selection/preparation worker. The viewer keeps drawing
//! during a dialog or delayed clipboard rendering. The worker holds no
//! authority; it pins one explicitly selected batch after native cleanup.
#[path = "copied_files.rs"]
mod copied_files;
use super::Source;
use crate::file_choice::{
    Choice, Error, Focus as FocusPolicy, MAX_PATH_UNITS, copied_paths, selected_paths,
};
use fr_mobile_ffi::session::{FileChoiceContext, PreparedUpload, Shared, Upload};
use std::{
    cell::{Cell, RefCell},
    ffi::OsString,
    os::windows::ffi::OsStringExt,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Sender},
    },
    thread::{self, JoinHandle},
};
use windows::{
    Win32::{
        Foundation::{HWND, LPARAM, LRESULT, WPARAM},
        System::{SystemInformation::GetTickCount64, Threading::GetCurrentThreadId},
        UI::{
            Controls::{
                Dialogs::{
                    CDN_FILEOK, CommDlgExtendedError, GetOpenFileNameW, OFN_ALLOWMULTISELECT,
                    OFN_DONTADDTORECENT, OFN_ENABLEHOOK, OFN_EXPLORER, OFN_FILEMUSTEXIST,
                    OFN_HIDEREADONLY, OFN_NOCHANGEDIR, OFN_NODEREFERENCELINKS, OFN_PATHMUSTEXIST,
                    OPENFILENAMEW,
                },
                NMHDR,
            },
            Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass},
            WindowsAndMessaging::{
                DWLP_MSGRESULT, GA_ROOTOWNER, GWLP_USERDATA, GetAncestor, GetForegroundWindow,
                GetMessageTime, GetParent, GetWindowLongPtrW, GetWindowThreadProcessId, IDCANCEL,
                KillTimer, PostMessageW, SetTimer, SetWindowLongPtrW, WINDOW_LONG_PTR_INDEX,
                WM_ACTIVATEAPP, WM_COMMAND, WM_INITDIALOG, WM_NCDESTROY, WM_NOTIFY, WM_TIMER,
            },
        },
    },
    core::{PWSTR, w},
};

const TIMER: usize = 0x4653;
const SUBCLASS: usize = 0x4654;
static RUNNING: AtomicBool = AtomicBool::new(false);
// A late native return stays owned even after the UI's original cleanup deadline
// fails. No later chooser can start until this exact worker is actually joined.
static RETIRING: Mutex<Option<JoinHandle<Result<Selected, Error>>>> = Mutex::new(None);

pub(super) fn ticks() -> u64 {
    // SAFETY: a scalar system-uptime read; includes suspend, unlike frame clocks.
    unsafe { GetTickCount64() }
}

pub(super) fn clipboard_sequence() -> u32 {
    copied_files::sequence()
}

pub(super) struct Selected {
    choice: Choice,
    prepared: PreparedUpload,
    focus: Arc<Focus>,
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
                ready()
                    && self.focus.owner_foreground()
                    && !stop.load(Ordering::SeqCst)
                    && self.choice.finish(ticks()).is_ok()
            })
            .map_err(|_| Error::Selection)
    }
}

pub(super) struct Picker {
    stop: Arc<AtomicBool>,
    focus: Arc<Focus>,
    start: Option<Sender<()>>,
    thread: Option<JoinHandle<Result<Selected, Error>>>,
}
impl Picker {
    pub(super) fn open(
        opened: u64,
        original: Arc<Shared>,
        context: FileChoiceContext,
        owner: HWND,
        lifetime: u64,
        source: Source,
    ) -> Result<Self, Error> {
        collect_retiring();
        if RUNNING
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return Err(Error::Cleanup);
        }
        let choice = match match &source {
            Source::Dialog => Choice::new(opened),
            Source::Copied(_) => Choice::from_gesture(opened, ticks()),
        } {
            Ok(choice) => choice,
            Err(error) => {
                RUNNING.store(false, Ordering::SeqCst);
                return Err(error);
            }
        };
        let stop = Arc::new(AtomicBool::new(false));
        // SAFETY: identity reads only; the caller owns this live viewer window.
        let owner_thread = unsafe { GetWindowThreadProcessId(owner, None) };
        let Some(policy) = FocusPolicy::new(owner_thread, owner.0 as usize, lifetime) else {
            RUNNING.store(false, Ordering::SeqCst);
            return Err(Error::Native);
        };
        let focus = Arc::new(Focus {
            policy: Mutex::new(policy),
            attempt: original.frames.app_id(0),
            original,
            context,
            stop: Arc::clone(&stop),
            hidden: AtomicBool::new(false),
        });
        let worker_focus = Arc::clone(&focus);
        let (start, started) = mpsc::channel();
        if let Ok(thread) = thread::Builder::new()
            .name("fr-file-choice".into())
            .spawn(move || {
                // No native window can activate before the UI publishes this
                // exact Picker/Focus into its original choice registration.
                started.recv().map_err(|_| Error::Cancelled)?;
                if let Source::Copied(sequence) = source {
                    // No dialog owns local input. In particular, never give
                    // this worker thread the dialog's activation exemption.
                    return choose_copied(choice, worker_focus, sequence);
                }
                // SAFETY: identify this worker before creating any native UI.
                let worker = unsafe { GetCurrentThreadId() };
                if !worker_focus
                    .policy
                    .lock()
                    .is_ok_and(|mut policy| policy.register_worker(worker))
                {
                    return Err(Error::Native);
                }
                let registration = WorkerFocus(Arc::clone(&worker_focus));
                let result = choose(choice, worker_focus);
                // Thread IDs can be reused even while their handles stay open.
                // Retire the exemption before exiting, including error paths.
                drop(registration);
                result
            })
        {
            Ok(Self {
                stop,
                focus,
                start: Some(start),
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

    pub(super) fn focus(&self) -> Arc<Focus> {
        Arc::clone(&self.focus)
    }

    /// Called only after both Menu and its reentrant TLS registration exist.
    pub(super) fn start(&mut self) {
        let published = self
            .focus
            .policy
            .lock()
            .is_ok_and(|mut policy| policy.publish());
        if !published
            || self
                .start
                .take()
                .is_none_or(|start| start.send(()).is_err())
        {
            self.cancel();
            self.start = None;
        }
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
        self.start = None;
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

/// Native callbacks can stop only the captured grant. They never restore
/// visibility or apply their delayed result to a replacement connection.
pub(super) struct Focus {
    policy: Mutex<FocusPolicy>,
    attempt: u64,
    original: Arc<Shared>,
    context: FileChoiceContext,
    stop: Arc<AtomicBool>,
    hidden: AtomicBool,
}
impl Focus {
    pub(super) fn allows_picker(&self, thread: u32, lifetime: u64) -> bool {
        !self.stop.load(Ordering::SeqCst)
            && self.original.frames.app_id(0) == self.attempt
            && self
                .policy
                .lock()
                .is_ok_and(|policy| policy.allows_picker(thread, lifetime))
    }

    pub(super) fn take_hidden(&self, lifetime: u64) -> bool {
        self.original.frames.app_id(0) == self.attempt
            && self
                .policy
                .lock()
                .is_ok_and(|policy| policy.current(lifetime))
            && self.hidden.swap(false, Ordering::SeqCst)
    }

    fn external_loss(&self) {
        self.stop.store(true, Ordering::SeqCst);
        if self.original.hide_file_choice(&self.context) {
            self.hidden.store(true, Ordering::SeqCst);
        }
    }

    fn foreground(&self, owner_only: bool) -> bool {
        let Ok(policy) = self.policy.lock().map(|policy| *policy) else {
            return false;
        };
        // SAFETY: bounded scalar reads only; no messages are pumped, native
        // callbacks invoked or ownership changed. Root ownership includes a
        // common dialog's own popups without admitting another local window.
        // Sample after the policy lock wait; no lock may stale a positive
        // foreground observation before the final original-gesture check.
        let (thread, root) = unsafe {
            let foreground = GetForegroundWindow();
            (
                GetWindowThreadProcessId(foreground, None),
                GetAncestor(foreground, GA_ROOTOWNER).0 as usize,
            )
        };
        if owner_only {
            policy.owner_foreground(thread, root)
        } else {
            policy.foreground(thread, root)
        }
    }

    fn owner_foreground(&self) -> bool {
        self.foreground(true)
    }
}

struct WorkerFocus(Arc<Focus>);
impl Drop for WorkerFocus {
    fn drop(&mut self) {
        self.0
            .policy
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .finish_worker();
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

fn check_copied(choice: &mut Choice, focus: &Focus) -> Result<(), Error> {
    if focus.stop.load(Ordering::SeqCst)
        || focus.original.frames.app_id(0) != focus.attempt
        || !focus
            .original
            .file_choice_ready(&focus.context)
            .is_ok_and(|ready| ready)
        || !focus.owner_foreground()
    {
        return choice.refuse(Error::Cancelled);
    }
    // Read the original acceptance clock after all source/foreground locks.
    choice.check(ticks())
}

fn choose_copied(mut choice: Choice, focus: Arc<Focus>, sequence: u32) -> Result<Selected, Error> {
    check_copied(&mut choice, &focus)?;
    let bytes = copied_files::read(sequence, || {
        // No authority/source lock spans a clipboard borrow or delayed render.
        if focus.stop.load(Ordering::SeqCst) {
            return choice.refuse(Error::Cancelled);
        }
        choice.check(ticks())
    })?;
    check_copied(&mut choice, &focus)?;
    let paths: Vec<PathBuf> = copied_paths(&bytes)?
        .iter()
        .map(|path| PathBuf::from(OsString::from_wide(path)))
        .collect();
    check_copied(&mut choice, &focus)?;
    // The borrowed clipboard handle and pointer are already unlocked/closed.
    // Reuse the exact regular-file custody and batch validation owner; never
    // send a partial selection or reopen names after their handles are pinned.
    let prepared = Upload::prepare_many(&paths).map_err(|_| Error::Selection)?;
    check_copied(&mut choice, &focus)?;
    Ok(Selected {
        choice,
        prepared,
        focus,
    })
}

struct Native {
    choice: RefCell<Choice>,
    focus: Arc<Focus>,
    destroyed: Cell<bool>,
    initialized: Cell<bool>,
    parent_subclassed: Cell<bool>,
    parent_destroyed: Cell<bool>,
    parent_removed: Cell<bool>,
}
impl Native {
    fn check(&self) -> Result<(), Error> {
        if self.focus.stop.load(Ordering::SeqCst) {
            return self.choice.borrow_mut().refuse(Error::Cancelled);
        }
        self.choice.borrow_mut().check(ticks())
    }
}

fn choose(choice: Choice, focus: Arc<Focus>) -> Result<Selected, Error> {
    let native = Box::new(Native {
        choice: RefCell::new(choice),
        focus,
        destroyed: Cell::new(false),
        initialized: Cell::new(false),
        parent_subclassed: Cell::new(false),
        parent_destroyed: Cell::new(false),
        parent_removed: Cell::new(false),
    });
    native.check()?;
    let mut path = vec![0_u16; MAX_PATH_UNITS];
    let mut dialog = OPENFILENAMEW {
        lStructSize: u32::try_from(size_of::<OPENFILENAMEW>()).map_err(|_| Error::Native)?,
        lpstrFile: PWSTR(path.as_mut_ptr()),
        nMaxFile: u32::try_from(path.len()).map_err(|_| Error::Native)?,
        lpstrTitle: w!("Choose up to 8 files to send to the current remote host"),
        Flags: OFN_EXPLORER
            | OFN_ALLOWMULTISELECT
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
    if (native.initialized.get() && !native.destroyed.get())
        || (native.parent_subclassed.get() && !native.parent_destroyed.get())
    {
        native.focus.stop.store(true, Ordering::SeqCst);
        // A native callback may still hold this pointer. Preserve its original
        // allocation on failed cleanup; never turn uncertainty into a dangling
        // hook/subclass reference or report a safe chooser replacement.
        let _ = Box::leak(native);
        return Err(Error::Cleanup);
    }
    if native.parent_subclassed.get() && !native.parent_removed.get() {
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
    let paths: Vec<PathBuf> = selected_paths(&path, usize::from(dialog.nFileOffset))?
        .iter()
        .map(|path| PathBuf::from(OsString::from_wide(path)))
        .collect();
    // The existing Windows custody boundary denies reparse leaves, alternate
    // streams and conflicting writers; it duplicates this actual disk handle.
    let prepared = Upload::prepare_many(&paths).map_err(|_| Error::Selection)?;
    native.check()?;
    Ok(Selected {
        choice: native.choice.into_inner(),
        prepared,
        focus: native.focus,
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
            if !install_parent(hwnd, native) {
                let _ = native.choice.borrow_mut().refuse(Error::Native);
                close_dialog(hwnd);
            }
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
            // APP activation notifications can precede the foreground switch.
            // Only the exact owner thread may use that short handoff; once the
            // native loop runs, verify the actual original HWND/root as well.
            if !native.focus.foreground(false) {
                native.focus.external_loss();
            }
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

fn install_parent(child: HWND, native: &Native) -> bool {
    // SAFETY: Explorer-style hooks live in a child; only its documented parent
    // receives application activation. Subclass on the creating worker thread,
    // never across threads, and keep the exact Native allocation until removal.
    unsafe {
        let Ok(parent) = GetParent(child) else {
            return false;
        };
        let thread = GetWindowThreadProcessId(parent, None);
        if thread != GetCurrentThreadId()
            || !native
                .focus
                .policy
                .lock()
                .is_ok_and(|mut policy| policy.register_dialog(thread, parent.0 as usize))
        {
            return false;
        }
        let installed = SetWindowSubclass(
            parent,
            Some(parent_proc),
            SUBCLASS,
            std::ptr::from_ref(native) as usize,
        )
        .as_bool();
        native.parent_subclassed.set(installed);
        installed
    }
}

extern "system" fn parent_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _id: usize,
    reference: usize,
) -> LRESULT {
    // SAFETY: only install_parent supplies this original allocation, on the
    // owning worker thread; failed cleanup retains it instead of freeing it.
    let native = unsafe { &*(reference as *const Native) };
    if message == WM_ACTIVATEAPP && wparam.0 == 0 {
        let thread = u32::try_from(lparam.0).unwrap_or(0);
        let owner_return = native
            .focus
            .policy
            .lock()
            .is_ok_and(|policy| policy.allows_owner(thread));
        // The viewer can cease receiving APP notifications while the separate
        // picker thread is foreground. Fence external loss here at the actual
        // top-level dialog; null/unknown/reused threads are never exempted.
        if !owner_return {
            native.focus.external_loss();
            close_parent(hwnd);
        }
    }
    if message == WM_NCDESTROY {
        // SAFETY: removal is from the same original parent on its own thread;
        // no reference-data lifetime is entrusted to automatic subclass cleanup.
        native
            .parent_removed
            .set(unsafe { RemoveWindowSubclass(hwnd, Some(parent_proc), SUBCLASS).as_bool() });
        // SAFETY: continue this same native subclass chain once, including its
        // final destruction. No RefCell or route mutex spans this dispatch.
        let result = unsafe { DefSubclassProc(hwnd, message, wparam, lparam) };
        native.parent_destroyed.set(true);
        return result;
    }
    // SAFETY: ordinary continuation of this parent window's subclass chain.
    unsafe { DefSubclassProc(hwnd, message, wparam, lparam) }
}

fn close_dialog(child: HWND) {
    // SAFETY: the hook child and parent belong to this current modal operation.
    // Posting cancellation is nonblocking and retains native ownership until
    // the original GetOpenFileNameW call and worker have actually completed.
    unsafe {
        if let Ok(parent) = GetParent(child) {
            close_parent(parent);
        }
    }
}

fn close_parent(parent: HWND) {
    // SAFETY: this current original dialog's worker owns the target. Posting
    // never pumps messages or releases the pinned hook/subclass allocation.
    unsafe {
        let _ = PostMessageW(
            Some(parent),
            WM_COMMAND,
            WPARAM(usize::try_from(IDCANCEL.0).unwrap_or(2)),
            LPARAM(0),
        );
    }
}
