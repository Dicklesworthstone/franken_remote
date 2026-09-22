//! Narrow libsystemd ABI. Bus, message and callback state are owned by ONE native
//! thread. No foreign pointer or borrowed message string crosses that thread.
//! Callback storage outlives the bus/slots; it never unwinds through C.
use super::{Selection, Shared, StopReason};
use std::{
    cell::Cell,
    ffi::{CStr, CString, c_char, c_int, c_long, c_void},
    ptr::{self, NonNull},
    sync::{Arc, OnceLock},
};
pub(super) const SYSTEM_ADDRESS: &str = "unix:path=/run/dbus/system_bus_socket";
const LOGIN: &CStr = c"org.freedesktop.login1";
const MANAGER: &CStr = c"/org/freedesktop/login1";
const SESSION: &CStr = c"org.freedesktop.login1.Session";
const PROPERTIES: &CStr = c"org.freedesktop.DBus.Properties";
const DBUS: &CStr = c"org.freedesktop.DBus";
const DBUS_PATH: &CStr = c"/org/freedesktop/DBus";
type Raw = *mut c_void;
type Handler = unsafe extern "C" fn(Raw, Raw, Raw) -> c_int;

// Declarations checked against systemd v257 src/systemd/sd-bus.h. Only opaque
// pointers cross the ABI; NULL error storage discards sensitive native strings.
#[link(name = "libsystemd.so.0", kind = "dylib", modifiers = "+verbatim")]
unsafe extern "C" {
    fn sd_bus_new(bus: *mut Raw) -> c_int;
    fn sd_bus_set_address(bus: Raw, address: *const c_char) -> c_int;
    fn sd_bus_set_bus_client(bus: Raw, client: c_int) -> c_int;
    fn sd_bus_set_method_call_timeout(bus: Raw, usec: u64) -> c_int;
    fn sd_bus_start(bus: Raw) -> c_int;
    fn sd_bus_close_unref(bus: Raw) -> Raw;
    fn sd_bus_get_owner_creds(bus: Raw, mask: u64, creds: *mut Raw) -> c_int;
    fn sd_bus_get_name_creds(bus: Raw, name: *const c_char, mask: u64, creds: *mut Raw) -> c_int;
    fn sd_bus_creds_get_euid(creds: Raw, uid: *mut u32) -> c_int;
    fn sd_bus_creds_unref(creds: Raw) -> Raw;
    fn sd_bus_add_match(
        bus: Raw,
        slot: *mut Raw,
        rule: *const c_char,
        handler: Option<Handler>,
        userdata: Raw,
    ) -> c_int;
    fn sd_bus_process(bus: Raw, message: *mut Raw) -> c_int;
    fn sd_bus_call_method(
        bus: Raw,
        destination: *const c_char,
        path: *const c_char,
        interface: *const c_char,
        method: *const c_char,
        error: Raw,
        reply: *mut Raw,
        types: *const c_char,
        ...
    ) -> c_int;
    fn sd_bus_get_property_trivial(
        bus: Raw,
        destination: *const c_char,
        path: *const c_char,
        interface: *const c_char,
        property: *const c_char,
        error: Raw,
        ty: c_char,
        result: Raw,
    ) -> c_int;
    fn sd_bus_message_unref(message: Raw) -> Raw;
    fn sd_bus_message_get_sender(message: Raw) -> *const c_char;
    fn sd_bus_message_get_path(message: Raw) -> *const c_char;
    fn sd_bus_message_get_interface(message: Raw) -> *const c_char;
    fn sd_bus_message_get_member(message: Raw) -> *const c_char;
    fn sd_bus_message_has_signature(message: Raw, signature: *const c_char) -> c_int;
    fn sd_bus_message_enter_container(message: Raw, ty: c_char, contents: *const c_char) -> c_int;
    fn sd_bus_message_exit_container(message: Raw) -> c_int;
    fn sd_bus_message_read_basic(message: Raw, ty: c_char, result: Raw) -> c_int;
    fn sd_bus_message_skip(message: Raw, signature: *const c_char) -> c_int;
    fn strnlen(s: *const c_char, maxlen: usize) -> usize;
    fn clock_gettime(clock: c_int, time: *mut Timespec) -> c_int;
    fn geteuid() -> u32;
}
pub(super) fn effective_uid() -> u32 {
    // SAFETY: getuid family has no parameters, memory ownership or side effects.
    unsafe { geteuid() }
}
#[repr(C)]
struct Timespec {
    sec: c_long,
    nanos: c_long,
}
pub(super) fn boottime() -> Result<u64, StopReason> {
    let mut t = Timespec { sec: 0, nanos: 0 };
    // SAFETY: writable ABI-compatible timespec; Linux CLOCK_BOOTTIME = 7.
    if unsafe { clock_gettime(7, &raw mut t) } != 0 || !(0..1_000_000_000).contains(&t.nanos) {
        return Err(StopReason::Clock);
    }
    u64::try_from(t.sec)
        .ok()
        .and_then(|s| s.checked_mul(1_000_000_000))
        .and_then(|s| s.checked_add(u64::try_from(t.nanos).ok()?))
        .ok_or(StopReason::Clock)
}
fn ok(value: c_int) -> Result<(), StopReason> {
    if value < 0 {
        Err(StopReason::BusUnavailable)
    } else {
        Ok(())
    }
}
fn read(value: c_int) -> Result<(), StopReason> {
    if value <= 0 {
        Err(StopReason::Malformed)
    } else {
        Ok(())
    }
}
// SAFETY: every caller supplies a NULL or live NUL-terminated libsystemd string.
// Limit scanning/copying; the owning message/bus remains alive throughout use.
unsafe fn string(p: *const c_char) -> Result<String, StopReason> {
    if p.is_null() || unsafe { strnlen(p, 513) } > 512 {
        return Err(StopReason::Malformed);
    }
    unsafe { CStr::from_ptr(p) }
        .to_str()
        .map(str::to_owned)
        .map_err(|_| StopReason::Malformed)
}
struct Message(NonNull<c_void>);
impl Drop for Message {
    fn drop(&mut self) {
        unsafe {
            sd_bus_message_unref(self.0.as_ptr());
        }
    }
}
// Borrowed cursor: never unreferences signal messages owned by libsystemd.
struct Cursor(Raw);
impl Cursor {
    fn signature(&self, signature: &CStr) -> Result<(), StopReason> {
        read(unsafe { sd_bus_message_has_signature(self.0, signature.as_ptr()) })
    }
    fn enter(&self, ty: u8, contents: &CStr) -> Result<bool, StopReason> {
        let n =
            unsafe { sd_bus_message_enter_container(self.0, ty.cast_signed(), contents.as_ptr()) };
        if n < 0 {
            Err(StopReason::Malformed)
        } else {
            Ok(n > 0)
        }
    }
    fn leave(&self) -> Result<(), StopReason> {
        ok(unsafe { sd_bus_message_exit_container(self.0) }).map_err(|_| StopReason::Malformed)
    }
    fn text(&self, ty: u8) -> Result<String, StopReason> {
        let mut p: *const c_char = ptr::null();
        read(unsafe { sd_bus_message_read_basic(self.0, ty.cast_signed(), (&raw mut p).cast()) })?;
        unsafe { string(p) }
    }
    fn number(&self) -> Result<u32, StopReason> {
        let mut n = 0_u32;
        read(unsafe {
            sd_bus_message_read_basic(self.0, b'u'.cast_signed(), (&raw mut n).cast())
        })?;
        Ok(n)
    }
    fn timestamp(&self) -> Result<u64, StopReason> {
        let mut n = 0_u64;
        read(unsafe {
            sd_bus_message_read_basic(self.0, b't'.cast_signed(), (&raw mut n).cast())
        })?;
        Ok(n)
    }
    fn boolean(&self) -> Result<bool, StopReason> {
        let mut n = 0_i32;
        read(unsafe {
            sd_bus_message_read_basic(self.0, b'b'.cast_signed(), (&raw mut n).cast())
        })?;
        match n {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(StopReason::Malformed),
        }
    }
    fn skip_variant(&self) -> Result<(), StopReason> {
        read(unsafe { sd_bus_message_skip(self.0, c"v".as_ptr()) })
    }
    fn variant(&self, signature: &CStr) -> Result<(), StopReason> {
        if self.enter(b'v', signature)? {
            Ok(())
        } else {
            Err(StopReason::Malformed)
        }
    }
}

// These independent booleans mirror logind properties, not application authority.
#[allow(clippy::struct_excessive_bools)]
#[derive(Default, PartialEq, Eq)]
pub(super) struct Snapshot {
    id: String,
    uid: u32,
    seat: String,
    display: String,
    timestamp: u64,
    leader: u32,
    kind: String,
    class: String,
    state: String,
    active: bool,
    remote: bool,
    locked: bool,
    can_lock: bool,
}
const FIELDS: [&str; 13] = [
    "Id",
    "User",
    "Seat",
    "Display",
    "TimestampMonotonic",
    "Leader",
    "Type",
    "Class",
    "State",
    "Active",
    "Remote",
    "LockedHint",
    "CanLock",
];
impl Snapshot {
    fn decode(cursor: &Cursor) -> Result<Self, StopReason> {
        cursor.signature(c"a{sv}")?;
        if !cursor.enter(b'a', c"{sv}")? {
            return Err(StopReason::Malformed);
        }
        let mut value = Self::default();
        let mut seen = 0_u16;
        let mut count = 0;
        while cursor.enter(b'e', c"sv")? {
            count += 1;
            if count > 64 {
                return Err(StopReason::Malformed);
            }
            let field = cursor.text(b's')?;
            if let Some(bit) = FIELDS.iter().position(|key| *key == field) {
                if seen & (1 << bit) != 0 {
                    return Err(StopReason::Malformed);
                }
                seen |= 1 << bit;
                match field.as_str() {
                    "User" => {
                        cursor.variant(c"(uo)")?;
                        if !cursor.enter(b'r', c"uo")? {
                            return Err(StopReason::Malformed);
                        }
                        value.uid = cursor.number()?;
                        cursor.text(b'o')?;
                        cursor.leave()?;
                    }
                    "Seat" => {
                        cursor.variant(c"(so)")?;
                        if !cursor.enter(b'r', c"so")? {
                            return Err(StopReason::Malformed);
                        }
                        value.seat = cursor.text(b's')?;
                        cursor.text(b'o')?;
                        cursor.leave()?;
                    }
                    "TimestampMonotonic" => {
                        cursor.variant(c"t")?;
                        value.timestamp = cursor.timestamp()?;
                    }
                    "Leader" => {
                        cursor.variant(c"u")?;
                        value.leader = cursor.number()?;
                    }
                    "Active" | "Remote" | "LockedHint" | "CanLock" => {
                        cursor.variant(c"b")?;
                        let b = cursor.boolean()?;
                        match field.as_str() {
                            "Active" => value.active = b,
                            "Remote" => value.remote = b,
                            "LockedHint" => value.locked = b,
                            _ => value.can_lock = b,
                        }
                    }
                    _ => {
                        cursor.variant(c"s")?;
                        let s = cursor.text(b's')?;
                        match field.as_str() {
                            "Id" => value.id = s,
                            "Display" => value.display = s,
                            "Type" => value.kind = s,
                            "Class" => value.class = s,
                            "State" => value.state = s,
                            _ => return Err(StopReason::Malformed),
                        }
                    }
                }
                cursor.leave()?;
            } else {
                cursor.skip_variant()?;
            }
            cursor.leave()?;
        }
        cursor.leave()?;
        if seen != (1 << FIELDS.len()) - 1 {
            return Err(StopReason::Malformed);
        }
        Ok(value)
    }
    pub(super) fn validate(&self, selected: &Selection) -> Result<(), StopReason> {
        if self.id != selected.session
            || self.uid != selected.uid
            || self.seat != selected.seat
            || self.display != selected.display
            || self.timestamp == 0
            || self.leader == 0
        {
            return Err(StopReason::IdentityChanged);
        }
        if self.kind != "x11" || self.class != "user" || self.remote || !self.can_lock {
            return Err(StopReason::UnsupportedSession);
        }
        if self.locked {
            return Err(StopReason::Locked);
        }
        if !self.active || self.state != "active" {
            return Err(StopReason::Inactive);
        }
        Ok(())
    }
}

struct Signals {
    shared: Arc<Shared>,
    owner: CString,
    session: String,
    path: OnceLock<CString>,
    count: Cell<u32>,
}
impl Signals {
    fn handle(&self, raw: Raw) -> Result<(), StopReason> {
        let sender = unsafe { string(sd_bus_message_get_sender(raw)) }?;
        let interface = unsafe { string(sd_bus_message_get_interface(raw)) }?;
        let member = unsafe { string(sd_bus_message_get_member(raw)) }?;
        let cursor = Cursor(raw);
        self.count.set(self.count.get().saturating_add(1));
        if self.count.get() > 32 {
            return Err(StopReason::EventFlood);
        }
        if sender == "org.freedesktop.DBus"
            && interface == "org.freedesktop.DBus"
            && member == "NameOwnerChanged"
        {
            cursor.signature(c"sss")?;
            if cursor.text(b's')? == "org.freedesktop.login1" {
                return Err(StopReason::IdentityChanged);
            }
            return Ok(());
        }
        if sender.as_bytes() != self.owner.as_bytes() {
            return Ok(());
        }
        if interface == "org.freedesktop.login1.Manager" {
            match member.as_str() {
                "PrepareForSleep" | "PrepareForShutdown" => {
                    cursor.signature(c"b")?;
                    if cursor.boolean()? {
                        return Err(StopReason::Suspending);
                    }
                }
                "SessionRemoved" => {
                    cursor.signature(c"so")?;
                    if cursor.text(b's')? == self.session {
                        return Err(StopReason::SessionUnavailable);
                    }
                }
                _ => {}
            }
            return Ok(());
        }
        let path = unsafe { string(sd_bus_message_get_path(raw)) }?;
        if self
            .path
            .get()
            .is_some_and(|expected| expected.as_bytes() != path.as_bytes())
        {
            return Ok(());
        }
        if interface == "org.freedesktop.login1.Session" && member == "Lock" {
            return Err(StopReason::Locked);
        }
        if interface == "org.freedesktop.DBus.Properties" && member == "PropertiesChanged" {
            cursor.signature(c"sa{sv}as")?;
            if cursor.text(b's')? != "org.freedesktop.login1.Session" {
                return Ok(());
            }
            if !cursor.enter(b'a', c"{sv}")? {
                return Err(StopReason::Malformed);
            }
            let mut count = 0;
            while cursor.enter(b'e', c"sv")? {
                count += 1;
                if count > 64 {
                    return Err(StopReason::EventFlood);
                }
                changed(&cursor.text(b's')?)?;
                cursor.skip_variant()?;
                cursor.leave()?;
            }
            cursor.leave()?;
            if !cursor.enter(b'a', c"s")? {
                return Err(StopReason::Malformed);
            }
            // Invalidating a critical property is not permission to retain it.
            loop {
                let mut p: *const c_char = ptr::null();
                let n = unsafe {
                    sd_bus_message_read_basic(raw, b's'.cast_signed(), (&raw mut p).cast())
                };
                if n == 0 {
                    break;
                }
                read(n)?;
                count += 1;
                if count > 128 {
                    return Err(StopReason::EventFlood);
                }
                changed(&unsafe { string(p) }?)?;
            }
        }
        Ok(())
    }
}
fn changed(field: &str) -> Result<(), StopReason> {
    match field {
        "LockedHint" => Err(StopReason::Locked),
        "Active" | "State" => Err(StopReason::Inactive),
        field if FIELDS.contains(&field) => Err(StopReason::IdentityChanged),
        _ => Ok(()),
    }
}
unsafe extern "C" fn signal(raw: Raw, data: Raw, _: Raw) -> c_int {
    // SAFETY: stable Box<Signals> registered with this bus; close/unref destroys
    // all floating match slots BEFORE the Box is dropped. Callbacks are serial.
    let state = unsafe { &*data.cast::<Signals>() };
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| state.handle(raw)));
    if let Err(reason) = result.unwrap_or(Err(StopReason::NativeFailure)) {
        state.shared.stop(reason);
    }
    1
}

pub(super) struct Connection {
    bus: NonNull<c_void>,
    signals: Box<Signals>,
}
impl Drop for Connection {
    fn drop(&mut self) {
        unsafe {
            sd_bus_close_unref(self.bus.as_ptr());
        }
    }
}
impl Connection {
    pub(super) fn open(
        address: &str,
        selected: &Selection,
        shared: Arc<Shared>,
        trusted_uid: u32,
    ) -> Result<Self, StopReason> {
        let mut bus = ptr::null_mut();
        ok(unsafe { sd_bus_new(&raw mut bus) })?;
        let mut this = Self {
            bus: NonNull::new(bus).ok_or(StopReason::BusUnavailable)?,
            signals: Box::new(Signals {
                shared,
                owner: CString::default(),
                session: selected.session.clone(),
                path: OnceLock::new(),
                count: Cell::new(0),
            }),
        };
        let address = CString::new(address).map_err(|_| StopReason::Malformed)?;
        ok(unsafe { sd_bus_set_address(bus, address.as_ptr()) })?;
        ok(unsafe { sd_bus_set_bus_client(bus, 1) })?;
        ok(unsafe { sd_bus_set_method_call_timeout(bus, 200_000) })?;
        ok(unsafe { sd_bus_start(bus) })?;
        this.credentials(None, trusted_uid)?;
        let owner = this.owner()?;
        this.credentials(Some(&owner), trusted_uid)?;
        this.signals.owner = owner;
        let rules = [
            "type='signal',sender='org.freedesktop.DBus',interface='org.freedesktop.DBus',member='NameOwnerChanged',arg0='org.freedesktop.login1'".to_owned(),
            format!("type='signal',sender='{}'", this.signals.owner.to_str().map_err(|_| StopReason::Malformed)?),
        ];
        for rule in rules {
            let rule = CString::new(rule).map_err(|_| StopReason::Malformed)?;
            // Floating slots are destroyed with the bus, while signals still live.
            ok(unsafe {
                sd_bus_add_match(
                    bus,
                    ptr::null_mut(),
                    rule.as_ptr(),
                    Some(signal),
                    (&raw mut *this.signals).cast(),
                )
            })?;
        }
        let session =
            CString::new(selected.session.as_bytes()).map_err(|_| StopReason::Malformed)?;
        let message = this
            .call(
                &this.signals.owner,
                MANAGER,
                c"org.freedesktop.login1.Manager",
                c"GetSession",
                &session,
            )
            .map_err(|_| StopReason::SessionUnavailable)?;
        let cursor = Cursor(message.0.as_ptr());
        cursor.signature(c"o")?;
        let path = cursor.text(b'o')?;
        if !path.starts_with("/org/freedesktop/login1/session/") {
            return Err(StopReason::Malformed);
        }
        this.signals
            .path
            .set(CString::new(path).map_err(|_| StopReason::Malformed)?)
            .map_err(|_| StopReason::NativeFailure)?;
        this.drain()?;
        Ok(this)
    }
    fn credentials(&self, name: Option<&CStr>, expected: u32) -> Result<(), StopReason> {
        let mut creds = ptr::null_mut();
        // EUID only, WITHOUT SD_BUS_CREDS_AUGMENT's race-prone /proc augmentation.
        let result = unsafe {
            match name {
                Some(name) => {
                    sd_bus_get_name_creds(self.bus.as_ptr(), name.as_ptr(), 1 << 4, &raw mut creds)
                }
                None => sd_bus_get_owner_creds(self.bus.as_ptr(), 1 << 4, &raw mut creds),
            }
        };
        let mut uid = u32::MAX;
        let valid = result >= 0
            && !creds.is_null()
            && unsafe { sd_bus_creds_get_euid(creds, &raw mut uid) } >= 0
            && uid == expected;
        if !creds.is_null() {
            unsafe {
                sd_bus_creds_unref(creds);
            }
        }
        if valid {
            Ok(())
        } else {
            Err(StopReason::UntrustedService)
        }
    }
    fn call(
        &self,
        destination: &CStr,
        path: &CStr,
        interface: &CStr,
        method: &CStr,
        arg: &CStr,
    ) -> Result<Message, StopReason> {
        let mut message = ptr::null_mut();
        let result = unsafe {
            sd_bus_call_method(
                self.bus.as_ptr(),
                destination.as_ptr(),
                path.as_ptr(),
                interface.as_ptr(),
                method.as_ptr(),
                ptr::null_mut(),
                &raw mut message,
                c"s".as_ptr(),
                arg.as_ptr(),
            )
        };
        if result < 0 {
            if !message.is_null() {
                unsafe {
                    sd_bus_message_unref(message);
                }
            }
            return Err(StopReason::BusUnavailable);
        }
        Ok(Message(NonNull::new(message).ok_or(StopReason::Malformed)?))
    }
    fn owner(&self) -> Result<CString, StopReason> {
        let reply = self.call(DBUS, DBUS_PATH, DBUS, c"GetNameOwner", LOGIN)?;
        let cursor = Cursor(reply.0.as_ptr());
        cursor.signature(c"s")?;
        let owner = cursor.text(b's')?;
        if owner.len() > 64
            || !owner.starts_with(':')
            || !owner[1..].bytes().all(|b| b.is_ascii_digit() || b == b'.')
        {
            return Err(StopReason::Malformed);
        }
        CString::new(owner).map_err(|_| StopReason::Malformed)
    }
    pub(super) fn snapshot(&mut self) -> Result<Snapshot, StopReason> {
        if self.owner()? != self.signals.owner {
            return Err(StopReason::IdentityChanged);
        }
        for property in [c"PreparingForSleep", c"PreparingForShutdown"] {
            let mut preparing = 0_i32;
            ok(unsafe {
                sd_bus_get_property_trivial(
                    self.bus.as_ptr(),
                    self.signals.owner.as_ptr(),
                    MANAGER.as_ptr(),
                    c"org.freedesktop.login1.Manager".as_ptr(),
                    property.as_ptr(),
                    ptr::null_mut(),
                    b'b'.cast_signed(),
                    (&raw mut preparing).cast(),
                )
            })?;
            if preparing != 0 {
                return Err(StopReason::Suspending);
            }
        }
        let path = self.signals.path.get().ok_or(StopReason::Malformed)?;
        let reply = self.call(&self.signals.owner, path, PROPERTIES, c"GetAll", SESSION)?;
        Snapshot::decode(&Cursor(reply.0.as_ptr()))
    }
    pub(super) fn drain(&mut self) -> Result<(), StopReason> {
        // Callbacks also run while synchronous methods are pending. Do not clear
        // their terminal latch or treat a later positive reply as replacement.
        for _ in 0..32 {
            let n = unsafe { sd_bus_process(self.bus.as_ptr(), ptr::null_mut()) };
            ok(n)?;
            if let super::Status::Stopped(reason) = self.signals.shared.status(boottime()) {
                return Err(reason);
            }
            if n == 0 {
                self.signals.count.set(0);
                return Ok(());
            }
        }
        Err(StopReason::EventFlood)
    }
}

#[cfg(test)]
pub(super) mod fixture;
