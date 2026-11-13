//! The Stop sharing window a Windows host shows while it shares: a small
//! always-on-top window that tells the person at the machine the screen is
//! shared, with a button that stops it (closing the window does too). It runs
//! on its own thread with its own message loop, never takes focus when it
//! appears, and shows no text from a peer.
use std::{
    cell::RefCell,
    io, mem,
    sync::mpsc,
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use windows::Win32::{
    Foundation::{
        ERROR_CLASS_ALREADY_EXISTS, GetLastError, HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM,
    },
    Graphics::Gdi::{COLOR_BTNFACE, CreateFontIndirectW, HBRUSH, HFONT},
    System::LibraryLoader::GetModuleHandleW,
    UI::{
        HiDpi::{
            DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForWindow,
            SetThreadDpiAwarenessContext, SystemParametersInfoForDpi,
        },
        WindowsAndMessaging::{
            BN_CLICKED, BS_PUSHBUTTON, CreateWindowExW, DefWindowProcW, DestroyWindow,
            DispatchMessageW, GetMessageW, HMENU, HWND_TOPMOST, IDC_ARROW, IsWindowVisible,
            LoadCursorW, MSG, NONCLIENTMETRICSW, PostMessageW, PostQuitMessage, RegisterClassExW,
            SMTO_ABORTIFHUNG, SMTO_BLOCK, SPI_GETNONCLIENTMETRICS, SPI_GETWORKAREA,
            SW_SHOWNOACTIVATE, SWP_NOACTIVATE, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS,
            SendMessageTimeoutW, SendMessageW, SetWindowPos, SetWindowTextW, ShowWindow,
            SystemParametersInfoW, TranslateMessage, WINDOW_EX_STYLE, WINDOW_STYLE, WM_APP,
            WM_CLOSE, WM_COMMAND, WM_DESTROY, WM_NULL, WM_SETFONT, WNDCLASSEXW, WS_BORDER,
            WS_CHILD, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP, WS_TABSTOP,
            WS_VISIBLE,
        },
    },
};
use windows::core::{PCWSTR, w};

/// Posted by [`Indicator::close`]: destroy the window without a stop.
const CLOSE: u32 = WM_APP + 1;
const STOP_BUTTON: u16 = 1;
const LABEL: u16 = 2;
/// Layout at 96 DPI, scaled to the window's DPI.
const WIDTH: i32 = 400;
const HEIGHT: i32 = 52;
const MARGIN: i32 = 12;
const BUTTON_WIDTH: i32 = 110;
const BUTTON_HEIGHT: i32 = 30;

thread_local! {
    /// The UI thread's stop callback, taken by the first stop.
    static ON_STOP: RefCell<Option<Box<dyn Fn()>>> = RefCell::new(None);
}

/// The open window. Dropping it closes the window without a stop.
pub struct Indicator {
    window: isize,
    thread: Option<JoinHandle<()>>,
}

impl Indicator {
    /// Show the window on a new UI thread and return once it is on screen.
    /// `on_stop` runs on that thread, once, when the person presses Stop
    /// sharing or closes the window.
    pub fn open(on_stop: impl Fn() + Send + 'static) -> io::Result<Self> {
        let (opened, receive) = mpsc::channel();
        let thread = thread::Builder::new()
            .name("fr-indicator-ui".into())
            .spawn(move || {
                ON_STOP.with(|stop| *stop.borrow_mut() = Some(Box::new(on_stop)));
                match create() {
                    Ok(window) => {
                        let _ = opened.send(Ok(window.0 as isize));
                        pump();
                    }
                    Err(error) => {
                        let _ = opened.send(Err(error));
                    }
                }
            })?;
        match receive.recv_timeout(Duration::from_secs(3)) {
            Ok(Ok(window)) => Ok(Self {
                window,
                thread: Some(thread),
            }),
            Ok(Err(error)) => {
                let _ = thread.join();
                Err(error)
            }
            Err(_) => Err(io::ErrorKind::TimedOut.into()),
        }
    }

    fn handle(&self) -> HWND {
        HWND(self.window as *mut _)
    }

    /// The UI thread answered a message within `timeout` and the window is
    /// still shown: evidence the person can still see and press Stop.
    pub fn responsive(&self, timeout: Duration) -> bool {
        let millis = u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX);
        let mut result = 0usize;
        // SAFETY: a message with no pointers to a window this owner created
        // and has not destroyed; a hung or gone thread returns 0.
        let answered = unsafe {
            SendMessageTimeoutW(
                self.handle(),
                WM_NULL,
                WPARAM(0),
                LPARAM(0),
                SMTO_ABORTIFHUNG | SMTO_BLOCK,
                millis,
                Some(&raw mut result),
            )
        };
        // SAFETY: a query of the same window.
        answered.0 != 0 && unsafe { IsWindowVisible(self.handle()) }.as_bool()
    }

    /// Close the window (no stop) and wait up to a second for its thread.
    /// True: the UI thread has exited.
    pub fn close(mut self) -> bool {
        self.post_close();
        let Some(thread) = self.thread.take() else {
            return true;
        };
        let until = Instant::now() + Duration::from_secs(1);
        while !thread.is_finished() && Instant::now() < until {
            thread::sleep(Duration::from_millis(5));
        }
        thread.is_finished() && thread.join().is_ok()
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
            self.post_close();
        }
    }
}

fn last_error() -> io::Error {
    io::Error::last_os_error()
}

fn create() -> io::Result<HWND> {
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
            WIDTH,
            HEIGHT,
            None,
            None,
            Some(instance),
            None,
        )?;
        let dpi = i32::try_from(GetDpiForWindow(window)).unwrap_or(96).max(96);
        let scale = |value: i32| value * dpi / 96;
        let (width, height) = (scale(WIDTH), scale(HEIGHT));
        // Bottom right of the primary display's work area, above the taskbar.
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
            area.bottom - height - scale(MARGIN),
            width,
            height,
            SWP_NOACTIVATE,
        )?;
        controls(window, instance, dpi, (width, height))?;
        let _ = ShowWindow(window, SW_SHOWNOACTIVATE);
        Ok(window)
    }
}

/// The label and the Stop sharing button, in the system's message font.
fn controls(window: HWND, instance: HINSTANCE, dpi: i32, size: (i32, i32)) -> io::Result<()> {
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
            w!("This screen is being shared by FrankenRemote."),
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
            w!("Stop sharing"),
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

fn pump() {
    let mut message = MSG::default();
    // SAFETY: the standard loop over this thread's queue; it ends when the
    // window is destroyed (WM_QUIT) or the queue fails.
    unsafe {
        while GetMessageW(&raw mut message, None, 0, 0).0 > 0 {
            let _ = TranslateMessage(&raw const message);
            DispatchMessageW(&raw const message);
        }
    }
}

/// The first Stop sharing press or close runs the callback; the window stays
/// until the host closes it, saying the share is stopping.
fn stop(window: HWND) {
    if let Some(on_stop) = ON_STOP.with(|stop| stop.borrow_mut().take()) {
        on_stop();
        // SAFETY: a child of this thread's live window; a static string.
        unsafe {
            if let Ok(label) =
                windows::Win32::UI::WindowsAndMessaging::GetDlgItem(Some(window), i32::from(LABEL))
            {
                let _ = SetWindowTextW(label, w!("Stopping the share..."));
            }
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
        CLOSE => {
            // SAFETY: this thread's own window.
            let _ = unsafe { DestroyWindow(window) };
            LRESULT(0)
        }
        WM_DESTROY => {
            // SAFETY: ends this thread's message loop.
            unsafe { PostQuitMessage(0) };
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

    /// Needs the interactive desktop (run in the logged-on session): the
    /// window opens and answers; a click on Stop sharing (sent the way a
    /// press arrives) runs the callback once, a close message counts as a
    /// stop too, and closing from the owner runs none.
    #[test]
    #[ignore = "shows a window on the interactive desktop"]
    fn the_window_answers_stops_once_and_closes_without_a_stop() {
        let stops = Arc::new(AtomicUsize::new(0));
        let counted = stops.clone();
        let indicator = Indicator::open(move || {
            counted.fetch_add(1, Ordering::SeqCst);
        })
        .unwrap();
        assert!(indicator.responsive(Duration::from_millis(250)));
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
        let indicator = Indicator::open(move || {
            counted.fetch_add(1, Ordering::SeqCst);
        })
        .unwrap();
        assert!(indicator.close());
        assert_eq!(quiet.load(Ordering::SeqCst), 0);
    }

    /// A UI thread that stops answering (here: held in its stop callback)
    /// fails the responsiveness check, and passes again once it returns.
    #[test]
    #[ignore = "shows a window on the interactive desktop"]
    fn a_hung_ui_thread_is_not_responsive() {
        let indicator = Indicator::open(|| thread::sleep(Duration::from_millis(800))).unwrap();
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
