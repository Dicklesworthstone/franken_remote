//! Visible input ownership and explicit local control choices. The menu's
//! original session is retained independently of the borrowed renderer so
//! Stop fences it immediately even during a nested native callback.
use super::{input_ticks, text};
use crate::{change_display, control::Choice};
use fr_mobile_ffi::session::{ControlRequest, ControlState, Shared};
use std::{
    cell::{Cell, RefCell},
    sync::{Arc, Weak},
    time::{Duration, Instant},
};

/// A new control state reaches the title and menu bar only after holding
/// this long. The ready-view token and stale-view suspensions can flip the
/// state several times a second, which made the title flash.
const SETTLE: Duration = Duration::from_millis(750);
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
    static STOP_CHOICE: RefCell<Option<change_display::MenuChoice<Shared>>> =
        const { RefCell::new(None) };
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
    STOP_CHOICE.with(|choice| *choice.borrow_mut() = None);
}

/// Window lifecycle checks need no mutable access to Viewer or any codec.
pub(super) fn visible(visible: bool) {
    let was = VISIBLE.with(|current| current.replace(visible));
    if !visible && was {
        invalidate_display();
    }
    ORIGINAL.with(|slot| {
        if let Some(original) = slot.borrow().as_ref().and_then(Weak::upgrade) {
            original.set_visible(visible);
        }
    });
}

/// The app switched away (`false`) or back (`true`) while the window stays
/// on screen. Old menu choices die with the switch; a granted owner releases
/// its held input and pauses instead of ending (`Shared::set_active`).
/// `false`: a release was refused, so the caller must end control instead.
#[must_use]
pub(super) fn active(active: bool) -> bool {
    if !active {
        invalidate_display();
    }
    ORIGINAL.with(|slot| {
        slot.borrow()
            .as_ref()
            .and_then(Weak::upgrade)
            .is_none_or(|original| original.set_active(active))
    })
}

/// Reflect a worker's already-scoped negative fence in this UI only. Repeating
/// `Shared::set_visible` here could apply an old notification to a later attempt.
pub(super) fn reflect_hidden() {
    if VISIBLE.with(|current| current.replace(false)) {
        invalidate_display();
    }
}

/// Fence the exact active grant before any native cleanup or Viewer borrow.
/// An already-closing/failed owner cannot turn another notification into a
/// replacement request. The request names the NEW hidden native lifetime.
pub(super) fn focus_lost() -> Option<change_display::Request<Shared>> {
    if !visible_now() {
        return None;
    }
    let original = ORIGINAL.with(|slot| slot.borrow().as_ref().and_then(Weak::upgrade))?;
    let attempt = ready_attempt(&original)?;
    let at = input_ticks();
    if !original.close_for_focus_loss(attempt) {
        return None;
    }
    reflect_hidden();
    Some(change_display::Request {
        original,
        lifetime: lifetime(),
        attempt,
        at,
    })
}

/// Identity for a deferred native activation. A replacement or internal
/// reconnect cannot consume a visibility notification from the old owner.
pub(super) fn activation_scope() -> Option<(u64, u64)> {
    let original = ORIGINAL.with(|slot| slot.borrow().as_ref().and_then(Weak::upgrade))?;
    Some((lifetime(), original.frames.app_id(0)))
}

pub(super) fn invalidate_choice() {
    invalidate_display();
    ORIGINAL.with(|slot| {
        if let Some(original) = slot.borrow().as_ref().and_then(Weak::upgrade) {
            original.retire_control_request();
        }
    });
}

/// Fence only the popup's exact original input owner before any renderer or
/// editor cleanup. A later native callback cannot close a replacement or start
/// another dial; fresh viewing still requires this owner's positive cleanup.
pub(super) fn stop(message: u32) -> Option<change_display::Request<Shared>> {
    let mut choice = STOP_CHOICE.with(|choice| choice.borrow_mut().take())?;
    let original = ORIGINAL.with(|slot| slot.borrow().as_ref().and_then(Weak::upgrade))?;
    let available = visible_now()
        && display_ready(&original)
        && original.control_state() == ControlState::Controlled;
    let request = choice.take(
        &original,
        lifetime(),
        original.frames.app_id(0),
        available,
        message,
        input_ticks(),
    )?;
    DISPLAY_CHOICE.with(|choice| *choice.borrow_mut() = None);
    request.original.release_control();
    Some(request)
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
    /// A different state seen since `displayed`, and when it was first seen.
    pending: Option<(ControlState, Instant)>,
    choice: Option<Choice<ControlRequest>>,
    display_available: bool,
    dormant: bool,
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
                    AppendMenuW(popup, MF_STRING, STOP, w!("&Stop control and keep viewing"))
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
            pending: None,
            choice: None,
            display_available: false,
            dormant: false,
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
        // The opening menu shows the current state's items at once.
        self.update(original.control_state(), true);
        let choice = original.control_request();
        let opened = input_ticks();
        self.choice = choice.map(|request| Choice::new(request, opened));
        let stop_choice = attempt
            .filter(|attempt| {
                original.control_state() == ControlState::Controlled
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
        STOP_CHOICE.with(|slot| *slot.borrow_mut() = stop_choice);
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
        self.update(original.control_state(), false);
    }

    /// Show `state` once it has held for [`SETTLE`]. The first state, an
    /// opening menu (`force`) and the end of a session show at once.
    fn update(&mut self, state: ControlState, force: bool) {
        if self.displayed == Some(state) {
            self.pending = None;
            return;
        }
        let now = Instant::now();
        let settled = match self.pending {
            Some((seen, since)) if seen == state => now.duration_since(since) >= SETTLE,
            _ => {
                self.pending = Some((state, now));
                false
            }
        };
        let immediate = force
            || self.displayed.is_none()
            || matches!(state, ControlState::Closing | ControlState::Ended);
        if !settled && !immediate {
            return;
        }
        self.pending = None;
        self.displayed = Some(state);
        self.dormant = false;
        let enabled = |available| if available { MF_ENABLED } else { MF_GRAYED };
        let description = match state {
            ControlState::Unavailable if super::super::application_session() => {
                "View only (choose View and control in FrankenRemote to request input)"
            }
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
                MF_BYCOMMAND | enabled(state == ControlState::Controlled),
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

    pub(super) fn waiting_for_foreground(&mut self) {
        if self.dormant {
            return;
        }
        self.dormant = true;
        // SAFETY: this UI thread's live window. The original session has
        // already completed cleanup; this text grants no new observation.
        unsafe {
            let _ = SetWindowTextW(
                self.hwnd,
                &HSTRING::from(format!(
                    "{} — Control stopped; return to this window to keep viewing",
                    self.title
                )),
            );
        }
    }
}
