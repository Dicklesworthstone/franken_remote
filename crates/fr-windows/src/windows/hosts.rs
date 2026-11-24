//! The native machine picker: one window over installed-Tailscale snapshots.
//!
//! Each snapshot becomes one [`Choice`]: list rows carry their ORIGINAL
//! catalog index, a click must postdate the list it was made on, and a
//! choice is consumed only after the window is destroyed. Refreshing (the
//! button, F5, or automatically before the snapshot's two-minute budget runs
//! out) replaces the snapshot and its choice together, so an old click can
//! never select a row of a new snapshot. Operating system, online state and
//! path are display metadata from the same snapshot. No remote probe,
//! observation or session starts while this window is open.
use crate::host_picker::Choice;
pub(super) use crate::host_picker::Error;
use crate::machines::{self, Platform, Row};
use crate::outcome;
use crate::receive_choice::{Choice as ReceiveChoice, Destination};
use crate::settings::Settings;
use fr_client::discovery::picker::{Candidate, Catalog};
use fr_mobile_ffi::session::connection;
use fr_mobile_ffi::video::DecoderSupport;
use fr_tailnet::Discovery;
use std::{
    cell::{Cell, RefCell},
    ffi::c_void,
    rc::{Rc, Weak},
};
use windows::{
    Win32::{
        Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM},
        Graphics::Gdi::{
            COLOR_WINDOW, COLOR_WINDOWTEXT, CreateFontIndirectW, CreateSolidBrush, DeleteObject,
            FW_SEMIBOLD, GetSysColor, GetSysColorBrush, HBRUSH, HDC, HFONT, HGDIOBJ,
            InvalidateRect, SetBkColor, SetTextColor,
        },
        System::{LibraryLoader::GetModuleHandleW, SystemInformation::GetTickCount64},
        UI::{
            Controls::{
                BST_CHECKED, CDDS_ITEMPREPAINT, CDDS_PREPAINT, CDDS_SUBITEM, CDRF_DODEFAULT,
                CDRF_NEWFONT, CDRF_NOTIFYITEMDRAW, CDRF_NOTIFYSUBITEMDRAW, EM_SETCUEBANNER,
                LIST_VIEW_ITEM_STATE_FLAGS, LVCF_FMT, LVCF_SUBITEM, LVCF_TEXT, LVCF_WIDTH,
                LVCFMT_LEFT, LVCOLUMNW, LVIF_PARAM, LVIF_STATE, LVIF_TEXT, LVIS_FOCUSED,
                LVIS_SELECTED, LVITEMW, LVM_DELETEALLITEMS, LVM_ENSUREVISIBLE, LVM_GETITEMW,
                LVM_GETNEXTITEM, LVM_INSERTCOLUMNW, LVM_INSERTITEMW, LVM_SETCOLUMNWIDTH,
                LVM_SETEXTENDEDLISTVIEWSTYLE, LVM_SETITEMSTATE, LVM_SETITEMTEXTW, LVN_ITEMCHANGED,
                LVNI_SELECTED, LVS_EX_DOUBLEBUFFER, LVS_EX_FULLROWSELECT, LVS_NOSORTHEADER,
                LVS_REPORT, LVS_SHOWSELALWAYS, LVS_SINGLESEL, LVSCW_AUTOSIZE_USEHEADER,
                NM_CUSTOMDRAW, NM_DBLCLK, NMHDR, NMITEMACTIVATE, NMLVCUSTOMDRAW, SetWindowTheme,
                WC_LISTVIEWW,
            },
            HiDpi::{GetDpiForSystem, GetDpiForWindow, SystemParametersInfoForDpi},
            Input::KeyboardAndMouse::{
                EnableWindow, GetFocus, GetKeyState, SetFocus, VK_CONTROL, VK_DOWN, VK_ESCAPE,
                VK_F5, VK_RETURN, VK_UP,
            },
            Shell::ShellExecuteW,
            WindowsAndMessaging::{
                BM_GETCHECK, BM_SETCHECK, BN_CLICKED, BS_AUTOCHECKBOX, BS_AUTORADIOBUTTON,
                BS_DEFPUSHBUTTON, BS_PUSHBUTTON, CW_USEDEFAULT, CreateWindowExW, DefWindowProcW,
                DestroyWindow, DispatchMessageW, EN_CHANGE, ES_AUTOHSCROLL, GetClientRect,
                GetMessageTime, GetWindowTextLengthW, GetWindowTextW, HMENU, IDC_ARROW, IsChild,
                IsDialogMessageW, LoadCursorW, MINMAXINFO, MSG, MoveWindow,
                MsgWaitForMultipleObjects, NONCLIENTMETRICSW, PM_NOREMOVE, PM_REMOVE, PeekMessageW,
                QS_ALLINPUT, RegisterClassExW, SPI_GETNONCLIENTMETRICS, SW_HIDE, SW_SHOW,
                SW_SHOWNORMAL, SWP_NOACTIVATE, SWP_NOZORDER, SendMessageW, SetForegroundWindow,
                SetWindowPos, SetWindowTextW, ShowWindow, TranslateMessage, WINDOW_EX_STYLE,
                WINDOW_STYLE, WM_CLOSE, WM_COMMAND, WM_CTLCOLORSTATIC, WM_DESTROY, WM_DPICHANGED,
                WM_GETMINMAXINFO, WM_KEYDOWN, WM_NOTIFY, WM_QUIT, WM_SETFONT, WM_SIZE, WNDCLASSEXW,
                WS_CHILD, WS_CLIPCHILDREN, WS_EX_CLIENTEDGE, WS_GROUP, WS_OVERLAPPEDWINDOW,
                WS_TABSTOP, WS_VISIBLE,
            },
        },
    },
    core::{HSTRING, PCWSTR, PWSTR, w},
};

/// Refresh well before the two-minute snapshot budget expires.
const REFRESH_AFTER_MS: u64 = 90_000;
/// The Microsoft Store page of Microsoft's HEVC Video Extensions.
const HEVC_STORE: &str = "ms-windows-store://pdp/?ProductId=9NMZLZ57R3T7";
/// Static-control styles (`SS_NOPREFIX | SS_ENDELLIPSIS`), without pulling
/// in the system-services bindings for two constants.
const STATIC_STYLE: u32 = 0x0080 | 0x4000;

const ID_CONNECT: usize = 1;
const ID_CLOSE: usize = 2;
const ID_REFRESH: usize = 3;
const ID_SEARCH: usize = 4;
const ID_LIST: usize = 5;
const ID_VIEW: usize = 6;
const ID_CONTROL: usize = 7;
const ID_CLIPBOARD: usize = 8;
const ID_DISPLAY: usize = 9;
const ID_INSTALL: usize = 10;
const ID_DECODER: usize = 11;
const ID_FULLSCREEN: usize = 12;
const ID_SHORTCUTS: usize = 13;
const ID_RECEIVE: usize = 14;
const ID_CLEAR_RECEIVE: usize = 15;
const ID_AUDIO: usize = 16;

/// Who opened the picker: the command line keeps its explicit options; the
/// application also shows connection options and remembers them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Mode {
    Command,
    Application,
}

/// One line above the list: the last session's outcome or setup guidance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Notice {
    pub text: String,
    pub warning: bool,
    /// Kept across refreshes (session outcomes); others are replaced.
    pub sticky: bool,
}

pub(super) struct Request {
    pub mode: Mode,
    /// The validated endpoint port of this original app or viewing command.
    pub port: u16,
    pub settings: Settings,
    pub notice: Option<Notice>,
    pub support: Option<DecoderSupport>,
    pub offer_install: bool,
}

/// The chosen row's original identity, the snapshot it came from, and the
/// connection options shown when it was chosen.
pub(super) struct Chosen {
    pub snapshot: Discovery,
    pub peer: Candidate,
    pub settings: Settings,
    /// One explicit local choice from this picker, never remembered settings.
    pub receive: Option<Destination>,
}

pub(super) enum Picked {
    Machine(Chosen),
    Install,
    Closed,
}

#[derive(Debug)]
pub(super) enum PickError {
    /// The command line's first snapshot failed before any window opened.
    Discovery(fr_tailnet::Error),
    Choice(Error),
}

pub(super) fn ticks() -> u64 {
    // SAFETY: GetTickCount64 reads the OS uptime clock without pointer arguments.
    unsafe { GetTickCount64() }
}

#[derive(Clone, Copy, Default)]
struct Controls {
    heading: HWND,
    subtitle: HWND,
    notice: HWND,
    decoder: HWND,
    search: HWND,
    refresh: HWND,
    list: HWND,
    view: HWND,
    control: HWND,
    clipboard: HWND,
    audio: HWND,
    display: HWND,
    fullscreen: HWND,
    shortcuts: HWND,
    receive: HWND,
    receive_path: HWND,
    clear_receive: HWND,
    status: HWND,
    install: HWND,
    connect: HWND,
    close: HWND,
}

impl Controls {
    fn all(&self) -> [HWND; 21] {
        [
            self.heading,
            self.subtitle,
            self.notice,
            self.decoder,
            self.search,
            self.refresh,
            self.list,
            self.view,
            self.control,
            self.clipboard,
            self.audio,
            self.display,
            self.fullscreen,
            self.shortcuts,
            self.receive,
            self.receive_path,
            self.clear_receive,
            self.status,
            self.install,
            self.connect,
            self.close,
        ]
    }
}

/// Deferred work requested from the window procedure and run by the loop,
/// outside any native callback.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Action {
    None,
    Refresh,
    Cancel,
    Install,
    Store,
    Receive,
    ClearReceive,
}

struct Snapshot {
    discovery: Discovery,
    choice: Choice,
    rows: Vec<Row>,
    /// (dimmed, online) by original catalog index, for custom drawing.
    display: Vec<(bool, bool)>,
    started: u64,
}

struct State {
    hwnd: HWND,
    mode: Mode,
    base: Settings,
    controls: Cell<Controls>,
    fonts: Cell<(HFONT, HFONT)>,
    brushes: (HBRUSH, HBRUSH),
    snapshot: RefCell<Option<Snapshot>>,
    notice: RefCell<Option<Notice>>,
    decoder_line: String,
    decoder_missing: bool,
    offer_install: bool,
    /// The selected row's stable ID (UI state only; never a choice).
    selected: RefCell<Option<String>>,
    receive: RefCell<ReceiveChoice>,
    choosing_receive: Cell<bool>,
    filling: Cell<bool>,
    action: Cell<Action>,
    retiring: Cell<bool>,
    closed: Cell<bool>,
}

impl State {
    fn owns_input(&self, hwnd: HWND) -> bool {
        // SAFETY: a pure window-relationship query.
        hwnd == self.hwnd || unsafe { IsChild(self.hwnd, hwnd) }.as_bool()
    }

    fn request(&self, action: Action) {
        // Cancel wins over everything; otherwise the latest request stands.
        if self.action.get() != Action::Cancel {
            self.action.set(action);
        }
    }

    fn cancel(&self) {
        self.action.set(Action::Cancel);
        self.receive.borrow_mut().clear();
        if let Some(snapshot) = self.snapshot.borrow_mut().as_mut() {
            snapshot.choice.refuse(Error::Cancelled);
        }
    }

    fn dpi(&self) -> i32 {
        // SAFETY: a scalar query of this thread's live window.
        i32::try_from(unsafe { GetDpiForWindow(self.hwnd) }.clamp(96, 768)).unwrap_or(96)
    }

    /// The selected list item's original catalog index, if any.
    fn selected_index(&self) -> Option<usize> {
        let list = self.controls.get().list;
        // SAFETY: LVM_GETNEXTITEM returns an ordinal; LVM_GETITEMW fills the
        // caller's LVITEMW (only the lParam mask) during the call.
        unsafe {
            let item = SendMessageW(
                list,
                LVM_GETNEXTITEM,
                Some(WPARAM(usize::MAX)),
                Some(LPARAM(wide(LVNI_SELECTED))),
            )
            .0;
            let item = i32::try_from(item).ok().filter(|item| *item >= 0)?;
            let mut lv = LVITEMW {
                mask: LVIF_PARAM,
                iItem: item,
                ..Default::default()
            };
            if SendMessageW(
                list,
                LVM_GETITEMW,
                None,
                Some(LPARAM((&raw mut lv) as isize)),
            )
            .0 == 0
            {
                return None;
            }
            usize::try_from(lv.lParam.0).ok()
        }
    }

    fn connect(&self, queued: u32) {
        if self.choosing_receive.get() || self.action.get() != Action::None {
            return;
        }
        let Some(index) = self.selected_index() else {
            return;
        };
        // No snapshot borrow spans the native reads above. Recheck the
        // original event time, snapshot and deadline in the policy.
        if let Some(snapshot) = self.snapshot.borrow_mut().as_mut() {
            snapshot.choice.choose(index, queued, ticks());
        }
    }

    fn search_text(&self) -> String {
        let search = self.controls.get().search;
        // SAFETY: bounded text read into a buffer this function owns.
        unsafe {
            let length = usize::try_from(GetWindowTextLengthW(search))
                .unwrap_or(0)
                .min(256);
            let mut buffer = vec![0_u16; length + 1];
            let read = usize::try_from(GetWindowTextW(search, &mut buffer)).unwrap_or(0);
            String::from_utf16_lossy(&buffer[..read.min(length)])
        }
    }

    fn options(&self, machine: &str) -> Settings {
        let mut settings = self.base.clone();
        settings.last_machine = Some(machine.to_owned());
        if self.mode == Mode::Application {
            let controls = self.controls.get();
            settings.control = checked(controls.control);
            settings.clipboard = checked(controls.clipboard);
            settings.audio = checked(controls.audio);
            settings.choose_display = checked(controls.display);
            settings.fullscreen = checked(controls.fullscreen);
            settings.shortcuts = checked(controls.shortcuts);
        }
        settings
    }

    fn show_receive(&self) {
        if self.mode != Mode::Application {
            return;
        }
        let controls = self.controls.get();
        let control = checked(controls.control);
        let (label, selected) = {
            let receive = self.receive.borrow();
            match receive.path() {
                Some(path) => (format!("Receive into: {path}"), true),
                None => (
                    "Off. Choose a folder for this connection.".to_owned(),
                    false,
                ),
            }
        };
        // SAFETY: the path is copied only into this local UI control. No
        // receive policy borrow spans these reentrant native messages.
        unsafe {
            let _ = SetWindowTextW(controls.receive_path, &HSTRING::from(label));
            let _ = EnableWindow(controls.receive, control && !self.choosing_receive.get());
            let _ = EnableWindow(controls.clear_receive, control && selected);
        }
    }

    fn clear_receive(&self) {
        self.receive.borrow_mut().clear();
        self.show_receive();
    }

    fn choose_receive(&self) {
        if self.mode != Mode::Application
            || !checked(self.controls.get().control)
            || self.choosing_receive.replace(true)
        {
            return;
        }
        let request = self.receive.borrow_mut().begin(ticks());
        let Ok(request) = request else {
            self.choosing_receive.set(false);
            self.clear_receive();
            return;
        };
        self.show_receive();
        let selected = super::receive_folder::choose(self.hwnd);
        self.choosing_receive.set(false);
        if self.action.get() == Action::Cancel
            || self.closed.get()
            || self.retiring.get()
            || !checked(self.controls.get().control)
        {
            self.clear_receive();
            return;
        }
        let result = match selected {
            Ok(path) => self
                .receive
                .borrow_mut()
                .finish(request, path.as_deref(), ticks()),
            Err(error) => {
                self.receive.borrow_mut().clear();
                Err(error)
            }
        };
        self.show_receive();
        // Native dialogs can outlive a discovery snapshot and pump queued
        // messages. Drop every old row/gesture before another Connect choice.
        let notice = result.err().map(|_| Notice {
            text: "The receive folder could not be selected. Choose it again, or connect without file receiving."
                .to_owned(),
            warning: true,
            sticky: false,
        });
        self.refresh(notice);
    }

    /// Replace the snapshot and its choice; an error clears the list.
    fn install_snapshot(
        &self,
        result: Result<Discovery, fr_tailnet::Error>,
        started: u64,
        notice: Option<Notice>,
    ) {
        let built = result.and_then(|discovery| {
            let catalog = connection::candidates(&discovery)?;
            Ok((discovery, catalog))
        });
        match built {
            Ok((discovery, catalog)) => {
                let (rows, display) = rows(&discovery, &catalog);
                match Choice::new(catalog, started) {
                    Ok(choice) => {
                        *self.snapshot.borrow_mut() = Some(Snapshot {
                            discovery,
                            choice,
                            rows,
                            display,
                            started,
                        });
                        let mut current = self.notice.borrow_mut();
                        if notice.is_some() || current.as_ref().is_some_and(|n| !n.sticky) {
                            *current = notice;
                        }
                    }
                    Err(error) => {
                        *self.snapshot.borrow_mut() = None;
                        *self.notice.borrow_mut() = Some(Notice {
                            text: format!(
                                "The machine list could not be read ({error:?}). Refresh to try again."
                            ),
                            warning: true,
                            sticky: false,
                        });
                    }
                }
            }
            Err(error) => {
                *self.snapshot.borrow_mut() = None;
                *self.notice.borrow_mut() = Some(Notice {
                    text: discovery_guidance(error).to_owned(),
                    warning: true,
                    sticky: false,
                });
            }
        }
        self.fill_list();
        // Only events after the populated native list can choose a row.
        if let Some(snapshot) = self.snapshot.borrow_mut().as_mut() {
            snapshot.choice.shown(ticks());
        }
        self.show_notice();
        self.layout();
    }

    fn refresh(&self, notice: Option<Notice>) {
        let started = ticks();
        let result = connection::discover();
        self.install_snapshot(result, started, notice);
    }

    /// Rebuild the native rows from the current snapshot and search text,
    /// keeping the selected machine selected when it is still listed.
    fn fill_list(&self) {
        let query = self.search_text();
        let entries: Vec<(usize, String, [String; 5])> = self
            .snapshot
            .borrow()
            .as_ref()
            .map(|snapshot| {
                snapshot
                    .rows
                    .iter()
                    .filter(|row| row.matches(&query))
                    .map(|row| {
                        let status = match row.online {
                            Some(true) => "\u{25CF} Online".to_owned(),
                            Some(false) => "\u{25CB} Offline".to_owned(),
                            None => row.status_label().to_owned(),
                        };
                        (
                            row.index,
                            row.stable_id.clone(),
                            [
                                row.short_name().to_owned(),
                                status,
                                row.system_label().to_owned(),
                                row.path_label().to_owned(),
                                row.address.clone().unwrap_or_default(),
                            ],
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();
        let wanted = self
            .selected
            .borrow()
            .clone()
            .or_else(|| self.base.last_machine.clone());
        let list = self.controls.get().list;
        self.filling.set(true);
        let mut select = None;
        // SAFETY: list-view messages on this thread's live control; every
        // LVITEMW and its UTF-16 text outlive the synchronous call that
        // copies them.
        unsafe {
            SendMessageW(list, LVM_DELETEALLITEMS, None, None);
            for (position, (index, stable_id, columns)) in entries.iter().enumerate() {
                let position = i32::try_from(position).unwrap_or(i32::MAX);
                let mut text: Vec<u16> = columns[0].encode_utf16().chain([0]).collect();
                let mut item = LVITEMW {
                    mask: LVIF_TEXT | LVIF_PARAM,
                    iItem: position,
                    pszText: PWSTR(text.as_mut_ptr()),
                    lParam: LPARAM(isize::try_from(*index).unwrap_or(isize::MAX)),
                    ..Default::default()
                };
                let inserted = SendMessageW(
                    list,
                    LVM_INSERTITEMW,
                    None,
                    Some(LPARAM((&raw mut item) as isize)),
                )
                .0;
                for (column, value) in columns.iter().enumerate().skip(1) {
                    let mut text: Vec<u16> = value.encode_utf16().chain([0]).collect();
                    let mut sub = LVITEMW {
                        iSubItem: i32::try_from(column).unwrap_or(0),
                        pszText: PWSTR(text.as_mut_ptr()),
                        ..Default::default()
                    };
                    SendMessageW(
                        list,
                        LVM_SETITEMTEXTW,
                        Some(WPARAM(usize::try_from(inserted).unwrap_or(0))),
                        Some(LPARAM((&raw mut sub) as isize)),
                    );
                }
                if wanted.as_deref() == Some(stable_id.as_str()) {
                    select = Some(inserted);
                }
            }
            let select = select.or_else(|| (!entries.is_empty()).then_some(0));
            if let Some(item) = select {
                let mut state = LVITEMW {
                    mask: LVIF_STATE,
                    state: LIST_VIEW_ITEM_STATE_FLAGS(LVIS_SELECTED.0 | LVIS_FOCUSED.0),
                    stateMask: LIST_VIEW_ITEM_STATE_FLAGS(LVIS_SELECTED.0 | LVIS_FOCUSED.0),
                    ..Default::default()
                };
                let item = usize::try_from(item).unwrap_or(0);
                SendMessageW(
                    list,
                    LVM_SETITEMSTATE,
                    Some(WPARAM(item)),
                    Some(LPARAM((&raw mut state) as isize)),
                );
                SendMessageW(list, LVM_ENSUREVISIBLE, Some(WPARAM(item)), Some(LPARAM(0)));
            }
        }
        self.filling.set(false);
        self.selection_changed();
    }

    fn selection_changed(&self) {
        let index = self.selected_index();
        let selected = index.and_then(|index| {
            self.snapshot.borrow().as_ref().and_then(|snapshot| {
                snapshot
                    .rows
                    .iter()
                    .find(|row| row.index == index)
                    .map(|row| row.stable_id.clone())
            })
        });
        if let Some(id) = &selected {
            *self.selected.borrow_mut() = Some(id.clone());
        }
        let controls = self.controls.get();
        let counts = self
            .snapshot
            .borrow()
            .as_ref()
            .map(|snapshot| machines::counts(&snapshot.rows));
        let mut line = match counts {
            Some((listed, online, _)) => {
                format!("{listed} machines, {online} online in Tailscale")
            }
            None => "No machine list".to_owned(),
        };
        if !self.decoder_line.is_empty() {
            line.push('\n');
            line.push_str(&self.decoder_line);
        }
        // SAFETY: text copied synchronously into this thread's controls.
        unsafe {
            let _ = SetWindowTextW(controls.status, &HSTRING::from(line));
            let _ = EnableWindow(controls.connect, selected.is_some());
        }
    }

    fn show_notice(&self) {
        let controls = self.controls.get();
        let notice = self.notice.borrow().clone();
        // SAFETY: text and visibility of this thread's own controls.
        unsafe {
            match notice {
                Some(notice) => {
                    let _ = SetWindowTextW(
                        controls.notice,
                        &HSTRING::from(format!("  {}", notice.text)),
                    );
                    let _ = ShowWindow(controls.notice, SW_SHOW);
                    let _ = InvalidateRect(Some(controls.notice), None, true);
                }
                None => {
                    let _ = ShowWindow(controls.notice, SW_HIDE);
                }
            }
            let _ = ShowWindow(
                controls.decoder,
                if self.decoder_missing {
                    SW_SHOW
                } else {
                    SW_HIDE
                },
            );
        }
    }

    fn notice_visible(&self) -> bool {
        self.notice.borrow().is_some()
    }

    #[allow(clippy::too_many_lines, reason = "one fixed layout")]
    fn layout(&self) {
        let dpi = self.dpi();
        let s = |value: i32| value * dpi / 96;
        let mut client = RECT::default();
        // SAFETY: a live window and a writable rectangle.
        if unsafe { GetClientRect(self.hwnd, &raw mut client) }.is_err() {
            return;
        }
        let (width, height) = (client.right - client.left, client.bottom - client.top);
        let c = self.controls.get();
        let margin = s(20);
        let inner = (width - 2 * margin).max(s(100));
        let place = |hwnd: HWND, x: i32, y: i32, w: i32, h: i32| {
            // SAFETY: moving this thread's own child window.
            let _ = unsafe { MoveWindow(hwnd, x, y, w.max(0), h.max(0), true) };
        };
        place(c.heading, margin, s(14), inner, s(32));
        place(c.subtitle, margin, s(48), inner, s(20));
        let mut y = s(76);
        if self.notice_visible() || self.decoder_missing {
            let button = if self.decoder_missing { s(190) } else { 0 };
            place(c.notice, margin, y, inner - button, s(40));
            place(
                c.decoder,
                margin + inner - button + s(8),
                y + s(4),
                button - s(8),
                s(32),
            );
            y += s(50);
        }
        place(c.search, margin, y, inner - s(112), s(28));
        place(c.refresh, margin + inner - s(100), y, s(100), s(28));
        y += s(38);
        let footer = if self.mode == Mode::Application {
            s(190)
        } else {
            s(64)
        };
        place(c.list, margin, y, inner, (height - y - footer).max(s(80)));
        if self.mode == Mode::Application {
            let row = height - s(182);
            place(c.view, margin, row, s(110), s(24));
            place(c.control, margin + s(116), row, s(150), s(24));
            place(c.clipboard, margin + s(280), row, s(180), s(24));
            place(c.display, margin + s(470), row, s(200), s(24));
            let row = height - s(152);
            place(c.fullscreen, margin, row, s(266), s(24));
            place(c.shortcuts, margin + s(280), row, s(390), s(24));
            let row = height - s(122);
            place(c.audio, margin, row, inner, s(24));
            let row = height - s(92);
            place(c.receive, margin, row, s(150), s(28));
            place(c.clear_receive, margin + s(158), row, s(64), s(28));
            place(
                c.receive_path,
                margin + s(232),
                row + s(4),
                inner - s(232),
                s(24),
            );
        }
        let bottom = height - s(50);
        let buttons = s(232) + if self.offer_install { s(170) } else { 0 };
        place(
            c.status,
            margin,
            bottom - s(2),
            inner - buttons - s(12),
            s(40),
        );
        if self.offer_install {
            place(c.install, margin + inner - s(402), bottom, s(160), s(32));
        }
        place(c.connect, margin + inner - s(232), bottom, s(110), s(32));
        place(c.close, margin + inner - s(110), bottom, s(110), s(32));
        // Fill the list's last column to its width.
        // SAFETY: a column-width message to this thread's list view.
        unsafe {
            SendMessageW(
                c.list,
                LVM_SETCOLUMNWIDTH,
                Some(WPARAM(4)),
                Some(LPARAM(LVSCW_AUTOSIZE_USEHEADER as isize)),
            );
        }
    }

    /// Fonts for the window's current DPI, applied to every child.
    fn apply_fonts(&self) {
        let (normal, heading) = fonts(u32::try_from(self.dpi()).unwrap_or(96));
        let old = self.fonts.replace((normal, heading));
        let controls = self.controls.get();
        // SAFETY: WM_SETFONT on this thread's children; old fonts are deleted
        // only after every child has been given its replacement.
        unsafe {
            for child in controls.all() {
                let font = if child == controls.heading {
                    heading
                } else {
                    normal
                };
                SendMessageW(
                    child,
                    WM_SETFONT,
                    Some(WPARAM(font.0 as usize)),
                    Some(LPARAM(1)),
                );
            }
            delete_fonts(old);
        }
    }

    fn colors(&self, hdc: HDC, control: HWND) -> LRESULT {
        let controls = self.controls.get();
        let warning = self
            .notice
            .borrow()
            .as_ref()
            .is_some_and(|notice| notice.warning);
        // SAFETY: the HDC belongs to this WM_CTLCOLORSTATIC call; the
        // returned brushes live as long as the window.
        unsafe {
            if control == controls.notice {
                let _ = SetTextColor(hdc, rgb(0x20, 0x20, 0x20));
                if warning {
                    let _ = SetBkColor(hdc, rgb(0xFF, 0xF4, 0xCE));
                    return LRESULT(self.brushes.0.0 as isize);
                }
                let _ = SetBkColor(hdc, rgb(0xE5, 0xF1, 0xFB));
                return LRESULT(self.brushes.1.0 as isize);
            }
            let window = GetSysColor(COLOR_WINDOW);
            let _ = SetBkColor(hdc, COLORREF(window));
            if control == controls.subtitle || control == controls.status {
                let _ = SetTextColor(hdc, rgb(0x5C, 0x5C, 0x5C));
            } else {
                let _ = SetTextColor(hdc, COLORREF(GetSysColor(COLOR_WINDOWTEXT)));
            }
            LRESULT(GetSysColorBrush(COLOR_WINDOW).0 as isize)
        }
    }

    /// Text colors per row: offline and viewer-only rows are dimmed, and the
    /// Online label is green.
    fn custom_draw(&self, draw: &mut NMLVCUSTOMDRAW) -> LRESULT {
        let stage = draw.nmcd.dwDrawStage;
        if stage == CDDS_PREPAINT {
            return LRESULT(wide(CDRF_NOTIFYITEMDRAW));
        }
        if stage == CDDS_ITEMPREPAINT {
            return LRESULT(wide(CDRF_NOTIFYSUBITEMDRAW));
        }
        if stage.0 != CDDS_ITEMPREPAINT.0 | CDDS_SUBITEM.0 {
            return LRESULT(wide(CDRF_DODEFAULT));
        }
        let index = usize::try_from(draw.nmcd.lItemlParam.0).unwrap_or(usize::MAX);
        let (dimmed, online) = self
            .snapshot
            .borrow()
            .as_ref()
            .and_then(|snapshot| snapshot.display.get(index).copied())
            .unwrap_or((false, false));
        draw.clrText = if draw.iSubItem == 1 && online && !dimmed {
            rgb(0x1E, 0x7B, 0x34)
        } else if dimmed {
            rgb(0x8A, 0x8A, 0x8A)
        } else {
            // SAFETY: a scalar system color query.
            COLORREF(unsafe { GetSysColor(COLOR_WINDOWTEXT) })
        };
        LRESULT(wide(CDRF_NEWFONT))
    }
}

fn checked(control: HWND) -> bool {
    // SAFETY: a scalar button-state query.
    unsafe { SendMessageW(control, BM_GETCHECK, None, None) }.0 == wide(BST_CHECKED.0)
}

/// A 32-bit message constant as a pointer-sized message value.
fn wide(value: u32) -> isize {
    isize::try_from(value).unwrap_or(isize::MAX)
}

const fn rgb(red: u8, green: u8, blue: u8) -> COLORREF {
    COLORREF(red as u32 | (green as u32) << 8 | (blue as u32) << 16)
}

fn rows(discovery: &Discovery, catalog: &Catalog) -> (Vec<Row>, Vec<(bool, bool)>) {
    let mut rows: Vec<Row> = catalog
        .candidates()
        .iter()
        .enumerate()
        .map(|(index, candidate)| {
            let peer = discovery
                .peers()
                .iter()
                .find(|peer| peer.stable_id() == candidate.stable_id());
            Row {
                index,
                stable_id: candidate.stable_id().to_owned(),
                full_name: candidate.certificate_name().to_owned(),
                platform: Platform::from_os(peer.and_then(fr_tailnet::DiscoveredPeer::os)),
                online: peer.and_then(fr_tailnet::DiscoveredPeer::online),
                path: candidate.path_hint(),
                address: peer.and_then(|peer| machines::display_address(peer.addresses())),
            }
        })
        .collect();
    let mut display = vec![(false, false); rows.len()];
    for row in &rows {
        if let Some(slot) = display.get_mut(row.index) {
            *slot = (row.dimmed(), row.online == Some(true));
        }
    }
    machines::sort(&mut rows);
    (rows, display)
}

pub(super) fn discovery_guidance(error: fr_tailnet::Error) -> &'static str {
    use fr_tailnet::Error;
    match error {
        Error::BackendNotRunning => {
            "Tailscale is not connected on this PC. Connect it from the Tailscale icon, then Refresh."
        }
        Error::LocalApiUnavailable => {
            "Tailscale is not running on this PC. Install or start Tailscale, then Refresh."
        }
        Error::LocalApiDenied | Error::UntrustedLocalApi => {
            "Windows denied access to the Tailscale service. Check the Tailscale installation, then Refresh."
        }
        Error::Timeout => "Tailscale did not answer in time. Refresh to try again.",
        _ => "The machine list could not be read from Tailscale. Refresh to try again.",
    }
}

fn fonts(dpi: u32) -> (HFONT, HFONT) {
    let mut metrics = NONCLIENTMETRICSW {
        cbSize: u32::try_from(size_of::<NONCLIENTMETRICSW>()).unwrap_or(0),
        ..Default::default()
    };
    // SAFETY: the metrics structure is writable and sized; the fonts are
    // created from copies of its LOGFONTW and owned by the caller.
    unsafe {
        let _ = SystemParametersInfoForDpi(
            SPI_GETNONCLIENTMETRICS.0,
            metrics.cbSize,
            Some((&raw mut metrics).cast()),
            0,
            dpi,
        );
        let normal = CreateFontIndirectW(&raw const metrics.lfMessageFont);
        let mut heading = metrics.lfMessageFont;
        heading.lfHeight = heading.lfHeight * 3 / 2;
        heading.lfWeight = FW_SEMIBOLD.0.cast_signed();
        let heading = CreateFontIndirectW(&raw const heading);
        (normal, heading)
    }
}

/// # Safety
/// No window may still be using either font.
unsafe fn delete_fonts((normal, heading): (HFONT, HFONT)) {
    for font in [normal, heading] {
        if !font.is_invalid() {
            // SAFETY: (caller) the font is no longer selected by any window.
            let _ = unsafe { DeleteObject(HGDIOBJ(font.0)) };
        }
    }
}

thread_local! {
    static ACTIVE: RefCell<Option<Weak<State>>> = const { RefCell::new(None) };
}

fn active(hwnd: HWND) -> Option<Rc<State>> {
    ACTIVE.with(|slot| {
        slot.borrow()
            .as_ref()
            .and_then(Weak::upgrade)
            .filter(|state| state.hwnd == hwnd)
    })
}

struct Picker(Rc<State>);

impl Picker {
    fn close(&self) -> Result<(), Error> {
        if self.0.closed.get() {
            return Ok(());
        }
        self.0.retiring.set(true);
        // SAFETY: this UI thread owns the root and its children. The weak
        // callback owner stays available so a reentrant close still cancels.
        if unsafe { DestroyWindow(self.0.hwnd) }.is_err() {
            if let Some(snapshot) = self.0.snapshot.borrow_mut().as_mut() {
                snapshot.choice.refuse(Error::CleanupIncomplete);
            }
            return Err(Error::CleanupIncomplete);
        }
        self.0.closed.set(true);
        ACTIVE.with(|slot| {
            let mut slot = slot.borrow_mut();
            if slot
                .as_ref()
                .and_then(Weak::upgrade)
                .is_some_and(|state| Rc::ptr_eq(&state, &self.0))
            {
                *slot = None;
            }
        });
        Ok(())
    }
}

impl Drop for Picker {
    fn drop(&mut self) {
        self.0.receive.borrow_mut().clear();
        if let Some(snapshot) = self.0.snapshot.borrow_mut().as_mut() {
            snapshot.choice.refuse(Error::Cancelled);
        }
        let closed = self.close().is_ok();
        if closed {
            // SAFETY: the window and every child using these objects is gone.
            unsafe {
                delete_fonts(self.0.fonts.get());
                let _ = DeleteObject(HGDIOBJ(self.0.brushes.0.0));
                let _ = DeleteObject(HGDIOBJ(self.0.brushes.1.0));
            }
        }
    }
}

/// Show the picker until a machine is chosen, installation is requested or
/// the window is closed.
#[allow(
    clippy::too_many_lines,
    reason = "one bounded message loop and its ordered cleanup"
)]
pub(super) fn pick(request: &Request) -> Result<Picked, PickError> {
    let started = ticks();
    let first = connection::discover();
    if request.mode == Mode::Command
        && let Err(error) = first
    {
        return Err(PickError::Discovery(error));
    }
    let picker = open(request).map_err(|_| PickError::Choice(Error::NativeUnavailable))?;
    picker
        .0
        .install_snapshot(first, started, request.notice.clone());
    // SAFETY: bringing this thread's new top-level window forward.
    unsafe {
        let _ = SetForegroundWindow(picker.0.hwnd);
        let _ = SetFocus(Some(picker.0.controls.get().list));
    }
    let chosen_settings;
    loop {
        // Finish a bounded batch even after Connect: a cancellation queued
        // behind that click must still invalidate the provisional selection.
        for _ in 0..64 {
            let mut message = MSG::default();
            // SAFETY: writable MSG, this thread's message queue, no raw payloads.
            if !unsafe { PeekMessageW(&raw mut message, None, 0, 0, PM_REMOVE) }.as_bool() {
                break;
            }
            if message.message == WM_QUIT {
                picker.0.cancel();
                continue;
            }
            if message.message == WM_KEYDOWN
                && picker.0.owns_input(message.hwnd)
                && pretranslate(&picker.0, &message)
            {
                continue;
            }
            // SAFETY: live UI-thread window and the initialized queue MSG.
            unsafe {
                if !IsDialogMessageW(picker.0.hwnd, &raw const message).as_bool() {
                    let _ = TranslateMessage(&raw const message);
                    DispatchMessageW(&raw const message);
                }
            }
        }
        match picker.0.action.replace(Action::None) {
            Action::Cancel => {
                picker.close().map_err(PickError::Choice)?;
                return Ok(Picked::Closed);
            }
            Action::Install => {
                picker.close().map_err(PickError::Choice)?;
                return Ok(Picked::Install);
            }
            Action::Refresh => picker.0.refresh(None),
            Action::Receive => picker.0.choose_receive(),
            Action::ClearReceive => picker.0.clear_receive(),
            Action::Store => {
                // SAFETY: a fixed Store URI handed to the shell for this window.
                unsafe {
                    ShellExecuteW(
                        Some(picker.0.hwnd),
                        w!("open"),
                        &HSTRING::from(HEVC_STORE),
                        PCWSTR::null(),
                        PCWSTR::null(),
                        SW_SHOWNORMAL,
                    );
                }
            }
            Action::None => {}
        }
        let now = ticks();
        let state = picker.0.snapshot.borrow_mut().as_mut().map(|snapshot| {
            let done = snapshot.choice.complete(now);
            (done, snapshot.choice.refusal(), snapshot.started)
        });
        match state {
            Some((true, None, _)) => {
                let mut pending = MSG::default();
                // SAFETY: writable MSG, no removal or borrowed message payload.
                // Never let Connect overtake a close hidden behind a queue flood.
                if unsafe { PeekMessageW(&raw mut pending, None, 0, 0, PM_NOREMOVE) }.as_bool() {
                    if let Some(snapshot) = picker.0.snapshot.borrow_mut().as_mut() {
                        snapshot.choice.refuse(Error::EventFlood);
                    }
                    continue;
                }
                let machine = picker.0.selected.borrow().clone().unwrap_or_default();
                chosen_settings = picker.0.options(&machine);
                break;
            }
            Some((true, Some(Error::Cancelled), _)) => {
                picker.close().map_err(PickError::Choice)?;
                return Ok(Picked::Closed);
            }
            Some((true, Some(Error::Expired), _)) => picker.0.refresh(None),
            Some((true, Some(error), _)) => picker.0.refresh(Some(Notice {
                text: format!(
                    "That selection was no longer current ({error:?}). Choose the machine again."
                ),
                warning: true,
                sticky: false,
            })),
            Some((false, _, started)) if now.saturating_sub(started) >= REFRESH_AFTER_MS => {
                picker.0.refresh(None);
            }
            _ => {}
        }
        // SAFETY: no handles or borrowed buffers; wait only for this UI
        // thread's input, with a bounded timeout to service expiry.
        unsafe { MsgWaitForMultipleObjects(None, false, 100, QS_ALLINPUT) };
    }
    picker.close().map_err(PickError::Choice)?;
    // Cleanup consumed part of the same original budget. No candidate leaves
    // this owner until destruction succeeded and the clock is rechecked.
    let Some(mut snapshot) = picker.0.snapshot.borrow_mut().take() else {
        return Err(PickError::Choice(Error::InvalidChoice));
    };
    match snapshot.choice.take(ticks()) {
        Ok(peer) => {
            let mut settings = chosen_settings;
            settings.last_machine = Some(peer.stable_id().to_owned());
            let receive = picker.0.receive.borrow_mut().take(settings.control);
            Ok(Picked::Machine(Chosen {
                snapshot: snapshot.discovery,
                peer,
                settings,
                receive,
            }))
        }
        Err(Error::Cancelled) => Ok(Picked::Closed),
        Err(error) => Err(PickError::Choice(error)),
    }
}

/// Keyboard handling ahead of dialog navigation. Returns true when consumed.
fn pretranslate(state: &State, message: &MSG) -> bool {
    let controls = state.controls.get();
    let key = message.wParam.0;
    if key == usize::from(VK_ESCAPE.0) {
        state.cancel();
        return true;
    }
    if key == usize::from(VK_F5.0) {
        state.request(Action::Refresh);
        return true;
    }
    if key == usize::from(VK_RETURN.0) {
        // SAFETY: a scalar focus query on this thread.
        let focus = unsafe { GetFocus() };
        if focus == controls.close {
            state.cancel();
        } else if focus == controls.refresh {
            state.request(Action::Refresh);
        } else if focus == controls.install {
            state.request(Action::Install);
        } else if focus == controls.decoder {
            state.request(Action::Store);
        } else if focus == controls.receive {
            state.request(Action::Receive);
        } else if focus == controls.clear_receive {
            state.request(Action::ClearReceive);
        } else {
            state.connect(message.time);
        }
        return true;
    }
    if (key == usize::from(VK_DOWN.0) || key == usize::from(VK_UP.0))
        && message.hwnd == controls.search
    {
        // SAFETY: forwards the original key message to this thread's list.
        unsafe {
            SendMessageW(
                controls.list,
                WM_KEYDOWN,
                Some(message.wParam),
                Some(message.lParam),
            );
        }
        return true;
    }
    // SAFETY: a scalar keyboard-state query.
    let control = unsafe { GetKeyState(i32::from(VK_CONTROL.0)) } < 0;
    if control && key == usize::from(b'F') {
        // SAFETY: focusing this thread's own search box.
        unsafe {
            let _ = SetFocus(Some(controls.search));
        }
        return true;
    }
    false
}

#[allow(
    clippy::too_many_lines,
    reason = "one bounded native window construction"
)]
fn open(request: &Request) -> windows::core::Result<Picker> {
    let (decoder_line, decoder_warning) = request.support.as_ref().map_or_else(
        || (String::new(), None),
        |support| {
            outcome::decoder_status(
                support.gpu_device,
                support.decoder_installed,
                support.gpu_hevc_profile,
                support.adapter.as_deref(),
            )
        },
    );
    let decoder_missing = request
        .support
        .as_ref()
        .is_some_and(|support| support.gpu_device && !support.decoder_installed);
    // SAFETY: every window/control is created, used, and destroyed on this
    // thread. Win32 copies all strings synchronously. Picker owns the root
    // before any fallible child creation, so errors destroy the whole tree.
    unsafe {
        let instance = GetModuleHandleW(None)?;
        let class = w!("FrankenRemoteMachinePicker");
        let wc = WNDCLASSEXW {
            cbSize: u32::try_from(size_of::<WNDCLASSEXW>()).unwrap_or(0),
            lpfnWndProc: Some(window_proc),
            hInstance: instance.into(),
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            hIcon: super::app_icon(false),
            hIconSm: super::app_icon(true),
            hbrBackground: GetSysColorBrush(COLOR_WINDOW),
            lpszClassName: class,
            ..Default::default()
        };
        RegisterClassExW(&raw const wc);
        let dpi = i32::try_from(GetDpiForSystem().clamp(96, 768)).unwrap_or(96);
        let scale = |px: i32| px * dpi / 96;
        let title = match request.mode {
            Mode::Application => w!("FrankenRemote"),
            Mode::Command => w!("Choose a machine \u{2014} FrankenRemote"),
        };
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class,
            title,
            WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            scale(880),
            scale(600),
            None,
            None,
            Some(instance.into()),
            None,
        )?;
        let mut notice = request.notice.clone();
        if let Some(text) = decoder_warning
            && notice.is_none()
        {
            notice = Some(Notice {
                text,
                warning: true,
                sticky: true,
            });
        }
        let picker = Picker(Rc::new(State {
            hwnd,
            mode: request.mode,
            base: request.settings.clone(),
            controls: Cell::new(Controls::default()),
            fonts: Cell::new((HFONT::default(), HFONT::default())),
            brushes: (
                CreateSolidBrush(rgb(0xFF, 0xF4, 0xCE)),
                CreateSolidBrush(rgb(0xE5, 0xF1, 0xFB)),
            ),
            snapshot: RefCell::new(None),
            notice: RefCell::new(notice),
            decoder_line,
            decoder_missing,
            offer_install: request.offer_install && request.mode == Mode::Application,
            selected: RefCell::new(None),
            receive: RefCell::new(ReceiveChoice::default()),
            choosing_receive: Cell::new(false),
            filling: Cell::new(false),
            action: Cell::new(Action::None),
            retiring: Cell::new(false),
            closed: Cell::new(false),
        }));
        ACTIVE.with(|slot| *slot.borrow_mut() = Some(Rc::downgrade(&picker.0)));
        let child =
            |class: PCWSTR, text: PCWSTR, style: WINDOW_STYLE, ex: WINDOW_EX_STYLE, id: usize| {
                CreateWindowExW(
                    ex,
                    class,
                    text,
                    WS_CHILD | WS_VISIBLE | style,
                    0,
                    0,
                    0,
                    0,
                    Some(hwnd),
                    Some(HMENU(id as *mut c_void)),
                    Some(instance.into()),
                    None,
                )
            };
        let none = WINDOW_EX_STYLE(0);
        let static_style = WINDOW_STYLE(STATIC_STYLE);
        let button = WS_TABSTOP | WINDOW_STYLE(BS_PUSHBUTTON as u32);
        let heading = HSTRING::from(format!(
            "{} (port {})",
            match request.mode {
                Mode::Application => "Connect to a machine on your tailnet",
                Mode::Command => "Choose a machine to view",
            },
            request.port,
        ));
        let mut controls = Controls {
            heading: child(
                w!("STATIC"),
                PCWSTR(heading.as_ptr()),
                static_style,
                none,
                0,
            )?,
            subtitle: child(
                w!("STATIC"),
                w!(
                    "Machines come from Tailscale on this PC. FrankenRemote must be hosting on the machine you choose."
                ),
                static_style,
                none,
                0,
            )?,
            notice: child(w!("STATIC"), w!(""), WINDOW_STYLE(0x0080), none, 0)?,
            decoder: child(
                w!("BUTTON"),
                w!("Get the HEVC decoder\u{2026}"),
                button,
                none,
                ID_DECODER,
            )?,
            search: child(
                w!("EDIT"),
                w!(""),
                WS_TABSTOP | WINDOW_STYLE(ES_AUTOHSCROLL as u32),
                WS_EX_CLIENTEDGE,
                ID_SEARCH,
            )?,
            refresh: child(w!("BUTTON"), w!("Refresh"), button, none, ID_REFRESH)?,
            list: child(
                WC_LISTVIEWW,
                w!(""),
                WS_TABSTOP
                    | WINDOW_STYLE(
                        LVS_REPORT | LVS_SINGLESEL | LVS_SHOWSELALWAYS | LVS_NOSORTHEADER,
                    ),
                WS_EX_CLIENTEDGE,
                ID_LIST,
            )?,
            ..Controls::default()
        };
        if request.mode == Mode::Application {
            let radio = WS_TABSTOP | WINDOW_STYLE(BS_AUTORADIOBUTTON as u32);
            let check = WS_TABSTOP | WINDOW_STYLE(BS_AUTOCHECKBOX as u32);
            controls.view = child(
                w!("BUTTON"),
                w!("View only"),
                radio | WS_GROUP,
                none,
                ID_VIEW,
            )?;
            controls.control = child(
                w!("BUTTON"),
                w!("View and control"),
                radio,
                none,
                ID_CONTROL,
            )?;
            controls.clipboard = child(
                w!("BUTTON"),
                w!("Share clipboard text"),
                check | WS_GROUP,
                none,
                ID_CLIPBOARD,
            )?;
            controls.display = child(
                w!("BUTTON"),
                w!("Ask which display to show"),
                check,
                none,
                ID_DISPLAY,
            )?;
            controls.fullscreen = child(
                w!("BUTTON"),
                w!("Full screen at this screen's resolution"),
                check,
                none,
                ID_FULLSCREEN,
            )?;
            controls.shortcuts = child(
                w!("BUTTON"),
                w!("Send Windows shortcuts (Win, Alt+Tab, Ctrl+Esc) there"),
                check,
                none,
                ID_SHORTCUTS,
            )?;
            controls.audio = child(w!("BUTTON"), w!("Play host audio"), check, none, ID_AUDIO)?;
            controls.receive = child(
                w!("BUTTON"),
                w!("Receive files\u{2026}"),
                button,
                none,
                ID_RECEIVE,
            )?;
            controls.clear_receive =
                child(w!("BUTTON"), w!("Clear"), button, none, ID_CLEAR_RECEIVE)?;
            controls.receive_path = child(w!("STATIC"), w!(""), static_style, none, 0)?;
        }
        // Two short lines (counts, decode path) that may wrap: no ellipsis.
        controls.status = child(w!("STATIC"), w!(""), WINDOW_STYLE(0x0080), none, 0)?;
        if picker.0.offer_install {
            controls.install = child(
                w!("BUTTON"),
                w!("Install for this user"),
                button,
                none,
                ID_INSTALL,
            )?;
        }
        controls.connect = child(
            w!("BUTTON"),
            w!("Connect"),
            WS_TABSTOP | WINDOW_STYLE(BS_DEFPUSHBUTTON as u32),
            none,
            ID_CONNECT,
        )?;
        controls.close = child(
            w!("BUTTON"),
            match request.mode {
                Mode::Application => w!("Quit"),
                Mode::Command => w!("Cancel"),
            },
            button,
            none,
            ID_CLOSE,
        )?;
        picker.0.controls.set(controls);
        let cue = HSTRING::from("Search machines (name, system, address)");
        SendMessageW(
            controls.search,
            EM_SETCUEBANNER,
            Some(WPARAM(1)),
            Some(LPARAM(cue.as_ptr() as isize)),
        );
        let _ = SetWindowTheme(controls.list, w!("Explorer"), PCWSTR::null());
        let extended = (LVS_EX_FULLROWSELECT | LVS_EX_DOUBLEBUFFER) as usize;
        SendMessageW(
            controls.list,
            LVM_SETEXTENDEDLISTVIEWSTYLE,
            Some(WPARAM(extended)),
            Some(LPARAM(extended.cast_signed())),
        );
        for (index, (title, width)) in [
            ("Machine", 220),
            ("Status", 100),
            ("System", 160),
            ("Connection", 100),
            ("Tailscale address", 150),
        ]
        .into_iter()
        .enumerate()
        {
            let mut text: Vec<u16> = title.encode_utf16().chain([0]).collect();
            let mut column = LVCOLUMNW {
                mask: LVCF_TEXT | LVCF_WIDTH | LVCF_SUBITEM | LVCF_FMT,
                fmt: LVCFMT_LEFT,
                cx: scale(width),
                pszText: PWSTR(text.as_mut_ptr()),
                iSubItem: i32::try_from(index).unwrap_or(0),
                ..Default::default()
            };
            SendMessageW(
                controls.list,
                LVM_INSERTCOLUMNW,
                Some(WPARAM(index)),
                Some(LPARAM((&raw mut column) as isize)),
            );
        }
        if request.mode == Mode::Application {
            let settings = &request.settings;
            let set = |control: HWND, on: bool| {
                SendMessageW(control, BM_SETCHECK, Some(WPARAM(usize::from(on))), None);
            };
            set(controls.view, !settings.control);
            set(controls.control, settings.control);
            set(controls.clipboard, settings.clipboard);
            set(controls.audio, settings.audio);
            set(controls.display, settings.choose_display);
            set(controls.fullscreen, settings.fullscreen);
            set(controls.shortcuts, settings.shortcuts);
            let _ = EnableWindow(controls.clipboard, settings.control);
            let _ = EnableWindow(controls.shortcuts, settings.control);
            picker.0.show_receive();
        }
        picker.0.apply_fonts();
        picker.0.show_notice();
        picker.0.layout();
        let _ = ShowWindow(hwnd, SW_SHOW);
        Ok(picker)
    }
}

#[allow(clippy::too_many_lines, reason = "one window's message dispatch")]
unsafe extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if let Some(state) = active(hwnd) {
        match message {
            WM_CLOSE => {
                state.cancel();
                return LRESULT(0);
            }
            WM_DESTROY => {
                if !state.retiring.get() {
                    state.receive.borrow_mut().clear();
                }
                if !state.retiring.get()
                    && let Some(snapshot) = state.snapshot.borrow_mut().as_mut()
                {
                    snapshot.choice.refuse(Error::NativeUnavailable);
                }
                return LRESULT(0);
            }
            WM_SIZE => {
                state.layout();
                return LRESULT(0);
            }
            WM_GETMINMAXINFO => {
                let dpi = state.dpi();
                // SAFETY: the system passes a writable MINMAXINFO for this call.
                if let Some(info) = unsafe { (lparam.0 as *mut MINMAXINFO).as_mut() } {
                    info.ptMinTrackSize.x = 640 * dpi / 96;
                    info.ptMinTrackSize.y = 440 * dpi / 96;
                }
                return LRESULT(0);
            }
            WM_DPICHANGED => {
                // SAFETY: the system passes the suggested window rectangle.
                if let Some(rect) = unsafe { (lparam.0 as *const RECT).as_ref() } {
                    // SAFETY: repositioning this thread's own window.
                    let _ = unsafe {
                        SetWindowPos(
                            hwnd,
                            None,
                            rect.left,
                            rect.top,
                            rect.right - rect.left,
                            rect.bottom - rect.top,
                            SWP_NOZORDER | SWP_NOACTIVATE,
                        )
                    };
                }
                state.apply_fonts();
                state.layout();
                return LRESULT(0);
            }
            WM_CTLCOLORSTATIC => {
                return state.colors(HDC(wparam.0 as *mut c_void), HWND(lparam.0 as *mut c_void));
            }
            WM_COMMAND => {
                let id = wparam.0 & 0xffff;
                let notification = (wparam.0 >> 16) & 0xffff;
                let source = HWND(lparam.0 as *mut c_void);
                let controls = state.controls.get();
                if id == ID_SEARCH
                    && notification == EN_CHANGE as usize
                    && source == controls.search
                {
                    state.fill_list();
                    return LRESULT(0);
                }
                if notification == BN_CLICKED as usize {
                    match id {
                        ID_CONNECT if source == controls.connect => {
                            // SAFETY: the native queue timestamp is a value.
                            let queued = unsafe { GetMessageTime() };
                            state.connect(u32::from_ne_bytes(queued.to_ne_bytes()));
                        }
                        ID_CLOSE if source == controls.close => state.cancel(),
                        ID_REFRESH if source == controls.refresh => state.request(Action::Refresh),
                        ID_INSTALL if source == controls.install => state.request(Action::Install),
                        ID_DECODER if source == controls.decoder => state.request(Action::Store),
                        ID_RECEIVE if source == controls.receive => state.request(Action::Receive),
                        ID_CLEAR_RECEIVE if source == controls.clear_receive => {
                            state.request(Action::ClearReceive);
                        }
                        ID_VIEW | ID_CONTROL => {
                            let control = checked(controls.control);
                            // SAFETY: enabling this thread's own checkbox.
                            let _ = unsafe { EnableWindow(controls.clipboard, control) };
                            // SAFETY: as above; shortcuts only reach a controlled host.
                            let _ = unsafe { EnableWindow(controls.shortcuts, control) };
                            if control {
                                state.show_receive();
                            } else {
                                state.clear_receive();
                            }
                        }
                        _ => {}
                    }
                    return LRESULT(0);
                }
            }
            WM_NOTIFY => {
                // SAFETY: WM_NOTIFY's lParam is an NMHDR-prefixed structure
                // owned by the sender for this call.
                let Some(header) = (unsafe { (lparam.0 as *const NMHDR).as_ref() }) else {
                    return LRESULT(0);
                };
                if header.hwndFrom == state.controls.get().list {
                    match header.code {
                        NM_CUSTOMDRAW => {
                            // SAFETY: a list view's NM_CUSTOMDRAW carries a
                            // writable NMLVCUSTOMDRAW for this call.
                            let draw = unsafe { (lparam.0 as *mut NMLVCUSTOMDRAW).as_mut() };
                            if let Some(draw) = draw {
                                return state.custom_draw(draw);
                            }
                        }
                        NM_DBLCLK => {
                            // SAFETY: a list view's NM_DBLCLK is NMITEMACTIVATE.
                            let item = unsafe { (lparam.0 as *const NMITEMACTIVATE).as_ref() }
                                .map_or(-1, |activate| activate.iItem);
                            if item >= 0 {
                                // SAFETY: the native queue timestamp is a value.
                                let queued = unsafe { GetMessageTime() };
                                state.connect(u32::from_ne_bytes(queued.to_ne_bytes()));
                            }
                            return LRESULT(0);
                        }
                        LVN_ITEMCHANGED if !state.filling.get() => {
                            state.selection_changed();
                            return LRESULT(0);
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    // SAFETY: unhandled messages retain their original Win32 parameters.
    unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
}
