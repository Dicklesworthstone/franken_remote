//! The Stop sharing window a Windows host shows while it shares: a small
//! always-on-top window that tells the person at the machine the screen is
//! shared, with a button that stops it (closing the window does too). It runs
//! on its own thread with its own message loop, never takes focus when it
//! appears, and shows no text from a peer.
//!
//! Clipboard sharing is shown subtly instead: an icon in the notification
//! area (tooltip "text clipboard sharing is on"), whose menu (either click)
//! has Turn off clipboard sharing. Its window stays hidden. It still owns the
//! clipboard and answers [`Indicator::responsive`], which then means "the
//! icon is in the notification area and its thread answers".
use std::{
    cell::{Cell, RefCell},
    io, mem,
    sync::mpsc,
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use windows::Win32::{
    Foundation::{
        ERROR_CLASS_ALREADY_EXISTS, GetLastError, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT,
        WPARAM,
    },
    Graphics::Gdi::{COLOR_BTNFACE, CreateFontIndirectW, HBRUSH, HFONT},
    System::LibraryLoader::GetModuleHandleW,
    UI::{
        HiDpi::{
            DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForWindow,
            SetThreadDpiAwarenessContext, SystemParametersInfoForDpi,
        },
        Shell::{
            NIF_ICON, NIF_MESSAGE, NIF_SHOWTIP, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY,
            NIM_SETVERSION, NOTIFYICON_VERSION_4, NOTIFYICONDATAW, Shell_NotifyIconW,
        },
        WindowsAndMessaging::{
            AppendMenuW, BN_CLICKED, BS_PUSHBUTTON, CreatePopupMenu, CreateWindowExW,
            DefWindowProcW, DestroyMenu, DestroyWindow, DispatchMessageW, GetCursorPos,
            GetMessageW, HMENU, HWND_TOPMOST, IDC_ARROW, IDI_INFORMATION, IsWindowVisible,
            LoadCursorW, LoadIconW, MF_GRAYED, MF_SEPARATOR, MF_STRING, MSG, NONCLIENTMETRICSW,
            PostMessageW, PostQuitMessage, RegisterClassExW, RegisterWindowMessageW,
            SMTO_ABORTIFHUNG, SMTO_BLOCK, SPI_GETNONCLIENTMETRICS, SPI_GETWORKAREA,
            SW_SHOWNOACTIVATE, SWP_NOACTIVATE, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS,
            SendMessageTimeoutW, SendMessageW, SetForegroundWindow, SetWindowPos, SetWindowTextW,
            ShowWindow, SystemParametersInfoW, TPM_NONOTIFY, TPM_RETURNCMD, TPM_RIGHTBUTTON,
            TrackPopupMenuEx, TranslateMessage, WINDOW_EX_STYLE, WINDOW_STYLE, WM_APP, WM_CLOSE,
            WM_COMMAND, WM_CONTEXTMENU, WM_DESTROY, WM_NULL, WM_SETFONT, WNDCLASSEXW, WS_BORDER,
            WS_CHILD, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP, WS_TABSTOP,
            WS_VISIBLE,
        },
    },
};
use windows::core::{PCWSTR, w};

/// Posted by [`Indicator::close`]: destroy the window without a stop.
const CLOSE: u32 = WM_APP + 1;
/// The notification-area icon's callback message.
const TRAY_CALLBACK: u32 = WM_APP + 2;
/// Sent by [`Indicator::responsive`] for an icon kind: 1 while it is shown.
const QUERY_SHOWN: u32 = WM_APP + 3;
/// The icon's id within its window, and its menu's Turn off item.
const TRAY_ID: u32 = 1;
const TURN_OFF_ITEM: usize = 10;
/// Version 4 icon events (`NIN_SELECT`, `NIN_KEYSELECT`): a left click or
/// Enter/Space on the icon.
const NIN_SELECT: u32 = 0x400;
const NIN_KEYSELECT: u32 = 0x401;
const STOP_BUTTON: u16 = 1;
const LABEL: u16 = 2;
/// Layout at 96 DPI, scaled to the window's DPI.
const WIDTH: i32 = 400;
/// Room for the longer control text on one line.
const CONTROL_WIDTH: i32 = 470;
const HEIGHT: i32 = 52;
const MARGIN: i32 = 12;
const BUTTON_WIDTH: i32 = 110;
const BUTTON_HEIGHT: i32 = 30;

/// What a window says, and what its button stops.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// The screen is shared; Stop sharing ends the share.
    View,
    /// The screen is shared and a viewer may control it.
    Control,
    /// The controller shares the clipboard; Turn off ends that sharing. A
    /// notification-area icon with a menu, not a window on screen.
    Clipboard,
}

impl Kind {
    /// Shown as a notification-area icon rather than a window.
    const fn icon(self) -> bool {
        matches!(self, Self::Clipboard)
    }

    /// The label, the button, the label after a stop, the width at 96 DPI and
    /// the row counted up from the bottom of the work area.
    const fn layout(self) -> (PCWSTR, PCWSTR, PCWSTR, i32, i32) {
        match self {
            Self::View => (
                w!("This screen is being shared by FrankenRemote."),
                w!("Stop sharing"),
                w!("Stopping the share..."),
                WIDTH,
                0,
            ),
            Self::Control => (
                w!("Shared by FrankenRemote; remote control is on."),
                w!("Stop sharing"),
                w!("Stopping the share..."),
                CONTROL_WIDTH,
                0,
            ),
            Self::Clipboard => (
                w!("Text clipboard sharing is on."),
                w!("Turn off"),
                w!("Clipboard sharing is off."),
                CONTROL_WIDTH,
                1,
            ),
        }
    }
}

thread_local! {
    /// The UI thread's stop callback, taken by the first stop.
    static ON_STOP: RefCell<Option<Box<dyn Fn()>>> = RefCell::new(None);
    /// The UI thread's window kind.
    static KIND: Cell<Kind> = const { Cell::new(Kind::View) };
    /// The UI thread's notification-area icon is registered.
    static ICON_SHOWN: Cell<bool> = const { Cell::new(false) };
    /// Explorer's "TaskbarCreated" broadcast: the icon must be added again.
    static TASKBAR_CREATED: Cell<u32> = const { Cell::new(0) };
}

/// The open window. Dropping it closes the window without a stop.
pub struct Indicator {
    window: isize,
    kind: Kind,
    thread: Option<JoinHandle<bool>>,
    closing: bool,
    result: Option<bool>,
    retain: bool,
}

/// Local setup and native cleanup are separate outcomes. A failed-cleanup
/// result is reported only after the original UI thread has joined.
#[derive(Debug)]
pub enum OpenError {
    Setup(io::Error),
    Cleanup,
}

impl From<io::Error> for OpenError {
    fn from(error: io::Error) -> Self {
        Self::Setup(error)
    }
}

impl Indicator {
    /// Show the Stop sharing window on a new UI thread and return once it is
    /// on screen; with `control` its text says a viewer may also control the
    /// machine. `on_stop` runs on that thread, once, when the person presses
    /// Stop sharing or closes the window.
    pub fn open(control: bool, on_stop: impl Fn() + Send + 'static) -> io::Result<Self> {
        Self::open_kind(if control { Kind::Control } else { Kind::View }, on_stop)
    }

    /// Show a window of `kind`, as [`Indicator::open`] does.
    pub fn open_kind(kind: Kind, on_stop: impl Fn() + Send + 'static) -> io::Result<Self> {
        Self::open_owned(kind, on_stop, false).map_err(|error| match error {
            OpenError::Setup(error) => error,
            OpenError::Cleanup => io::ErrorKind::Other.into(),
        })
    }

    /// Open on a supervised foreign-call worker which must retain native
    /// cleanup even after its caller's wait expires. A setup timeout or Drop
    /// fences/closes the window and keeps that worker until its original UI
    /// thread joins. Never call this from an authority or application UI thread.
    pub fn open_kind_retained(
        kind: Kind,
        on_stop: impl Fn() + Send + 'static,
    ) -> Result<Self, OpenError> {
        Self::open_owned(kind, on_stop, true)
    }

    fn open_owned(
        kind: Kind,
        on_stop: impl Fn() + Send + 'static,
        retain: bool,
    ) -> Result<Self, OpenError> {
        let (opened, receive) = mpsc::channel();
        let (accept, accepted) = mpsc::channel();
        let thread = thread::Builder::new()
            .name("fr-indicator-ui".into())
            .spawn(move || {
                ON_STOP.with(|stop| *stop.borrow_mut() = Some(Box::new(on_stop)));
                KIND.with(|current| current.set(kind));
                match create(kind) {
                    Ok(window) => {
                        if opened.send(Ok(window.0 as isize)).is_ok() && accepted.recv().is_ok() {
                            pump()
                        } else {
                            // SAFETY: the opening caller did not accept this
                            // thread's window. Late creation cannot become an
                            // orphan privacy surface after setup timed out.
                            unsafe { DestroyWindow(window).is_ok() }
                        }
                    }
                    Err(error) => {
                        let _ = opened.send(Err(error));
                        // No admitted window. Thread exit retires any native
                        // window that partial creation attached to this thread.
                        true
                    }
                }
            })?;
        match receive.recv_timeout(Duration::from_secs(3)) {
            Ok(Ok(window)) => {
                if accept.send(()).is_err() {
                    return Err(if thread.join().unwrap_or(false) {
                        OpenError::Setup(io::ErrorKind::BrokenPipe.into())
                    } else {
                        OpenError::Cleanup
                    });
                }
                Ok(Self {
                    window,
                    kind,
                    thread: Some(thread),
                    closing: false,
                    result: None,
                    retain,
                })
            }
            Ok(Err(error)) => Err(if thread.join().unwrap_or(false) {
                OpenError::Setup(error)
            } else {
                OpenError::Cleanup
            }),
            Err(_) => {
                // Deny the exact pending open, including a success queued at
                // the timeout boundary. Its UI thread destroys any late window.
                drop(accept);
                // This is the original supervised native worker, not its
                // caller: keep custody across the caller's bounded reap.
                if retain && !thread.join().unwrap_or(false) {
                    return Err(OpenError::Cleanup);
                }
                Err(OpenError::Setup(io::ErrorKind::TimedOut.into()))
            }
        }
    }

    fn handle(&self) -> HWND {
        HWND(self.window as *mut _)
    }

    /// The window's handle, valid until [`Indicator::close`]. Its UI thread
    /// pumps messages, so it can own the clipboard.
    pub fn window(&self) -> isize {
        self.window
    }

    /// The UI thread answered a message within `timeout` and the indicator
    /// is still shown (the window, or for an icon kind the notification-area
    /// icon): evidence the person can still see and press Stop or Turn off.
    pub fn responsive(&self, timeout: Duration) -> bool {
        let millis = u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX);
        let mut result = 0usize;
        let message = if self.kind.icon() {
            QUERY_SHOWN
        } else {
            WM_NULL
        };
        // SAFETY: a message with no pointers to a window this owner created
        // and has not destroyed; a hung or gone thread returns 0.
        let answered = unsafe {
            SendMessageTimeoutW(
                self.handle(),
                message,
                WPARAM(0),
                LPARAM(0),
                SMTO_ABORTIFHUNG | SMTO_BLOCK,
                millis,
                Some(&raw mut result),
            )
        };
        if self.kind.icon() {
            return answered.0 != 0 && result == 1;
        }
        // SAFETY: a query of the same window.
        answered.0 != 0 && unsafe { IsWindowVisible(self.handle()) }.as_bool()
    }

    /// Close the window (no stop) and wait up to a second for its thread.
    /// True: the UI thread has exited successfully. A retained worker owner
    /// continues holding its thread after this wait; use `begin_close` and
    /// `poll_close` for nonblocking, reportable completion instead.
    pub fn close(mut self) -> bool {
        self.begin_close();
        let until = Instant::now() + Duration::from_secs(1);
        loop {
            if let Some(result) = self.poll_close() {
                return result;
            }
            if Instant::now() >= until {
                return false;
            }
            thread::sleep(Duration::from_millis(5));
        }
    }

    /// Request native closure exactly once, without waiting for it or running
    /// the user's Stop callback. No retry or replacement lifetime is created.
    pub fn begin_close(&mut self) {
        if !std::mem::replace(&mut self.closing, true) {
            self.post_close();
        }
    }

    /// Finish cleanup on the supervised native worker, retaining the original
    /// UI thread across any caller timeout. No authority/UI thread may wait
    /// here. Unlike the compatibility `close` wait, this returns the actual
    /// joined native outcome, never a timeout mistaken for that outcome.
    pub fn finish_close(mut self) -> bool {
        self.begin_close();
        loop {
            if let Some(result) = self.poll_close() {
                return result;
            }
            thread::park_timeout(Duration::from_millis(5));
        }
    }

    /// Nonblocking inspection of this same UI thread. Pending retains the
    /// original handle; a completed failure is cached and cannot become success.
    pub fn poll_close(&mut self) -> Option<bool> {
        if let Some(result) = self.result {
            return Some(result);
        }
        if !self.closing || !self.thread.as_ref()?.is_finished() {
            return None;
        }
        let result = self.thread.take()?.join().unwrap_or(false);
        self.result = Some(result);
        Some(result)
    }

    fn post_close(&self) {
        // SAFETY: posting a private message carries no pointers; a window
        // already destroyed fails harmlessly.
        let _ = unsafe { PostMessageW(Some(self.handle()), CLOSE, WPARAM(0), LPARAM(0)) };
    }
}

impl Drop for Indicator {
    fn drop(&mut self) {
        if self.thread.is_some() {
            self.begin_close();
            if self.retain {
                while self.poll_close().is_none() {
                    thread::park_timeout(Duration::from_millis(5));
                }
            }
        }
    }
}

fn last_error() -> io::Error {
    io::Error::last_os_error()
}

fn create(kind: Kind) -> io::Result<HWND> {
    let (_, _, _, base_width, row) = kind.layout();
    // SAFETY: Win32 window creation on this thread with owned, static or
    // stack-local arguments; every handle stays with this thread's window.
    unsafe {
        let _ = SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let instance: HINSTANCE = GetModuleHandleW(PCWSTR::null())?.into();
        let class = WNDCLASSEXW {
            cbSize: u32::try_from(mem::size_of::<WNDCLASSEXW>()).unwrap_or(0),
            lpfnWndProc: Some(window_proc),
            hInstance: instance,
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            hbrBackground: HBRUSH((COLOR_BTNFACE.0 + 1) as isize as *mut _),
            lpszClassName: w!("FrankenRemoteIndicator"),
            ..Default::default()
        };
        if RegisterClassExW(&raw const class) == 0 && GetLastError() != ERROR_CLASS_ALREADY_EXISTS {
            return Err(last_error());
        }
        let window = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            w!("FrankenRemoteIndicator"),
            w!("FrankenRemote"),
            WS_POPUP | WS_BORDER,
            0,
            0,
            base_width,
            HEIGHT,
            None,
            None,
            Some(instance),
            None,
        )?;
        let dpi = i32::try_from(GetDpiForWindow(window)).unwrap_or(96).max(96);
        let scale = |value: i32| value * dpi / 96;
        let (width, height) = (scale(base_width), scale(HEIGHT));
        // Bottom right of the primary display's work area, above the taskbar;
        // a later row stacks above the first.
        let mut area = RECT::default();
        SystemParametersInfoW(
            SPI_GETWORKAREA,
            0,
            Some((&raw mut area).cast()),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS::default(),
        )?;
        SetWindowPos(
            window,
            Some(HWND_TOPMOST),
            area.right - width - scale(MARGIN),
            area.bottom - (row + 1) * (height + scale(MARGIN)),
            width,
            height,
            SWP_NOACTIVATE,
        )?;
        controls(window, instance, dpi, (width, height), kind)?;
        if kind.icon() {
            // The window stays hidden (it owns the clipboard and keeps the
            // state); the person sees only the notification-area icon.
            TASKBAR_CREATED.with(|id| id.set(RegisterWindowMessageW(w!("TaskbarCreated"))));
            if let Err(error) = add_icon(window, instance) {
                let _ = DestroyWindow(window);
                return Err(error);
            }
        } else {
            let _ = ShowWindow(window, SW_SHOWNOACTIVATE);
        }
        Ok(window)
    }
}

/// The text shown while hovering the icon (at most 127 characters).
const ICON_TIP: &str = "FrankenRemote: text clipboard sharing is on. Click to turn it off.";
const ICON_TIP_OFF: &str = "FrankenRemote: clipboard sharing is off.";

fn icon_data(window: HWND) -> NOTIFYICONDATAW {
    NOTIFYICONDATAW {
        cbSize: u32::try_from(mem::size_of::<NOTIFYICONDATAW>()).unwrap_or(0),
        hWnd: window,
        uID: TRAY_ID,
        ..Default::default()
    }
}

fn set_tip(data: &mut NOTIFYICONDATAW, tip: &str) {
    for (slot, unit) in data
        .szTip
        .iter_mut()
        .zip(tip.encode_utf16().take(127).chain(std::iter::once(0)))
    {
        *slot = unit;
    }
}

/// Add this window's notification-area icon: the application's own icon
/// (resource 1) when it has one, else the system's information icon.
fn add_icon(window: HWND, instance: HINSTANCE) -> io::Result<()> {
    let mut data = icon_data(window);
    data.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP | NIF_SHOWTIP;
    data.uCallbackMessage = TRAY_CALLBACK;
    // SAFETY: resource and stock icon loads; an integer name is
    // MAKEINTRESOURCE(1), never dereferenced as a string.
    data.hIcon = unsafe {
        LoadIconW(Some(instance), PCWSTR(std::ptr::without_provenance(1)))
            .or_else(|_| LoadIconW(None, IDI_INFORMATION))
            .map_err(io::Error::from)?
    };
    set_tip(&mut data, ICON_TIP);
    // SAFETY: a fully initialized structure naming this thread's own window.
    unsafe {
        if !Shell_NotifyIconW(NIM_ADD, &raw const data).as_bool() {
            return Err(last_error());
        }
        data.Anonymous.uVersion = NOTIFYICON_VERSION_4;
        let _ = Shell_NotifyIconW(NIM_SETVERSION, &raw const data);
    }
    ICON_SHOWN.with(|shown| shown.set(true));
    Ok(())
}

fn remove_icon(window: HWND) {
    if ICON_SHOWN.with(|shown| shown.replace(false)) {
        let data = icon_data(window);
        // SAFETY: deleting this thread's own icon by window and id.
        let _ = unsafe { Shell_NotifyIconW(NIM_DELETE, &raw const data) };
    }
}

/// The icon's menu, at the pointer: what is shared, and Turn off.
fn icon_menu(window: HWND) {
    // SAFETY: a popup menu owned and destroyed here, shown for this thread's
    // own window; the menu loop runs on this thread and still answers
    // messages sent to the window.
    unsafe {
        let Ok(menu) = CreatePopupMenu() else {
            return;
        };
        let stopped = ON_STOP.with(|stop| stop.borrow().is_none());
        let status = if stopped {
            w!("Clipboard sharing is off")
        } else {
            w!("Text clipboard sharing is on")
        };
        let _ = AppendMenuW(menu, MF_STRING | MF_GRAYED, 0, status);
        if !stopped {
            let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
            let _ = AppendMenuW(
                menu,
                MF_STRING,
                TURN_OFF_ITEM,
                w!("Turn off clipboard sharing"),
            );
        }
        let mut point = POINT::default();
        let _ = GetCursorPos(&raw mut point);
        // Required for a notification-area menu to close when clicked away.
        let _ = SetForegroundWindow(window);
        let chosen = TrackPopupMenuEx(
            menu,
            (TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_NONOTIFY).0,
            point.x,
            point.y,
            window,
            None,
        );
        let _ = PostMessageW(Some(window), WM_NULL, WPARAM(0), LPARAM(0));
        let _ = DestroyMenu(menu);
        if usize::try_from(chosen.0).ok() == Some(TURN_OFF_ITEM) {
            stop(window);
        }
    }
}

/// The label and the button, in the system's message font.
fn controls(
    window: HWND,
    instance: HINSTANCE,
    dpi: i32,
    size: (i32, i32),
    kind: Kind,
) -> io::Result<()> {
    let (text, button_text, _, _, _) = kind.layout();
    let scale = |value: i32| value * dpi / 96;
    let (width, height) = size;
    // SAFETY: child windows of this thread's own window, static strings.
    unsafe {
        let font = message_font(u32::try_from(dpi).unwrap_or(96));
        let child = |class: PCWSTR, text: PCWSTR, style: WINDOW_STYLE, id: u16, rect: [i32; 4]| {
            CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                class,
                text,
                WS_CHILD | WS_VISIBLE | style,
                rect[0],
                rect[1],
                rect[2],
                rect[3],
                Some(window),
                Some(HMENU(usize::from(id) as *mut _)),
                Some(instance),
                None,
            )
        };
        let button_width = scale(BUTTON_WIDTH);
        let label = child(
            w!("STATIC"),
            text,
            WINDOW_STYLE::default(),
            LABEL,
            [
                scale(MARGIN),
                (height - scale(18)) / 2,
                width - button_width - scale(3 * MARGIN),
                scale(20),
            ],
        )?;
        let button = child(
            w!("BUTTON"),
            button_text,
            WS_TABSTOP | WINDOW_STYLE(BS_PUSHBUTTON as u32),
            STOP_BUTTON,
            [
                width - button_width - scale(MARGIN),
                (height - scale(BUTTON_HEIGHT)) / 2,
                button_width,
                scale(BUTTON_HEIGHT),
            ],
        )?;
        if let Some(font) = font {
            for control in [label, button] {
                SendMessageW(
                    control,
                    WM_SETFONT,
                    Some(WPARAM(font.0 as usize)),
                    Some(LPARAM(1)),
                );
            }
        }
        Ok(())
    }
}

/// The system's message font at `dpi`. Never deleted: a control may still use
/// it, and a host opens one window per run.
fn message_font(dpi: u32) -> Option<HFONT> {
    let mut metrics = NONCLIENTMETRICSW {
        cbSize: u32::try_from(mem::size_of::<NONCLIENTMETRICSW>()).unwrap_or(0),
        ..Default::default()
    };
    // SAFETY: a correctly sized out-structure for this query.
    unsafe {
        SystemParametersInfoForDpi(
            SPI_GETNONCLIENTMETRICS.0,
            metrics.cbSize,
            Some((&raw mut metrics).cast()),
            0,
            dpi,
        )
        .ok()?;
        let font = CreateFontIndirectW(&raw const metrics.lfMessageFont);
        (!font.is_invalid()).then_some(font)
    }
}

fn pump() -> bool {
    let mut message = MSG::default();
    // SAFETY: the standard loop over this thread's queue; it ends when the
    // window is destroyed (WM_QUIT) or the queue fails.
    unsafe {
        loop {
            let result = GetMessageW(&raw mut message, None, 0, 0).0;
            if result <= 0 {
                return result == 0;
            }
            let _ = TranslateMessage(&raw const message);
            DispatchMessageW(&raw const message);
        }
    }
}

/// The first button press or close runs the callback; the window stays until
/// the host closes it, saying what stopped.
fn stop(window: HWND) {
    if let Some(on_stop) = ON_STOP.with(|stop| stop.borrow_mut().take()) {
        on_stop();
        let kind = KIND.with(Cell::get);
        let (_, _, stopped, _, _) = kind.layout();
        // SAFETY: a child of this thread's live window; a static string.
        unsafe {
            if let Ok(label) =
                windows::Win32::UI::WindowsAndMessaging::GetDlgItem(Some(window), i32::from(LABEL))
            {
                let _ = SetWindowTextW(label, stopped);
            }
        }
        if kind.icon() && ICON_SHOWN.with(Cell::get) {
            let mut data = icon_data(window);
            data.uFlags = NIF_TIP | NIF_SHOWTIP;
            set_tip(&mut data, ICON_TIP_OFF);
            // SAFETY: modifying this thread's own icon by window and id.
            let _ = unsafe { Shell_NotifyIconW(NIM_MODIFY, &raw const data) };
        }
    }
}

extern "system" fn window_proc(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let low = u16::try_from(wparam.0 & 0xffff).unwrap_or(0);
    let high = u32::try_from((wparam.0 >> 16) & 0xffff).unwrap_or(u32::MAX);
    match message {
        WM_COMMAND if low == STOP_BUTTON && high == BN_CLICKED => {
            stop(window);
            LRESULT(0)
        }
        WM_CLOSE => {
            stop(window);
            LRESULT(0)
        }
        TRAY_CALLBACK => {
            // Version 4: the event is the low word of lParam.
            let event = u32::try_from(lparam.0 & 0xffff).unwrap_or(0);
            if matches!(event, WM_CONTEXTMENU | NIN_SELECT | NIN_KEYSELECT) {
                icon_menu(window);
            }
            LRESULT(0)
        }
        QUERY_SHOWN => LRESULT(isize::from(ICON_SHOWN.with(Cell::get))),
        CLOSE => {
            remove_icon(window);
            // SAFETY: this thread's own window.
            let _ = unsafe { DestroyWindow(window) };
            LRESULT(0)
        }
        WM_DESTROY => {
            remove_icon(window);
            // SAFETY: ends this thread's message loop.
            unsafe { PostQuitMessage(0) };
            LRESULT(0)
        }
        // Explorer restarted: its notification area forgot the icon.
        _ if message != 0
            && message == TASKBAR_CREATED.with(Cell::get)
            && ICON_SHOWN.with(|shown| shown.replace(false)) =>
        {
            // SAFETY: the module handle of this process.
            let added = unsafe { GetModuleHandleW(PCWSTR::null()) }
                .is_ok_and(|module| add_icon(window, module.into()).is_ok());
            if !added {
                // Not shown any more: responsive() now says so, and the
                // owner stops clipboard sharing.
                ICON_SHOWN.with(|shown| shown.set(false));
            }
            LRESULT(0)
        }
        // SAFETY: default handling with the arguments Windows passed.
        _ => unsafe { DefWindowProcW(window, message, wparam, lparam) },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    fn label(indicator: &Indicator) -> String {
        use windows::Win32::UI::WindowsAndMessaging::{GetDlgItem, GetWindowTextW};
        let mut text = [0_u16; 128];
        // SAFETY: a child of this test's live window and a writable buffer.
        let length = unsafe {
            let label = GetDlgItem(Some(indicator.handle()), i32::from(LABEL)).unwrap();
            GetWindowTextW(label, &mut text)
        };
        String::from_utf16_lossy(&text[..usize::try_from(length).unwrap()])
    }

    #[test]
    #[ignore = "shows a window on the interactive desktop"]
    fn retained_close_polls_the_original_ui_and_caches_its_result() {
        let stops = Arc::new(AtomicUsize::new(0));
        let counted = stops.clone();
        let mut indicator = Indicator::open_kind_retained(Kind::Clipboard, move || {
            counted.fetch_add(1, Ordering::SeqCst);
        })
        .unwrap();
        assert!(indicator.responsive(Duration::from_millis(250)));
        assert_eq!(indicator.poll_close(), None);
        indicator.begin_close();
        indicator.begin_close();
        let until = Instant::now() + Duration::from_secs(2);
        loop {
            if let Some(result) = indicator.poll_close() {
                assert!(result);
                break;
            }
            assert!(Instant::now() < until);
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(indicator.poll_close(), Some(true));
        assert!(indicator.thread.is_none());
        assert_eq!(stops.load(Ordering::SeqCst), 0);
    }

    /// Needs the interactive desktop (run in the logged-on session): the
    /// window opens and answers; a click on Stop sharing (sent the way a
    /// press arrives) runs the callback once, a close message counts as a
    /// stop too, and closing from the owner runs none.
    #[test]
    #[ignore = "shows a window on the interactive desktop"]
    fn the_window_answers_stops_once_and_closes_without_a_stop() {
        let stops = Arc::new(AtomicUsize::new(0));
        let counted = stops.clone();
        let indicator = Indicator::open(true, move || {
            counted.fetch_add(1, Ordering::SeqCst);
        })
        .unwrap();
        assert!(indicator.responsive(Duration::from_millis(250)));
        assert_eq!(
            label(&indicator),
            "Shared by FrankenRemote; remote control is on."
        );
        let click = WPARAM((BN_CLICKED as usize) << 16 | usize::from(STOP_BUTTON));
        for _ in 0..2 {
            // SAFETY: test-only messages to this test's own window.
            unsafe { PostMessageW(Some(indicator.handle()), WM_COMMAND, click, LPARAM(0)) }
                .unwrap();
        }
        // SAFETY: as above.
        unsafe { PostMessageW(Some(indicator.handle()), WM_CLOSE, WPARAM(0), LPARAM(0)) }.unwrap();
        let until = Instant::now() + Duration::from_secs(1);
        while stops.load(Ordering::SeqCst) == 0 && Instant::now() < until {
            thread::sleep(Duration::from_millis(5));
        }
        // The window is still up after a stop: the host closes it.
        assert!(indicator.responsive(Duration::from_millis(250)));
        assert_eq!(stops.load(Ordering::SeqCst), 1);
        let handle = indicator.handle();
        assert!(indicator.close());
        assert_eq!(stops.load(Ordering::SeqCst), 1);
        // SAFETY: a query of a destroyed window's former handle.
        assert!(!unsafe { IsWindowVisible(handle) }.as_bool());

        // Closed by its owner without a press: no stop at all.
        let quiet = Arc::new(AtomicUsize::new(0));
        let counted = quiet.clone();
        let indicator = Indicator::open(false, move || {
            counted.fetch_add(1, Ordering::SeqCst);
        })
        .unwrap();
        assert_eq!(
            label(&indicator),
            "This screen is being shared by FrankenRemote."
        );
        assert!(indicator.close());
        assert_eq!(quiet.load(Ordering::SeqCst), 0);
    }

    /// The clipboard window: its own text and button, one row above the
    /// Stop sharing window, says sharing is off after Turn off, and its
    /// handle can own a clipboard write while its thread pumps.
    #[test]
    #[ignore = "shows a window on the interactive desktop and replaces the clipboard"]
    fn the_clipboard_window_turns_off_once_and_can_own_the_clipboard() {
        use windows::Win32::UI::WindowsAndMessaging::{GetDlgItem, GetWindowRect, GetWindowTextW};
        let share = Indicator::open(true, || {}).unwrap();
        let stops = Arc::new(AtomicUsize::new(0));
        let counted = stops.clone();
        let indicator = Indicator::open_kind(Kind::Clipboard, move || {
            counted.fetch_add(1, Ordering::SeqCst);
        })
        .unwrap();
        assert!(indicator.responsive(Duration::from_millis(250)));
        assert_eq!(label(&indicator), "Text clipboard sharing is on.");
        let mut text = [0_u16; 32];
        // SAFETY: a child of this test's live window and a writable buffer.
        let length = unsafe {
            GetWindowTextW(
                GetDlgItem(Some(indicator.handle()), i32::from(STOP_BUTTON)).unwrap(),
                &mut text,
            )
        };
        assert_eq!(
            String::from_utf16_lossy(&text[..usize::try_from(length).unwrap()]),
            "Turn off"
        );
        let (mut upper, mut lower) = (RECT::default(), RECT::default());
        // SAFETY: queries of this test's live windows.
        unsafe {
            GetWindowRect(indicator.handle(), &raw mut upper).unwrap();
            GetWindowRect(share.handle(), &raw mut lower).unwrap();
        }
        assert!(upper.bottom <= lower.top, "the clipboard window sits above");

        let written = crate::clipboard::write_text(
            indicator.window(),
            "owned",
            crate::clipboard::sequence(),
            Duration::from_millis(500),
            || true,
        )
        .unwrap();
        assert_eq!(crate::clipboard::sequence(), written);
        assert!(crate::clipboard::owned_by(indicator.window()));

        let click = WPARAM((BN_CLICKED as usize) << 16 | usize::from(STOP_BUTTON));
        for _ in 0..2 {
            // SAFETY: test-only messages to this test's own window.
            unsafe { PostMessageW(Some(indicator.handle()), WM_COMMAND, click, LPARAM(0)) }
                .unwrap();
        }
        let until = Instant::now() + Duration::from_secs(1);
        while stops.load(Ordering::SeqCst) == 0 && Instant::now() < until {
            thread::sleep(Duration::from_millis(5));
        }
        assert!(indicator.responsive(Duration::from_millis(250)));
        assert_eq!(stops.load(Ordering::SeqCst), 1);
        assert_eq!(label(&indicator), "Clipboard sharing is off.");
        assert!(indicator.close());
        assert!(share.close());
    }

    /// A UI thread that stops answering (here: held in its stop callback)
    /// fails the responsiveness check, and passes again once it returns.
    #[test]
    #[ignore = "shows a window on the interactive desktop"]
    fn a_hung_ui_thread_is_not_responsive() {
        let indicator =
            Indicator::open(false, || thread::sleep(Duration::from_millis(800))).unwrap();
        assert!(indicator.responsive(Duration::from_millis(250)));
        let click = WPARAM((BN_CLICKED as usize) << 16 | usize::from(STOP_BUTTON));
        // SAFETY: a test-only message to this test's own window.
        unsafe { PostMessageW(Some(indicator.handle()), WM_COMMAND, click, LPARAM(0)) }.unwrap();
        thread::sleep(Duration::from_millis(100));
        assert!(!indicator.responsive(Duration::from_millis(250)));
        thread::sleep(Duration::from_millis(800));
        assert!(indicator.responsive(Duration::from_millis(250)));
        assert!(indicator.close());
    }
}
