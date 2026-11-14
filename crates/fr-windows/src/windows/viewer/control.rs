//! Visible input ownership and explicit local control choices. The menu's
//! original session is retained independently of the borrowed renderer so
//! Stop fences it immediately even during a nested native callback.
use super::{input_ticks, text};
use crate::{change_display, control::Choice};
use fr_mobile_ffi::session::{ControlRequest, ControlState, Shared};
use std::{
    cell::{Cell, RefCell},
    sync::{Arc, Weak},
};
use windows::{
    Win32::{
        Foundation::HWND,
        UI::WindowsAndMessaging::{
            AppendMenuW, CreateMenu, CreatePopupMenu, DestroyMenu, DrawMenuBar, EnableMenuItem,
            HMENU, MF_BYCOMMAND, MF_ENABLED, MF_GRAYED, MF_POPUP, MF_SEPARATOR, MF_STRING, SetMenu,
            SetWindowTextW,
        },
    },
    core::{HSTRING, Result, w},
};

pub(super) const REQUEST: usize = 0x4670;
pub(super) const STOP: usize = 0x4671;
pub(super) const CHANGE_DISPLAY: usize = 0x4672;

/// Menu command ids are `WM_COMMAND`'s 16-bit low word; `EnableMenuItem` takes
/// them as `u32`.
const fn item(id: usize) -> u32 {
    assert!(id <= 0xffff);
    #[allow(clippy::cast_possible_truncation)] // bounded just above
    {
        id as u32
    }
}

thread_local! {
    static ORIGINAL: RefCell<Option<Weak<Shared>>> = const { RefCell::new(None) };
    static STOP_ORIGINAL: RefCell<Option<Weak<Shared>>> = const { RefCell::new(None) };
    static DISPLAY_CHOICE: RefCell<Option<change_display::MenuChoice<Shared>>> =
        const { RefCell::new(None) };
    static LIFETIME: Cell<u64> = const { Cell::new(0) };
    static VISIBLE: Cell<bool> = const { Cell::new(false) };
}

pub(super) fn bind(original: &Arc<Shared>) {
    invalidate_display();
    ORIGINAL.with(|slot| *slot.borrow_mut() = Some(Arc::downgrade(original)));
}

pub(super) fn lifetime() -> u64 {
    LIFETIME.with(Cell::get)
}

pub(super) fn visible_now() -> bool {
    VISIBLE.with(Cell::get) && lifetime() != u64::MAX
}

/// A retained picture from a prior internal connection attempt cannot offer
/// a new display action. View-only sessions have no control-readiness token.
pub(super) fn display_ready(original: &Shared) -> bool {
    ready_attempt(original).is_some()
}

fn ready_attempt(original: &Shared) -> Option<u64> {
    let attempt = original.frames.app_id(0);
    (original.frames.drawn() > attempt && original.frames.app_id(0) == attempt).then_some(attempt)
}

pub(super) fn invalidate_display() {
    LIFETIME.with(|lifetime| lifetime.set(lifetime.get().saturating_add(1)));
    DISPLAY_CHOICE.with(|choice| *choice.borrow_mut() = None);
}

/// Window lifecycle checks need no mutable access to Viewer or any codec.
pub(super) fn visible(visible: bool) {
    VISIBLE.with(|current| current.set(visible));
    if !visible {
        invalidate_display();
    }
    ORIGINAL.with(|slot| {
        if let Some(original) = slot.borrow().as_ref().and_then(Weak::upgrade) {
            original.set_visible(visible);
        }
    });
}

pub(super) fn invalidate_choice() {
    invalidate_display();
    ORIGINAL.with(|slot| {
        if let Some(original) = slot.borrow().as_ref().and_then(Weak::upgrade) {
            original.retire_control_request();
        }
    });
}

/// Stop only the owner from the popup that offered it. A delayed callback
/// cannot close a replacement session or create a control request.
pub(super) fn stop() -> bool {
    invalidate_display();
    let Some(original) =
        STOP_ORIGINAL.with(|slot| slot.borrow_mut().take().and_then(|owner| owner.upgrade()))
    else {
        return false;
    };
    original.release_control();
    ORIGINAL.with(|slot| {
        slot.borrow()
            .as_ref()
            .is_some_and(|current| current.ptr_eq(&Arc::downgrade(&original)))
    })
}

pub(super) fn close() {
    invalidate_display();
    ORIGINAL.with(|slot| {
        if let Some(original) = slot.borrow().as_ref().and_then(Weak::upgrade) {
            original.close();
        }
    });
}

/// Fence the popup's original owner before any renderer/editor borrow. A
/// reentrant callback may defer UI cleanup, but never input revocation.
pub(super) fn change_display(message: u32) -> Option<change_display::Request<Shared>> {
    let mut choice = DISPLAY_CHOICE.with(|choice| choice.borrow_mut().take())?;
    let original = ORIGINAL.with(|slot| slot.borrow().as_ref().and_then(Weak::upgrade))?;
    let available = visible_now()
        && display_ready(&original)
        && matches!(
            original.control_state(),
            ControlState::Unavailable | ControlState::Viewing | ControlState::Controlled
        );
    let request = choice.take(
        &original,
        lifetime(),
        original.frames.app_id(0),
        available,
        message,
        input_ticks(),
    )?;
    request.original.close();
    Some(request)
}

pub(super) struct Menu {
    hwnd: HWND,
    popup: HMENU,
    title: String,
    displayed: Option<ControlState>,
    choice: Option<Choice<ControlRequest>>,
    display_available: bool,
}

impl Menu {
    /// Ownership of the menu transfers to `hwnd` only after `SetMenu` succeeds.
    pub(super) fn install(hwnd: HWND, title: &str) -> Result<Self> {
        // SAFETY: all handles are created here on this UI thread. Failure
        // destroys only the newly owned menu tree; the window owns it after
        // SetMenu, and remains alive longer than this borrowed popup handle.
        let popup = unsafe {
            let menu = CreateMenu()?;
            let popup = match CreatePopupMenu() {
                Ok(popup) => popup,
                Err(error) => {
                    let _ = DestroyMenu(menu);
                    return Err(error);
                }
            };
            let attached = AppendMenuW(popup, MF_STRING, REQUEST, w!("&Request control"))
                .and_then(|()| {
                    AppendMenuW(popup, MF_STRING, STOP, w!("&Stop control and disconnect"))
                })
                .and_then(|()| AppendMenuW(popup, MF_SEPARATOR, 0, w!("")))
                .and_then(|()| AppendMenuW(popup, MF_STRING, text::OPEN, w!("Send &text…")))
                .and_then(|()| AppendMenuW(popup, MF_SEPARATOR, 0, w!("")))
                .and_then(|()| {
                    AppendMenuW(
                        popup,
                        MF_STRING | MF_GRAYED,
                        CHANGE_DISPLAY,
                        w!("Change &display…"),
                    )
                })
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
            popup
        };
        Ok(Self {
            hwnd,
            popup,
            title: title.to_owned(),
            displayed: None,
            choice: None,
            display_available: false,
        })
    }

    pub(super) fn is_popup(&self, handle: usize) -> bool {
        self.popup.0 as usize == handle
    }

    pub(super) fn opened(&mut self, original: &Arc<Shared>) {
        // Capture before native refresh, which can reenter a window lifecycle
        // callback. Neither that callback nor a reconnect renews this popup.
        let attempt = ready_attempt(original);
        let native_lifetime = lifetime();
        self.refresh(original);
        let choice = original.control_request();
        let opened = input_ticks();
        self.choice = choice.map(|request| Choice::new(request, opened));
        STOP_ORIGINAL.with(|slot| *slot.borrow_mut() = Some(Arc::downgrade(original)));
        let display_choice = attempt
            .filter(|attempt| {
                self.display_available
                    && visible_now()
                    && lifetime() == native_lifetime
                    && ready_attempt(original) == Some(*attempt)
            })
            .map(|attempt| {
                change_display::MenuChoice::new(
                    Arc::clone(original),
                    native_lifetime,
                    attempt,
                    opened,
                )
            });
        DISPLAY_CHOICE.with(|slot| *slot.borrow_mut() = display_choice);
    }

    pub(super) fn display_available(&mut self, available: bool) {
        if self.display_available == available {
            return;
        }
        self.display_available = available;
        if !available {
            DISPLAY_CHOICE.with(|choice| *choice.borrow_mut() = None);
        }
        // SAFETY: this thread's live menu and window. The real action consumes
        // its original owner and native lifetime; enabling is descriptive only.
        unsafe {
            let _ = EnableMenuItem(
                self.popup,
                item(CHANGE_DISPLAY),
                MF_BYCOMMAND | if available { MF_ENABLED } else { MF_GRAYED },
            );
            let _ = DrawMenuBar(self.hwnd);
        }
    }

    pub(super) fn request(&mut self, original: &Shared, message: u32) -> bool {
        invalidate_display();
        let Some(mut choice) = self.choice.take() else {
            return false;
        };
        let Some((request, age)) = choice.take(message, input_ticks()) else {
            return false;
        };
        original.request_control_for(&request, age)
    }

    pub(super) fn refresh(&mut self, original: &Shared) {
        let state = original.control_state();
        if self.displayed == Some(state) {
            return;
        }
        self.displayed = Some(state);
        let enabled = |available| if available { MF_ENABLED } else { MF_GRAYED };
        let description = match state {
            ControlState::Unavailable => "View only (start with --control to request input)",
            ControlState::WaitingForView => "Waiting for a ready view",
            ControlState::Viewing => "Viewing — Input → Request control",
            ControlState::Requesting => "Control requested — waiting for host grant",
            ControlState::Controlled => "Controlling — Input → Stop control",
            ControlState::Closing => "Stopping — waiting for cleanup",
            ControlState::Ended => "Session ended",
        };
        // SAFETY: this thread's live menu and window. Text is copied by Win32.
        // UI state is only descriptive; request admission rechecks its original
        // token and control remains exclusively with the shared session owner.
        unsafe {
            let _ = EnableMenuItem(
                self.popup,
                item(REQUEST),
                MF_BYCOMMAND | enabled(state == ControlState::Viewing),
            );
            let _ = EnableMenuItem(
                self.popup,
                item(STOP),
                MF_BYCOMMAND
                    | enabled(matches!(
                        state,
                        ControlState::Requesting | ControlState::Controlled
                    )),
            );
            let _ = EnableMenuItem(
                self.popup,
                item(text::OPEN),
                MF_BYCOMMAND | enabled(state == ControlState::Controlled),
            );
            let _ = SetWindowTextW(
                self.hwnd,
                &HSTRING::from(format!("{} — {description}", self.title)),
            );
            let _ = DrawMenuBar(self.hwnd);
        }
    }
}
