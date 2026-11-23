//! One explicit receiving-folder choice through the installed Linux portal.
//!
//! This private bus owns exactly one request. Setup and every event turn are
//! asynchronous and bounded; no runtime, remote session, file or authority is created.
//! The original `Response` ends the native interaction before its one local URI
//! can leave this owner. Cancellation instead requires a bounded `Request.Close`
//! acknowledgement. The caller must still validate and pin the selected folder.
//!
//! ABI/API references: systemd sd-bus; XDG Desktop Portal `FileChooser` version 3
//! (`directory`) and Request (`handle_token`, `Response`, `Close`).
//!
//! Result copies have the small limits below; native framing retains sd-bus's
//! message ceiling (128 MiB in reviewed systemd v252), before those checks.
//! The private pump consumes and releases one message at a time. This is not a
//! small-memory sandbox against a compromised same-user session-bus peer.
#[path = "directory_picker/bus.rs"]
mod bus;

use bus::{Bus, Message};
use std::{
    ffi::{CStr, CString},
    fmt,
    path::PathBuf,
    time::Duration,
};

const DBUS: &CStr = c"org.freedesktop.DBus";
const DBUS_PATH: &CStr = c"/org/freedesktop/DBus";
const PORTAL: &CStr = c"org.freedesktop.portal.Desktop";
const DESKTOP: &CStr = c"/org/freedesktop/portal/desktop";
const CHOOSER: &CStr = c"org.freedesktop.portal.FileChooser";
const REQUEST: &CStr = c"org.freedesktop.portal.Request";
const CHOICE_US: u64 = 120_000_000;
const SETUP_US: u64 = 5_000_000;
const CLOSE_US: u64 = 2_000_000;
const TURN_LIMIT: usize = 64;
const MESSAGE_LIMIT: usize = 256;

/// Content-free failures. No path, URI, bus address or native error text escapes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Unavailable,
    Cancelled,
    Expired,
    InvalidSelection,
    CleanupIncomplete,
    EventFlood,
    Clock,
    AlreadyUsed,
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "directory-picker: {self:?}")
    }
}
impl std::error::Error for Error {}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Connecting,
    Activating(u64),
    Owner(u64),
    PortalUid(u64),
    Version(u64),
    Subscribing(u64),
    Opening(u64),
    Waiting,
    Closed,
}
impl Phase {
    fn cookie(self) -> Option<u64> {
        match self {
            Self::Activating(n)
            | Self::Owner(n)
            | Self::PortalUid(n)
            | Self::Version(n)
            | Self::Subscribing(n)
            | Self::Opening(n) => Some(n),
            Self::Connecting | Self::Waiting | Self::Closed => None,
        }
    }
}

/// Thread-confined native request; all borrowed bus strings stay inside `poll`.
/// `poll` never waits for user interaction. `close` may wait at most two seconds
/// for the original portal's cleanup; failure never releases a selected path.
pub struct DirectoryPicker {
    bus: Option<Bus>,
    phase: Phase,
    until: u64,
    setup_until: u64,
    previous: u64,
    token: CString,
    owner: Option<CString>,
    path: Option<CString>,
    activation_attempted: bool,
    request_sent: bool,
    response_received: bool,
    response: Option<Result<PathBuf, Error>>,
    terminal: Option<Error>,
    messages: usize,
}
impl fmt::Debug for DirectoryPicker {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("DirectoryPicker([one local request])")
    }
}

impl DirectoryPicker {
    pub fn open() -> Result<Self, Error> {
        let now = bus::now()?;
        let address = session_address()?;
        Self::open_at(&address, now)
    }

    fn open_at(address: &CStr, now: u64) -> Result<Self, Error> {
        Ok(Self {
            bus: Some(Bus::open(address)?),
            phase: Phase::Connecting,
            until: now.checked_add(CHOICE_US).ok_or(Error::Clock)?,
            setup_until: now.checked_add(SETUP_US).ok_or(Error::Clock)?,
            previous: now,
            token: bus::token()?,
            owner: None,
            path: None,
            activation_attempted: false,
            request_sent: false,
            response_received: false,
            response: None,
            terminal: None,
            messages: 0,
        })
    }

    /// One bounded, nonblocking turn. A successful choice consumes this owner.
    pub fn poll(&mut self) -> Result<Option<PathBuf>, Error> {
        if let Some(error) = self.terminal {
            return Err(error);
        }
        if self.phase == Phase::Closed {
            return Err(Error::AlreadyUsed);
        }
        let result = self.poll_inner();
        if let Err(error) = result {
            self.terminal = Some(error);
            self.response = None;
        }
        result
    }

    fn poll_inner(&mut self) -> Result<Option<PathBuf>, Error> {
        self.check_time()?;
        self.turn()?;
        self.check_time()?;
        if self.phase != Phase::Waiting || !self.response_received {
            return Ok(None);
        }
        // Request.Response is the portal's documented end-of-interaction
        // acknowledgement. Opening must ALSO have returned the exact handle.
        let result = self.response.take().ok_or(Error::InvalidSelection)?;
        self.disconnect();
        self.check_choice_time()?;
        result.map(Some)
    }

    fn check_choice_time(&mut self) -> Result<u64, Error> {
        let now = bus::now()?;
        if now < self.previous {
            return Err(Error::Clock);
        }
        self.previous = now;
        if now >= self.until {
            return Err(Error::Expired);
        }
        Ok(now)
    }

    fn check_time(&mut self) -> Result<(), Error> {
        let now = self.check_choice_time()?;
        if self.phase != Phase::Waiting && now >= self.setup_until {
            return Err(Error::Unavailable);
        }
        Ok(())
    }

    fn turn(&mut self) -> Result<(), Error> {
        for _ in 0..TURN_LIMIT {
            let bus = self.bus.as_ref().ok_or(Error::AlreadyUsed)?;
            if self.phase == Phase::Connecting && bus.ready()? {
                bus.verify_owner_uid()?;
                self.path = Some(request_path(&bus.unique_name()?, &self.token)?);
                let message = bus.call(DBUS, DBUS_PATH, DBUS, c"GetNameOwner")?;
                message.text(PORTAL)?;
                self.phase = Phase::Owner(bus.send(&message)?);
            }
            let (progress, message) = bus.process()?;
            if let Some(message) = message {
                self.messages += 1;
                if self.messages > MESSAGE_LIMIT {
                    return Err(Error::EventFlood);
                }
                self.message(&message)?;
            }
            if !progress {
                return Ok(());
            }
        }
        Err(Error::EventFlood)
    }

    #[allow(clippy::too_many_lines)]
    fn message(&mut self, message: &Message) -> Result<(), Error> {
        if message.is_signal(REQUEST, c"Response")? {
            if !self.request_sent
                || !message.from(self.owner.as_deref().ok_or(Error::Unavailable)?)?
                || !message.on_path(self.path.as_deref().ok_or(Error::Unavailable)?)?
            {
                // An unrelated local signal cannot complete this request.
                return Ok(());
            }
            if self.response_received {
                return Err(Error::InvalidSelection);
            }
            // Only a well-formed terminal Response envelope proves the UI
            // ended. Invalid selected content still refuses the choice.
            let result = response(message)?;
            self.response_received = true;
            self.response = Some(result);
            return Ok(());
        }
        if self.phase.cookie().is_none() || message.reply_cookie()? != self.phase.cookie() {
            return Ok(());
        }
        if matches!(self.phase, Phase::Owner(_))
            && !self.activation_attempted
            && message.from(DBUS)?
            && message.is_error(c"org.freedesktop.DBus.Error.NameHasNoOwner")?
        {
            // An already-running portal need not have activation metadata.
            // Only an authenticated absence permits one bounded activation;
            // a subsequent owner lookup cannot restart the setup lifetime.
            let bus = self.bus.as_ref().ok_or(Error::Unavailable)?;
            let query = bus.call(DBUS, DBUS_PATH, DBUS, c"StartServiceByName")?;
            query.text(PORTAL)?;
            query.number(0)?;
            self.activation_attempted = true;
            self.phase = Phase::Activating(bus.send(&query)?);
            return Ok(());
        }
        let expected = match self.phase {
            Phase::Version(_) | Phase::Opening(_) => {
                self.owner.as_deref().ok_or(Error::Unavailable)?
            }
            _ => DBUS,
        };
        message.success_from(expected)?;
        let bus = self.bus.as_ref().ok_or(Error::Unavailable)?;
        match self.phase {
            Phase::Activating(_) => {
                message.signature(c"u")?;
                if !matches!(message.read_number()?, 1 | 2) {
                    return Err(Error::Unavailable);
                }
                let query = bus.call(DBUS, DBUS_PATH, DBUS, c"GetNameOwner")?;
                query.text(PORTAL)?;
                self.phase = Phase::Owner(bus.send(&query)?);
            }
            Phase::Owner(_) => {
                message.signature(c"s")?;
                let owner = message.read_text(b's', 255)?;
                if !unique_name(&owner) {
                    return Err(Error::Unavailable);
                }
                let owner = CString::new(owner).map_err(|_| Error::Unavailable)?;
                let query = bus.call(DBUS, DBUS_PATH, DBUS, c"GetConnectionUnixUser")?;
                query.text(&owner)?;
                self.owner = Some(owner);
                if bus.requires_peer_uid() {
                    self.phase = Phase::PortalUid(bus.send(&query)?);
                } else {
                    // Private anonymous TCP test brokers have no Unix identity;
                    // the production bus always takes the checked UID phase.
                    self.start_version()?;
                }
            }
            Phase::PortalUid(_) => {
                message.signature(c"u")?;
                if message.read_number()? != bus::effective_uid() {
                    return Err(Error::Unavailable);
                }
                self.start_version()?;
            }
            Phase::Version(_) => {
                message.signature(c"v")?;
                message.require_container(b'v', c"u")?;
                if message.read_number()? < 3 {
                    return Err(Error::Unavailable);
                }
                message.leave()?;
                let owner = self.owner.as_ref().ok_or(Error::Unavailable)?;
                let path = self.path.as_ref().ok_or(Error::Unavailable)?;
                let rule = CString::new(format!(
                    "type='signal',sender='{}',path='{}',interface='org.freedesktop.portal.Request',member='Response'",
                    owner.to_str().map_err(|_| Error::Unavailable)?,
                    path.to_str().map_err(|_| Error::Unavailable)?
                ))
                .map_err(|_| Error::Unavailable)?;
                let query = bus.call(DBUS, DBUS_PATH, DBUS, c"AddMatch")?;
                query.text(&rule)?;
                self.phase = Phase::Subscribing(bus.send(&query)?);
            }
            Phase::Subscribing(_) => {
                message.signature(c"")?;
                let query = bus.call(
                    self.owner.as_deref().ok_or(Error::Unavailable)?,
                    DESKTOP,
                    CHOOSER,
                    c"OpenFile",
                )?;
                query.text(c"")?;
                query.text(c"Choose a folder to receive files")?;
                query.open(b'a', c"{sv}")?;
                query.string_option(c"handle_token", &self.token)?;
                query.boolean_option(c"directory", true)?;
                query.boolean_option(c"multiple", false)?;
                query.string_option(c"accept_label", c"Receive Here")?;
                query.close()?;
                // Even uncertain queue admission retains original UI cleanup.
                self.request_sent = true;
                self.phase = Phase::Opening(0);
                self.phase = Phase::Opening(bus.send(&query)?);
            }
            Phase::Opening(_) => {
                message.signature(c"o")?;
                let returned = CString::new(message.read_text(b'o', 512)?)
                    .map_err(|_| Error::InvalidSelection)?;
                if self.path.as_ref() != Some(&returned) {
                    // This exact method returned a different native owner. Keep
                    // it solely for Close; never accept an old-handle response.
                    self.path = Some(returned);
                    self.response_received = false;
                    self.response = None;
                    self.phase = Phase::Waiting;
                    return Err(Error::InvalidSelection);
                }
                self.phase = Phase::Waiting;
            }
            Phase::Connecting | Phase::Waiting | Phase::Closed => {}
        }
        Ok(())
    }

    fn start_version(&mut self) -> Result<(), Error> {
        let bus = self.bus.as_ref().ok_or(Error::Unavailable)?;
        let query = bus.call(
            self.owner.as_deref().ok_or(Error::Unavailable)?,
            DESKTOP,
            c"org.freedesktop.DBus.Properties",
            c"Get",
        )?;
        query.text(CHOOSER)?;
        query.text(c"version")?;
        self.phase = Phase::Version(bus.send(&query)?);
        Ok(())
    }

    /// End the original native interaction. No unconfirmed cleanup can be
    /// interpreted as consent to connect; repeated calls preserve a failure.
    pub fn close(&mut self) -> Result<(), Error> {
        if self.phase == Phase::Closed {
            return if self.terminal == Some(Error::CleanupIncomplete) {
                Err(Error::CleanupIncomplete)
            } else {
                Ok(())
            };
        }
        self.response = None;
        let result = self.close_inner();
        self.disconnect();
        if result.is_err() {
            self.terminal = Some(Error::CleanupIncomplete);
        } else if self.terminal.is_none() {
            self.terminal = Some(Error::Cancelled);
        }
        result.map_err(|_| Error::CleanupIncomplete)
    }

    fn close_inner(&mut self) -> Result<(), Error> {
        if !self.request_sent {
            return Ok(());
        }
        let until = bus::now()?.checked_add(CLOSE_US).ok_or(Error::Clock)?;
        // A pending OpenFile must settle first: closing a not-yet-created
        // guessed object does not prove that delayed opening was cancelled.
        while matches!(self.phase, Phase::Opening(_)) {
            self.cleanup_turn(until)?;
        }
        if self.response_received {
            return Ok(());
        }
        let bus = self.bus.as_ref().ok_or(Error::CleanupIncomplete)?;
        let query = bus.call(
            self.owner.as_deref().ok_or(Error::CleanupIncomplete)?,
            self.path.as_deref().ok_or(Error::CleanupIncomplete)?,
            REQUEST,
            c"Close",
        )?;
        let cookie = bus.send(&query)?;
        loop {
            self.cleanup_deadline(until)?;
            for _ in 0..TURN_LIMIT {
                let (progress, message) = self
                    .bus
                    .as_ref()
                    .ok_or(Error::CleanupIncomplete)?
                    .process()?;
                if let Some(message) = message {
                    if message.reply_cookie()? == Some(cookie) {
                        message
                            .success_from(self.owner.as_deref().ok_or(Error::CleanupIncomplete)?)?;
                        message.signature(c"")?;
                        self.cleanup_deadline(until)?;
                        return Ok(());
                    }
                    self.message(&message)?;
                    if self.response_received {
                        self.cleanup_deadline(until)?;
                        return Ok(());
                    }
                }
                if !progress {
                    break;
                }
            }
            self.wait_cleanup(until)?;
        }
    }

    fn cleanup_deadline(&mut self, until: u64) -> Result<u64, Error> {
        let now = bus::now()?;
        if now < self.previous || now >= until {
            return Err(Error::CleanupIncomplete);
        }
        self.previous = now;
        Ok(now)
    }
    fn cleanup_turn(&mut self, until: u64) -> Result<(), Error> {
        self.cleanup_deadline(until)?;
        match self.turn() {
            Ok(()) => {}
            Err(error) if self.phase == Phase::Waiting => {
                // Opening settled even if its returned handle differed. Retain
                // that refusal and close the actual owner before disconnecting.
                self.terminal.get_or_insert(error);
            }
            Err(error) => return Err(error),
        }
        if self.phase == Phase::Waiting {
            self.cleanup_deadline(until)?;
            Ok(())
        } else {
            self.wait_cleanup(until)
        }
    }
    fn wait_cleanup(&mut self, until: u64) -> Result<(), Error> {
        let now = self.cleanup_deadline(until)?;
        self.bus
            .as_ref()
            .ok_or(Error::CleanupIncomplete)?
            .wait(Duration::from_micros((until - now).min(10_000)))
    }
    fn disconnect(&mut self) {
        self.bus = None;
        self.phase = Phase::Closed;
    }
}
impl Drop for DirectoryPicker {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

fn unique_name(name: &str) -> bool {
    name.starts_with(':')
        && name.len() <= 255
        && name.len() > 3
        && name[1..].contains('.')
        && name[1..].bytes().all(|b| b.is_ascii_digit() || b == b'.')
}

fn request_path(sender: &str, token: &CStr) -> Result<CString, Error> {
    if !unique_name(sender) {
        return Err(Error::Unavailable);
    }
    let token = token.to_str().map_err(|_| Error::Unavailable)?;
    if token.is_empty()
        || !token
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_')
    {
        return Err(Error::Unavailable);
    }
    CString::new(format!(
        "/org/freedesktop/portal/desktop/request/{}/{}",
        sender[1..].replace('.', "_"),
        token
    ))
    .map_err(|_| Error::Unavailable)
}

fn response(message: &Message) -> Result<Result<PathBuf, Error>, Error> {
    message.signature(c"ua{sv}")?;
    match message.read_number()? {
        0 => Ok(selected_response(message)),
        1 => Ok(Err(Error::Cancelled)),
        2 => Ok(Err(Error::Unavailable)),
        _ => Err(Error::InvalidSelection),
    }
}

fn selected_response(message: &Message) -> Result<PathBuf, Error> {
    message.require_container(b'a', c"{sv}")?;
    let mut selected = None;
    let mut count = 0;
    while message.enter(b'e', c"sv")? {
        count += 1;
        if count > 16 {
            return Err(Error::InvalidSelection);
        }
        let key = message.read_text(b's', 128)?;
        if key == "uris" {
            if selected.is_some() {
                return Err(Error::InvalidSelection);
            }
            message.require_container(b'v', c"as")?;
            message.require_container(b'a', c"s")?;
            let uri = message.read_text(b's', 12_288)?;
            if !message.at_end()? {
                return Err(Error::InvalidSelection);
            }
            selected = Some(selected_directory(&uri)?);
            message.leave()?;
            message.leave()?;
        } else {
            message.skip(c"v")?;
        }
        message.leave()?;
    }
    message.leave()?;
    selected.ok_or(Error::InvalidSelection)
}

fn selected_directory(uri: &str) -> Result<PathBuf, Error> {
    // A portal result is ONE URI, not a text/uri-list; newlines and comments
    // cannot disguise additional or substituted selections.
    if uri.contains(['\n', '\r']) {
        return Err(Error::InvalidSelection);
    }
    let mut paths = crate::file_drop::paths(uri.as_bytes()).map_err(|_| Error::InvalidSelection)?;
    if paths.len() != 1 {
        return Err(Error::InvalidSelection);
    }
    paths.pop().ok_or(Error::InvalidSelection)
}

fn session_address() -> Result<CString, Error> {
    let address = match std::env::var("DBUS_SESSION_BUS_ADDRESS") {
        Ok(value) => value,
        Err(std::env::VarError::NotUnicode(_)) => return Err(Error::Unavailable),
        Err(std::env::VarError::NotPresent) => {
            let root = std::env::var("XDG_RUNTIME_DIR")
                .unwrap_or_else(|_| format!("/run/user/{}", bus::effective_uid()));
            if !root.starts_with('/') || root.len() > 4096 {
                return Err(Error::Unavailable);
            }
            // Runtime directory bytes must not become bus-address syntax.
            let mut escaped = String::new();
            for b in format!("{root}/bus").bytes() {
                if b.is_ascii_alphanumeric() || b"/_.-".contains(&b) {
                    escaped.push(char::from(b));
                } else {
                    use std::fmt::Write;
                    write!(escaped, "%{b:02x}").map_err(|_| Error::Unavailable)?;
                }
            }
            format!("unix:path={escaped}")
        }
    };
    local_address(&address)
}

fn local_address(address: &str) -> Result<CString, Error> {
    if address.len() > 16_384 || !address.starts_with("unix:") || address.contains(';') {
        return Err(Error::Unavailable);
    }
    CString::new(address).map_err(|_| Error::Unavailable)
}

#[cfg(test)]
#[path = "directory_picker/tests.rs"]
mod tests;
