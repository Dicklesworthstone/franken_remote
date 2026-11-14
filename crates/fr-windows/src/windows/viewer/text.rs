//! One modeless native Unicode editor. The standard EDIT control owns IME
//! preedit locally; only its explicit Send hands one complete value to Rust.
use super::{Viewer, current_message_time, input_ticks, message_age, with_viewer};
use crate::text::{Draft, Lifetime, MAX_UTF16_UNITS, from_utf16};
use fr_mobile_ffi::session::{InputResult, Shared, TextDraft};
use std::{
    cell::RefCell,
    ffi::c_void,
    sync::{Arc, Weak},
};
use windows::{
    Win32::{
        Foundation::{HWND, LPARAM, LRESULT, WPARAM},
        Graphics::Gdi::{COLOR_WINDOW, GetSysColorBrush},
        System::LibraryLoader::GetModuleHandleW,
        UI::{
            Controls::{EM_EMPTYUNDOBUFFER, EM_SETLIMITTEXT},
            HiDpi::GetDpiForWindow,
            Input::{
                Ime::{
                    CPS_CANCEL, GCS_COMPSTR, ImmGetCompositionStringW, ImmGetContext, ImmNotifyIME,
                    ImmReleaseContext, NI_COMPOSITIONSTR,
                },
                KeyboardAndMouse::{EnableWindow, SetFocus},
            },
            WindowsAndMessaging::{
                AppendMenuW, BN_CLICKED, CreateMenu, CreatePopupMenu, CreateWindowExW,
                DefWindowProcW, DestroyMenu, DestroyWindow, EN_CHANGE, EN_ERRSPACE, EN_MAXTEXT,
                ES_AUTOVSCROLL, ES_MULTILINE, ES_WANTRETURN, GetWindowRect, GetWindowTextLengthW,
                GetWindowTextW, HMENU, IDC_ARROW, LoadCursorW, MF_POPUP, MF_STRING,
                RegisterClassExW, SW_SHOW, SendMessageW, SetMenu, SetWindowTextW, ShowWindow,
                WINDOW_EX_STYLE, WINDOW_STYLE, WM_ACTIVATEAPP, WM_CLOSE, WM_COMMAND, WNDCLASSEXW,
                WS_BORDER, WS_CAPTION, WS_CHILD, WS_POPUP, WS_SYSMENU, WS_TABSTOP, WS_VISIBLE,
                WS_VSCROLL,
            },
        },
    },
    core::{HSTRING, Result, w},
};

pub(super) const OPEN: usize = 0x4650;
const EDIT: usize = 0x4651;
const SEND: usize = 0x4652;
const CANCEL: usize = 0x4653;

struct Lifecycle {
    original: Weak<Shared>,
    draft: Lifetime,
}

thread_local! {
    /// Native APIs can reenter callbacks while Viewer is mutably borrowed.
    /// The independent lifetime cancels Send first; native cleanup is separate.
    static ACTIVE: RefCell<Option<Weak<Lifecycle>>> = const { RefCell::new(None) };
}

struct ActivationFence {
    state: Arc<Lifecycle>,
}
impl ActivationFence {
    fn new(original: &Arc<Shared>) -> Self {
        let state = Arc::new(Lifecycle {
            original: Arc::downgrade(original),
            draft: Lifetime::default(),
        });
        ACTIVE.with(|slot| *slot.borrow_mut() = Some(Arc::downgrade(&state)));
        Self { state }
    }
}
impl Drop for ActivationFence {
    fn drop(&mut self) {
        ACTIVE.with(|slot| {
            let mut slot = slot.borrow_mut();
            if slot
                .as_ref()
                .is_some_and(|state| state.ptr_eq(&Arc::downgrade(&self.state)))
            {
                *slot = None;
            }
        });
    }
}

fn active() -> Option<Arc<Lifecycle>> {
    ACTIVE.with(|slot| slot.borrow().as_ref().and_then(Weak::upgrade))
}

fn invalidate() {
    if let Some(state) = active() {
        state.draft.invalidate();
    }
}

fn dismiss() {
    if let Some(state) = active() {
        state.draft.dismiss();
    }
}

fn changed() {
    if let Some(state) = active() {
        state.draft.changed();
    }
}

/// Call before borrowing Viewer, including from nested OS callbacks.
pub(super) fn deactivate() {
    if let Some(state) = active() {
        state.draft.dismiss();
        if let Some(original) = state.original.upgrade()
            && original.controlled()
        {
            original.set_visible(false);
        }
    }
}

/// Parent closure fences the original owner even if its window is borrowed.
pub(super) fn close_owner() {
    if let Some(state) = active() {
        state.draft.dismiss();
        if let Some(original) = state.original.upgrade() {
            original.close();
        }
    }
}

/// Ownership of the menu transfers to hwnd only after `SetMenu` succeeds.
pub(super) fn install_menu(hwnd: HWND) -> Result<()> {
    // SAFETY: all handles are created here on the UI thread; failed attachment
    // destroys only this newly owned menu tree, never another window's menu.
    unsafe {
        let menu = CreateMenu()?;
        let popup = match CreatePopupMenu() {
            Ok(popup) => popup,
            Err(error) => {
                let _ = DestroyMenu(menu);
                return Err(error);
            }
        };
        let attached = AppendMenuW(popup, MF_STRING, OPEN, w!("Send &text…"))
            .and_then(|()| AppendMenuW(menu, MF_POPUP, popup.0 as usize, w!("&Input")));
        if let Err(error) = attached {
            let _ = DestroyMenu(popup);
            let _ = DestroyMenu(menu);
            return Err(error);
        }
        if let Err(error) = SetMenu(hwnd, Some(menu)) {
            let _ = DestroyMenu(menu);
            return Err(error);
        }
        Ok(())
    }
}

pub(super) struct Editor {
    hwnd: HWND,
    edit: HWND,
    send: HWND,
    cancel: HWND,
    notice: HWND,
    original: Arc<Shared>,
    activation: ActivationFence,
    draft: Option<Draft<TextDraft>>,
    ready: bool,
    opened: u64,
    message: &'static str,
}

impl Editor {
    #[allow(clippy::too_many_lines, reason = "one bounded native editor construction")]
    pub(super) fn open(
        owner: HWND,
        original: Arc<Shared>,
        context: Option<TextDraft>,
        notice: &'static str,
    ) -> Result<Self> {
        let activation = ActivationFence::new(&original);
        // SAFETY: the owner and every control belong to this UI thread. Child
        // IDs are nonzero constants, and all text pointers are call-scoped.
        unsafe {
            let instance = GetModuleHandleW(None)?;
            let class = w!("FrankenRemoteTextEditor");
            let wc = WNDCLASSEXW {
                cbSize: u32::try_from(size_of::<WNDCLASSEXW>()).unwrap_or(0),
                lpfnWndProc: Some(window_proc),
                hInstance: instance.into(),
                hCursor: LoadCursorW(None, IDC_ARROW)?,
                hbrBackground: GetSysColorBrush(COLOR_WINDOW),
                lpszClassName: class,
                ..Default::default()
            };
            RegisterClassExW(&raw const wc);
            let dpi = GetDpiForWindow(owner).clamp(96, 384);
            let scale = |px: i32| px * i32::try_from(dpi).unwrap_or(96) / 96;
            let mut bounds = windows::Win32::Foundation::RECT::default();
            GetWindowRect(owner, &raw mut bounds)?;
            let hwnd = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                class,
                w!("Send text — FrankenRemote"),
                WS_POPUP | WS_CAPTION | WS_SYSMENU,
                bounds.left + scale(24),
                bounds.top + scale(48),
                scale(580),
                scale(340),
                Some(owner),
                None,
                Some(instance.into()),
                None,
            )?;
            // A partially constructed editor still owns its parent. Drop
            // clears/destroys all child controls on any subsequent error.
            let mut editor = Self {
                hwnd,
                edit: HWND::default(),
                send: HWND::default(),
                cancel: HWND::default(),
                notice: HWND::default(),
                original,
                activation,
                draft: context.map(Draft::new),
                ready: false,
                opened: 0,
                message: "",
            };
            let control = |class, text, style, id, (x, y, width, height)| {
                CreateWindowExW(
                    WINDOW_EX_STYLE(0),
                    class,
                    text,
                    WS_CHILD | WS_VISIBLE | style,
                    scale(x),
                    scale(y),
                    scale(width),
                    scale(height),
                    Some(hwnd),
                    Some(HMENU(id as *mut c_void)),
                    Some(instance.into()),
                    None,
                )
            };
            let _ = control(
                w!("STATIC"),
                w!("Compose locally, including IME. Limit: 4,096 UTF-8 bytes."),
                WINDOW_STYLE(0),
                0,
                (12, 12, 548, 24),
            )?;
            editor.edit = control(
                w!("EDIT"),
                w!(""),
                WS_TABSTOP
                    | WS_BORDER
                    | WS_VSCROLL
                    | WINDOW_STYLE(
                        u32::try_from(ES_MULTILINE | ES_AUTOVSCROLL | ES_WANTRETURN).unwrap_or(0),
                    ),
                EDIT,
                (12, 40, 548, 170),
            )?;
            let _ = SendMessageW(
                editor.edit,
                EM_SETLIMITTEXT,
                Some(WPARAM(MAX_UTF16_UNITS)),
                None,
            );
            editor.notice = control(
                w!("STATIC"),
                w!(""),
                WINDOW_STYLE(0),
                0,
                (12, 220, 548, 34),
            )?;
            editor.send = control(
                w!("BUTTON"),
                w!("&Send once"),
                WS_TABSTOP,
                SEND,
                (310, 262, 120, 28),
            )?;
            editor.cancel = control(
                w!("BUTTON"),
                if editor.draft.is_some() {
                    w!("&Cancel")
                } else {
                    w!("&Close")
                },
                WS_TABSTOP,
                CANCEL,
                (440, 262, 120, 28),
            )?;
            let _ = EnableWindow(editor.send, false);
            let _ = EnableWindow(editor.edit, editor.draft.is_some());
            editor.message(notice);
            let _ = ShowWindow(hwnd, SW_SHOW);
            let _ = SetFocus(Some(editor.edit));
            editor.opened = input_ticks();
            Ok(editor)
        }
    }

    pub(super) const fn window(&self) -> HWND {
        self.hwnd
    }

    pub(super) fn dismissed(&self) -> bool {
        self.activation.state.draft.dismissed()
    }

    pub(super) fn refresh(&mut self) {
        let Some(context) = self.draft.as_ref().and_then(Draft::context) else {
            return;
        };
        if !self.activation.state.draft.valid() {
            self.invalidate("Local text was cancelled or its original view changed.");
            return;
        }
        match self.original.text_ready(context) {
            Ok(ready) => {
                if self.ready != ready {
                    self.ready = ready;
                    // SAFETY: this editor owns the live Send button.
                    let _ = unsafe { EnableWindow(self.send, ready) };
                    self.message(if ready {
                        "Ready. Send queues the whole value once; insertion is not yet confirmed."
                    } else {
                        "Waiting for the original remote view; text stays local."
                    });
                }
            }
            Err(_) => {
                self.invalidate("The original remote view changed. Local text was discarded.");
            }
        }
    }

    pub(super) fn send(&mut self, message_time: u32, ordered: bool) {
        // Consume BEFORE native calls. A duplicate command, stale gesture,
        // failed read, context change or admission refusal never retries it.
        let revision = self.activation.state.draft.revision();
        let draft = self.draft.as_mut().and_then(Draft::take);
        let captured = draft
            .filter(|_| self.activation.state.draft.valid())
            .and_then(|draft| self.original.capture_text(draft).ok());
        let captured = captured.and_then(|capture| {
            message_age(message_time)
                .ok()
                .filter(|(at, _)| *at > self.opened)
                .and_then(|(_, age)| capture.with_queue_age(age))
        });
        let result = if ordered {
            captured.and_then(|capture| {
                let text = self.read_committed().ok()?;
                // Native uptime also charges suspend or stalls during the read.
                // It cannot replace the already frozen source-clock deadline.
                let result = if self.activation.state.draft.unchanged(revision)
                    && message_age(message_time).is_ok()
                {
                    Some(self.original.captured_text(capture, &text))
                } else {
                    None
                };
                text.into_bytes().fill(0);
                result
            })
        } else {
            None
        };
        self.invalidate(match result {
            Some(InputResult::Sent) => {
                "Queued once for the host. Insertion is not confirmed; no automatic retry."
            }
            Some(InputResult::Unsupported) => {
                "The host does not support direct text. Nothing was sent."
            }
            _ => "Text was refused and discarded. Reopen Send text for a new explicit choice.",
        });
    }

    fn read_committed(&self) -> std::result::Result<String, ()> {
        // SAFETY: querying only our thread's own EDIT control. The temporary
        // IME context is released on every path, before reading its text.
        unsafe {
            let ime = ImmGetContext(self.edit);
            if !ime.0.is_null() {
                let composing = ImmGetCompositionStringW(ime, GCS_COMPSTR, None, 0);
                let released = ImmReleaseContext(self.edit, ime).as_bool();
                if !released || composing != 0 {
                    return Err(());
                }
            }
            let count = usize::try_from(GetWindowTextLengthW(self.edit)).map_err(|_| ())?;
            if count == 0 || count > MAX_UTF16_UNITS {
                return Err(());
            }
            // Read into the full bounded capacity, never the earlier length:
            // growth between native calls must not become a valid prefix.
            let mut units = vec![0_u16; MAX_UTF16_UNITS + 1];
            let copied = usize::try_from(GetWindowTextW(self.edit, &mut units)).ok();
            let after = usize::try_from(GetWindowTextLengthW(self.edit)).ok();
            let text = if copied == Some(count) && after == Some(count) && units[count] == 0 {
                from_utf16(&units[..count]).map_err(|_| ())
            } else {
                Err(())
            };
            units.fill(0);
            text
        }
    }

    pub(super) fn invalidate(&mut self, notice: &'static str) {
        if let Some(draft) = &mut self.draft {
            draft.invalidate();
        }
        self.ready = false;
        self.clear();
        // SAFETY: these child handles remain owned until this editor drops.
        unsafe {
            let _ = EnableWindow(self.edit, false);
            let _ = EnableWindow(self.send, false);
            let _ = SetWindowTextW(self.cancel, w!("&Close"));
        }
        self.message(notice);
    }

    fn message(&mut self, notice: &'static str) {
        if self.message != notice {
            self.message = notice;
            // SAFETY: the STATIC control owns its copied, content-free label.
            let _ = unsafe { SetWindowTextW(self.notice, &HSTRING::from(notice)) };
        }
    }

    fn clear(&self) {
        if !self.edit.0.is_null() {
            // SAFETY: only this editor's IME state, text and native undo buffer
            // are cleared. Every acquired context is released. No clipboard or
            // host field is written, and input has already been fenced on loss.
            unsafe {
                let ime = ImmGetContext(self.edit);
                if !ime.0.is_null() {
                    let _ = ImmNotifyIME(ime, NI_COMPOSITIONSTR, CPS_CANCEL, 0);
                    let _ = ImmReleaseContext(self.edit, ime);
                }
                let _ = SetWindowTextW(self.edit, w!(""));
                let _ = SendMessageW(self.edit, EM_EMPTYUNDOBUFFER, None, None);
            }
        }
    }
}

impl Drop for Editor {
    fn drop(&mut self) {
        self.clear();
        // SAFETY: this top-level owned window and all its children belong only
        // to this editor; its procedure never calls PostQuitMessage.
        let _ = unsafe { DestroyWindow(self.hwnd) };
    }
}

extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        WM_COMMAND => {
            let id = wparam.0 & 0xffff;
            let notice = u32::try_from((wparam.0 >> 16) & 0xffff).unwrap_or(0);
            match id {
                SEND if notice == BN_CLICKED => {
                    let at = current_message_time();
                    with_viewer(|v| v.send_text(at));
                }
                // IsDialogMessage sends IDCANCEL (2) for the Escape gesture.
                CANCEL | 2 if notice == BN_CLICKED => {
                    dismiss();
                    with_viewer(|v| v.text = None);
                }
                EDIT if notice == EN_CHANGE => changed(),
                EDIT if matches!(notice, EN_MAXTEXT | EN_ERRSPACE) => {
                    invalidate();
                    with_viewer(|v| {
                        if let Some(editor) = &mut v.text {
                            editor.invalidate(
                                "The native text limit was exceeded. The draft was discarded.",
                            );
                        }
                    });
                }
                _ => {}
            }
            LRESULT(0)
        }
        WM_CLOSE => {
            dismiss();
            with_viewer(|v| v.text = None);
            LRESULT(0)
        }
        WM_ACTIVATEAPP if wparam.0 == 0 => {
            deactivate();
            with_viewer(Viewer::text_deactivated);
            LRESULT(0)
        }
        _ => {
            // SAFETY: default processing for this thread's native editor.
            unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
        }
    }
}
