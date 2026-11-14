//! The Windows clipboard's Unicode text, for frd's Windows host: change
//! metadata without opening the clipboard (the window station's sequence
//! number, and whether Unicode text is offered), a bounded read, and a
//! compare-and-replace write. Clipboard text never appears in errors or
//! Debug output here.
//!
//! `OpenClipboard` is exclusive: while this thread holds the clipboard open,
//! no other program can change it. So a sequence number checked after opening
//! cannot race another copy before this thread's own read or write. Another
//! program may hold the clipboard open briefly itself; opening retries until
//! the caller's bound, sleeping between attempts.
//!
//! A write names an owner window of this process, and only emptying the
//! clipboard changes its owner. Closing after a write adds the formats Windows
//! synthesizes from Unicode text (measured: three more changes, inside
//! `CloseClipboard`), so a write reports the number read after closing, and
//! only while its window still owns the clipboard: that number then describes
//! this write's text, not another program's copy. Windows sends the owner
//! `WM_DESTROYCLIPBOARD` when another program empties the clipboard, so the
//! owner's thread must keep pumping messages.
use core::fmt;
use std::{
    thread,
    time::{Duration, Instant},
};
use windows::Win32::{
    Foundation::{GlobalFree, HANDLE, HGLOBAL, HWND},
    System::{
        DataExchange::{
            CloseClipboard, EmptyClipboard, GetClipboardData, GetClipboardOwner,
            GetClipboardSequenceNumber, IsClipboardFormatAvailable, OpenClipboard,
            SetClipboardData,
        },
        Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock},
    },
};

/// `CF_UNICODETEXT`: UTF-16 text ending in a NUL unit. Windows synthesizes
/// it from the ANSI and OEM text formats, so every text copy offers it.
const UNICODE_TEXT: u32 = 13;
/// Between attempts to open a clipboard another program holds.
const RETRY: Duration = Duration::from_millis(1);

/// The window station's clipboard sequence number. It changes whenever the
/// clipboard is emptied or its contents change. Opening is not needed.
pub fn sequence() -> u32 {
    // SAFETY: no arguments; reads the window station's counter.
    unsafe { GetClipboardSequenceNumber() }
}

/// Whether the clipboard offers Unicode text now. Opening is not needed.
pub fn has_text() -> bool {
    // SAFETY: a format query with no pointers.
    unsafe { IsClipboardFormatAvailable(UNICODE_TEXT) }.is_ok()
}

/// Whether `owner` (a window handle) owns the clipboard: it was the window
/// named by the last program to empty it.
pub fn owned_by(owner: isize) -> bool {
    // SAFETY: no arguments; returns a handle value that is only compared.
    unsafe { GetClipboardOwner() }.is_ok_and(|window| window.0 as isize == owner)
}

/// Why a clipboard operation did not complete. Content-free.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// Another program held the clipboard open for the whole wait.
    Busy,
    /// Once open, the sequence number was not the expected one: the
    /// clipboard changed since the caller looked.
    Changed,
    /// The clipboard offers no Unicode text.
    NoText,
    /// The text is longer than the caller's limit in UTF-8 bytes.
    Limit,
    /// The text is not valid UTF-16 (an unpaired surrogate).
    Invalid,
    /// The caller's last check, run while open, refused the change.
    Refused,
    /// A system call failed.
    Unavailable,
}

/// Valid UTF-8 read from the clipboard. Zeroed when dropped; its Debug output
/// never shows the text.
pub struct Text(Vec<u8>);

impl Text {
    pub fn as_str(&self) -> &str {
        std::str::from_utf8(&self.0).expect("built from decoded UTF-16")
    }
}

impl AsRef<str> for Text {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl fmt::Debug for Text {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ClipboardText([redacted])")
    }
}

impl Drop for Text {
    fn drop(&mut self) {
        for byte in &mut self.0 {
            // SAFETY: an exclusive reference to a live byte; the volatile store
            // keeps the erasure from being optimized away before the free.
            unsafe { std::ptr::write_volatile(byte, 0) };
        }
    }
}

/// UTF-16 units for a write, zeroed when dropped.
struct Wide(Vec<u16>);

impl Drop for Wide {
    fn drop(&mut self) {
        for unit in &mut self.0 {
            // SAFETY: as for `Text`.
            unsafe { std::ptr::write_volatile(unit, 0) };
        }
    }
}

/// The clipboard, open on this thread until dropped.
struct Open;

impl Open {
    fn new(owner: Option<HWND>, wait: Duration) -> Result<Self, Error> {
        let until = Instant::now() + wait;
        loop {
            // SAFETY: `owner` is None or a window of this process; the
            // clipboard is closed again by this value's drop on this thread.
            if unsafe { OpenClipboard(owner) }.is_ok() {
                return Ok(Self);
            }
            if Instant::now() >= until {
                return Err(Error::Busy);
            }
            thread::sleep(RETRY);
        }
    }
}

impl Drop for Open {
    fn drop(&mut self) {
        // SAFETY: this thread opened the clipboard and closes it once.
        let _ = unsafe { CloseClipboard() };
    }
}

/// An allocation for `SetClipboardData`, freed unless the clipboard took it.
struct Global(Option<HGLOBAL>);

impl Global {
    fn new(units: &[u16]) -> Result<Self, Error> {
        let bytes = std::mem::size_of_val(units);
        // SAFETY: a plain allocation request; the handle is owned below.
        let global =
            unsafe { GlobalAlloc(GMEM_MOVEABLE, bytes) }.map_err(|_| Error::Unavailable)?;
        let owned = Self(Some(global));
        // SAFETY: a live movable block of `bytes` bytes, locked once here.
        let pointer = unsafe { GlobalLock(global) };
        if pointer.is_null() {
            return Err(Error::Unavailable);
        }
        // SAFETY: the locked block holds exactly `units.len()` u16 slots and
        // does not overlap the source; GlobalLock memory is 8-byte aligned.
        unsafe {
            std::ptr::copy_nonoverlapping(units.as_ptr(), pointer.cast::<u16>(), units.len());
        }
        // SAFETY: unlocks the lock taken above. FALSE with no error is the
        // normal result when the lock count reaches zero.
        let _ = unsafe { GlobalUnlock(global) };
        Ok(owned)
    }

    /// The clipboard owns the block now: never free it.
    fn give(&mut self) {
        self.0 = None;
    }
}

impl Drop for Global {
    fn drop(&mut self) {
        if let Some(global) = self.0.take() {
            // SAFETY: a block this value allocated and still owns.
            let _ = unsafe { GlobalFree(Some(global)) };
        }
    }
}

/// Read the clipboard's Unicode text as UTF-8 of at most `limit` bytes,
/// provided its sequence number is still `expected` once open and `allowed()`
/// (the caller's last check, run while open) returns true.
pub fn read_text(
    limit: usize,
    expected: u32,
    wait: Duration,
    allowed: impl FnOnce() -> bool,
) -> Result<Text, Error> {
    let open = Open::new(None, wait)?;
    if sequence() != expected {
        return Err(Error::Changed);
    }
    if !allowed() {
        return Err(Error::Refused);
    }
    // SAFETY: the clipboard is open on this thread; the returned handle stays
    // valid until it closes and is never freed here.
    let handle = unsafe { GetClipboardData(UNICODE_TEXT) }.map_err(|_| Error::NoText)?;
    let global = HGLOBAL(handle.0);
    // SAFETY: a live clipboard block while the clipboard is open.
    let size = unsafe { GlobalSize(global) };
    // SAFETY: as above; unlocked below before the clipboard closes.
    let pointer = unsafe { GlobalLock(global) };
    if pointer.is_null() {
        return Err(Error::Unavailable);
    }
    // SAFETY: the locked block holds `size` bytes, so `size / 2` whole u16
    // units, 8-byte aligned; it is only read, and only until the unlock below.
    let wide = unsafe { std::slice::from_raw_parts(pointer.cast::<u16>().cast_const(), size / 2) };
    let text = decode(wide, limit);
    // SAFETY: the lock taken above.
    let _ = unsafe { GlobalUnlock(global) };
    drop(open);
    text
}

/// The UTF-8 of `wide` up to its first NUL, refusing more than `limit`
/// bytes. Bounded work: every UTF-16 unit yields at least one UTF-8 byte, so
/// only the first `limit + 1` units can matter.
fn decode(wide: &[u16], limit: usize) -> Result<Text, Error> {
    let window = &wide[..wide.len().min(limit.saturating_add(1))];
    let end = match window.iter().position(|&unit| unit == 0) {
        Some(end) => end,
        None if window.len() < wide.len() => return Err(Error::Limit),
        // A block without its NUL: all of it is the text.
        None => window.len(),
    };
    let units = &window[..end];
    let mut length = 0_usize;
    for decoded in char::decode_utf16(units.iter().copied()) {
        length += decoded.map_err(|_| Error::Invalid)?.len_utf8();
        if length > limit {
            return Err(Error::Limit);
        }
    }
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(length)
        .map_err(|_| Error::Unavailable)?;
    let mut text = Text(bytes);
    let mut buffer = [0_u8; 4];
    for decoded in char::decode_utf16(units.iter().copied()).flatten() {
        text.0
            .extend_from_slice(decoded.encode_utf8(&mut buffer).as_bytes());
    }
    buffer.fill(0);
    Ok(text)
}

/// Why a write did not complete, and whether it had already emptied the
/// clipboard (its previous contents are then gone whatever happened next).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WriteError {
    pub error: Error,
    pub attempted: bool,
}

/// Replace the clipboard's contents with `text` as Unicode text owned by
/// `owner` (a window of this process whose thread pumps messages), provided
/// its sequence number is still `expected` once open and `allowed()` (the
/// caller's last check, run while open, immediately before the change)
/// returns true. Returns the sequence number after closing, while `owner`
/// still owns the clipboard; if another program emptied it first, the write
/// happened but its number is unknown (`Changed`, attempted).
pub fn write_text(
    owner: isize,
    text: &str,
    expected: u32,
    wait: Duration,
    allowed: impl FnOnce() -> bool,
) -> Result<u32, WriteError> {
    let refuse = |error| WriteError {
        error,
        attempted: false,
    };
    let mut wide = Wide(Vec::new());
    let units = text
        .encode_utf16()
        .count()
        .checked_add(1)
        .ok_or(refuse(Error::Limit))?;
    wide.0
        .try_reserve_exact(units)
        .map_err(|_| refuse(Error::Unavailable))?;
    wide.0.extend(text.encode_utf16());
    wide.0.push(0);
    let mut global = Global::new(&wide.0).map_err(refuse)?;
    drop(wide);
    let open = Open::new(Some(HWND(owner as *mut _)), wait).map_err(refuse)?;
    if sequence() != expected {
        return Err(refuse(Error::Changed));
    }
    if !allowed() {
        return Err(refuse(Error::Refused));
    }
    let attempted = |error| WriteError {
        error,
        attempted: true,
    };
    // SAFETY: the clipboard is open on this thread with `owner` as owner.
    unsafe { EmptyClipboard() }.map_err(|_| attempted(Error::Unavailable))?;
    let handle = HANDLE(global.0.map_or(std::ptr::null_mut(), |g| g.0));
    // SAFETY: the clipboard is open and emptied by this thread; on success
    // the system owns the block, which is then never freed here.
    unsafe { SetClipboardData(UNICODE_TEXT, Some(handle)) }
        .map_err(|_| attempted(Error::Unavailable))?;
    global.give();
    drop(open);
    let after = sequence();
    if owned_by(owner) {
        Ok(after)
    } else {
        Err(attempted(Error::Changed))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use windows::Win32::Foundation::{LPARAM, WPARAM};
    use windows::Win32::System::Threading::GetCurrentThreadId;
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DispatchMessageW, GetMessageW, HWND_MESSAGE, MSG, PostMessageW,
        PostThreadMessageW, WINDOW_EX_STYLE, WINDOW_STYLE, WM_CLOSE, WM_QUIT,
    };
    use windows::core::w;

    const WAIT: Duration = Duration::from_millis(500);

    /// A message-only window on its own pumping thread, like the host's
    /// clipboard window: the production owner lives on another thread.
    struct Owner {
        window: isize,
        thread_id: u32,
        thread: Option<thread::JoinHandle<()>>,
    }

    impl Owner {
        fn new() -> Self {
            let (send, receive) = mpsc::channel();
            let thread = thread::spawn(move || {
                // SAFETY: a message-only STATIC window owned by this thread.
                let window = unsafe {
                    CreateWindowExW(
                        WINDOW_EX_STYLE::default(),
                        w!("STATIC"),
                        w!(""),
                        WINDOW_STYLE::default(),
                        0,
                        0,
                        0,
                        0,
                        Some(HWND_MESSAGE),
                        None,
                        None,
                        None,
                    )
                }
                .unwrap();
                // SAFETY: no arguments.
                let id = unsafe { GetCurrentThreadId() };
                send.send((window.0 as isize, id)).unwrap();
                let mut message = MSG::default();
                // SAFETY: this thread's queue; ends on WM_QUIT.
                unsafe {
                    while GetMessageW(&raw mut message, None, 0, 0).0 > 0 {
                        DispatchMessageW(&raw const message);
                    }
                }
            });
            let (window, thread_id) = receive.recv().unwrap();
            Self {
                window,
                thread_id,
                thread: Some(thread),
            }
        }
    }

    impl Drop for Owner {
        fn drop(&mut self) {
            // SAFETY: test-only messages without pointers: the default
            // WM_CLOSE handling destroys the window on its own thread, then
            // WM_QUIT ends that thread's loop.
            unsafe {
                let _ = PostMessageW(
                    Some(HWND(self.window as *mut _)),
                    WM_CLOSE,
                    WPARAM(0),
                    LPARAM(0),
                );
                let _ = PostThreadMessageW(self.thread_id, WM_QUIT, WPARAM(0), LPARAM(0));
            }
            if let Some(thread) = self.thread.take() {
                let until = Instant::now() + Duration::from_secs(1);
                while !thread.is_finished() && Instant::now() < until {
                    thread::sleep(Duration::from_millis(5));
                }
            }
        }
    }

    fn put(owner: &Owner, text: &str) -> u32 {
        write_text(owner.window, text, sequence(), WAIT, || true).unwrap()
    }

    fn get(limit: usize) -> Result<Text, Error> {
        read_text(limit, sequence(), WAIT, || true)
    }

    /// One test, run on the interactive desktop (`--ignored`): it replaces the
    /// session's clipboard. Steps share the one clipboard, so they run in
    /// order instead of as parallel tests.
    #[test]
    #[ignore = "replaces the interactive session's clipboard"]
    fn the_clipboard_round_trips_refuses_stale_or_refused_writes_and_bounds_reads() {
        let owner = Owner::new();

        // A write's number is the clipboard's after it closes, and stays
        // (no other writer): a change notice can recognize this write.
        let text = "h\u{e9}llo \u{1d11e} w\u{f6}rld";
        let written = put(&owner, text);
        assert_eq!(sequence(), written);
        assert!(owned_by(owner.window));
        thread::sleep(Duration::from_millis(100));
        assert_eq!(sequence(), written);
        assert!(has_text());
        assert_eq!(get(64).unwrap().as_str(), text);

        // Exact limit reads; one byte less refuses. 'é' is 2 bytes, the
        // clef 4, so the UTF-8 is 18 bytes.
        assert_eq!(get(text.len()).unwrap().as_str(), text);
        assert_eq!(get(text.len() - 1).unwrap_err(), Error::Limit);

        // A stale expectation changes nothing (planted: the old text stays).
        let stale = sequence().wrapping_sub(1);
        let error = write_text(owner.window, "never", stale, WAIT, || true).unwrap_err();
        assert_eq!(
            error,
            WriteError {
                error: Error::Changed,
                attempted: false
            }
        );
        assert_eq!(get(64).unwrap().as_str(), text);
        assert_eq!(
            read_text(64, stale, WAIT, || true).unwrap_err(),
            Error::Changed
        );
        // A refusing last check reads nothing.
        assert_eq!(
            read_text(64, sequence(), WAIT, || false).unwrap_err(),
            Error::Refused
        );

        // A refusing last check changes nothing either.
        let error = write_text(owner.window, "never", sequence(), WAIT, || false).unwrap_err();
        assert_eq!(
            error,
            WriteError {
                error: Error::Refused,
                attempted: false
            }
        );
        assert_eq!(get(64).unwrap().as_str(), text);

        // Empty text is text.
        put(&owner, "");
        assert!(has_text());
        assert_eq!(get(0).unwrap().as_str(), "");

        // Another holder: a short wait reports Busy, a longer one succeeds.
        let (opened, wait_open) = mpsc::channel();
        let (release, wait_release) = mpsc::channel::<()>();
        let holder = thread::spawn(move || {
            let open = Open::new(None, WAIT).unwrap();
            opened.send(()).unwrap();
            let _ = wait_release.recv_timeout(Duration::from_secs(2));
            thread::sleep(Duration::from_millis(100));
            drop(open);
        });
        wait_open.recv().unwrap();
        let error = write_text(
            owner.window,
            "late",
            sequence(),
            Duration::from_millis(20),
            || true,
        )
        .unwrap_err();
        assert_eq!(
            error,
            WriteError {
                error: Error::Busy,
                attempted: false
            }
        );
        release.send(()).unwrap();
        put(&owner, "after the holder");
        holder.join().unwrap();
        assert_eq!(get(64).unwrap().as_str(), "after the holder");

        // Another window's copy takes ownership: the first window's number
        // no longer describes the clipboard (planted: same text).
        let mine = put(&owner, "same");
        let other = Owner::new();
        let theirs = put(&other, "same");
        assert_ne!(theirs, mine);
        assert!(!owned_by(owner.window));
        assert!(owned_by(other.window));
    }

    #[test]
    fn decoding_stops_at_the_nul_and_bounds_the_work() {
        let units: Vec<u16> = "ab\u{1d11e}".encode_utf16().chain([0, 0x61]).collect();
        assert_eq!(decode(&units, 6).unwrap().as_str(), "ab\u{1d11e}");
        assert_eq!(decode(&units, 5).unwrap_err(), Error::Limit);
        // No NUL within limit + 1 units of a longer block: refused unread.
        let long = vec![0x61_u16; 1000];
        assert_eq!(decode(&long, 10).unwrap_err(), Error::Limit);
        // A block without its NUL is all text.
        assert_eq!(decode(&[0x61, 0x62], 2).unwrap().as_str(), "ab");
        // An unpaired surrogate is refused, not replaced.
        assert_eq!(decode(&[0xd834, 0x61, 0], 10).unwrap_err(), Error::Invalid);
        assert_eq!(
            format!("{:?}", decode(&[0x61], 1).unwrap()),
            "ClipboardText([redacted])"
        );
    }
}
