//! Thread-confined libsystemd boundary. No callback or Rust pointer is retained
//! by C; every returned message is uniquely owned and released on this thread.
use super::Error;
use std::{
    ffi::{CStr, CString, c_char, c_int, c_long, c_void},
    marker::PhantomData,
    ptr::{self, NonNull},
    rc::Rc,
    time::Duration,
};

pub(super) type Raw = *mut c_void;

// Signatures follow systemd's sd-bus.h; opaque message/bus pointers never leave
// this module. As in logind/bus.rs, link the installed system SONAME directly.
#[link(name = "libsystemd.so.0", kind = "dylib", modifiers = "+verbatim")]
unsafe extern "C" {
    fn sd_bus_new(bus: *mut Raw) -> c_int;
    fn sd_bus_set_address(bus: Raw, address: *const c_char) -> c_int;
    fn sd_bus_set_bus_client(bus: Raw, client: c_int) -> c_int;
    fn sd_bus_negotiate_fds(bus: Raw, enabled: c_int) -> c_int;
    fn sd_bus_start(bus: Raw) -> c_int;
    fn sd_bus_is_ready(bus: Raw) -> c_int;
    fn sd_bus_get_unique_name(bus: Raw, name: *mut *const c_char) -> c_int;
    fn sd_bus_get_owner_creds(bus: Raw, mask: u64, creds: *mut Raw) -> c_int;
    fn sd_bus_creds_get_euid(creds: Raw, uid: *mut u32) -> c_int;
    fn sd_bus_creds_unref(creds: Raw) -> Raw;
    fn sd_bus_close_unref(bus: Raw) -> Raw;
    fn sd_bus_process(bus: Raw, message: *mut Raw) -> c_int;
    fn sd_bus_wait(bus: Raw, timeout_usec: u64) -> c_int;
    fn sd_bus_send(bus: Raw, message: Raw, cookie: *mut u64) -> c_int;
    fn sd_bus_message_new_method_call(
        bus: Raw,
        message: *mut Raw,
        destination: *const c_char,
        path: *const c_char,
        interface: *const c_char,
        member: *const c_char,
    ) -> c_int;
    fn sd_bus_message_unref(message: Raw) -> Raw;
    fn sd_bus_message_get_type(message: Raw, kind: *mut u8) -> c_int;
    fn sd_bus_message_get_reply_cookie(message: Raw, cookie: *mut u64) -> c_int;
    fn sd_bus_message_get_sender(message: Raw) -> *const c_char;
    fn sd_bus_message_get_path(message: Raw) -> *const c_char;
    fn sd_bus_message_is_signal(
        message: Raw,
        interface: *const c_char,
        member: *const c_char,
    ) -> c_int;
    fn sd_bus_message_is_method_error(message: Raw, name: *const c_char) -> c_int;
    fn sd_bus_message_has_signature(message: Raw, signature: *const c_char) -> c_int;
    fn sd_bus_message_append_basic(message: Raw, kind: c_char, value: *const c_void) -> c_int;
    fn sd_bus_message_read_basic(message: Raw, kind: c_char, value: Raw) -> c_int;
    fn sd_bus_message_open_container(message: Raw, kind: c_char, contents: *const c_char) -> c_int;
    fn sd_bus_message_close_container(message: Raw) -> c_int;
    fn sd_bus_message_enter_container(message: Raw, kind: c_char, contents: *const c_char)
    -> c_int;
    fn sd_bus_message_exit_container(message: Raw) -> c_int;
    fn sd_bus_message_at_end(message: Raw, complete: c_int) -> c_int;
    fn sd_bus_message_skip(message: Raw, signature: *const c_char) -> c_int;
    fn strnlen(text: *const c_char, limit: usize) -> usize;
    fn clock_gettime(clock: c_int, time: *mut Timespec) -> c_int;
    fn getrandom(buffer: *mut c_void, count: usize, flags: u32) -> isize;
    fn geteuid() -> u32;
    #[cfg(test)]
    fn sd_bus_set_anonymous(bus: Raw, enabled: c_int) -> c_int;
}

fn checked(value: c_int) -> Result<(), Error> {
    if value < 0 {
        Err(Error::Unavailable)
    } else {
        Ok(())
    }
}
fn read(value: c_int) -> Result<(), Error> {
    if value <= 0 {
        Err(Error::InvalidSelection)
    } else {
        Ok(())
    }
}
fn code(value: u8) -> c_char {
    c_char::from_ne_bytes([value])
}

// SAFETY: caller supplies NULL or a live NUL-terminated sd-bus string. Copying
// is bounded and completes while its owner remains alive on the same thread.
unsafe fn text(pointer: *const c_char, limit: usize) -> Result<String, Error> {
    // SAFETY: live nullable C string per contract; at most limit+1 bytes read.
    if pointer.is_null() || unsafe { strnlen(pointer, limit + 1) } > limit {
        return Err(Error::InvalidSelection);
    }
    // SAFETY: preceding bounded scan found NUL and the owner is still alive.
    unsafe { CStr::from_ptr(pointer) }
        .to_str()
        .map(str::to_owned)
        .map_err(|_| Error::InvalidSelection)
}

enum PeerIdentity {
    UnixUid,
    #[cfg(test)]
    AnonymousTest,
}

pub(super) struct Bus(NonNull<c_void>, PhantomData<Rc<()>>, PeerIdentity);
impl Bus {
    pub(super) fn open(address: &CStr) -> Result<Self, Error> {
        let mut raw = ptr::null_mut();
        // SAFETY: exclusive output scalar; successful new returns our reference.
        checked(unsafe { sd_bus_new(&raw mut raw) })?;
        let identity = PeerIdentity::UnixUid;
        #[cfg(test)]
        let identity = if address.to_bytes().starts_with(b"tcp:") {
            PeerIdentity::AnonymousTest
        } else {
            identity
        };
        let bus = Self(
            NonNull::new(raw).ok_or(Error::Unavailable)?,
            PhantomData,
            identity,
        );
        // SAFETY: unique live bus; sd-bus copies the bounded address string.
        checked(unsafe { sd_bus_set_address(raw, address.as_ptr()) })?;
        // SAFETY: unique bus before start; this selects authenticated bus mode.
        checked(unsafe { sd_bus_set_bus_client(raw, 1) })?;
        // SAFETY: unique bus before authentication. FileChooser exchanges only
        // typed values and URIs, so never negotiate incoming file descriptors.
        checked(unsafe { sd_bus_negotiate_fds(raw, 0) })?;
        #[cfg(test)]
        if address.to_bytes().starts_with(b"tcp:") {
            // SAFETY: private test broker only. Production permits only unix:
            // addresses and never enables anonymous authentication.
            checked(unsafe { sd_bus_set_anonymous(raw, 1) })?;
        }
        // SAFETY: configured unique bus. start queues asynchronous Hello; no
        // user interaction or synchronous method call is performed here.
        checked(unsafe { sd_bus_start(raw) })?;
        Ok(bus)
    }
    pub(super) fn ready(&self) -> Result<bool, Error> {
        // SAFETY: live bus, accessed only on its original thread.
        let value = unsafe { sd_bus_is_ready(self.0.as_ptr()) };
        checked(value)?;
        Ok(value > 0)
    }
    pub(super) fn requires_peer_uid(&self) -> bool {
        // The production type contains only UnixUid. Only private test
        // brokers can select an identity without Unix peer credentials.
        matches!(self.2, PeerIdentity::UnixUid)
    }
    pub(super) fn verify_owner_uid(&self) -> Result<(), Error> {
        if !self.requires_peer_uid() {
            return Ok(());
        }
        let mut credentials = ptr::null_mut();
        // SAFETY: live ready Unix bus and exclusive output. EUID-only metadata
        // comes from the connected peer socket, without /proc augmentation or
        // a synchronous D-Bus method call. One returned reference is ours.
        let result =
            unsafe { sd_bus_get_owner_creds(self.0.as_ptr(), 1 << 4, &raw mut credentials) };
        let mut uid = u32::MAX;
        let trusted = result >= 0
            && !credentials.is_null()
            // SAFETY: preceding successful query returned live credentials.
            && unsafe { sd_bus_creds_get_euid(credentials, &raw mut uid) } >= 0
            && uid == effective_uid();
        if !credentials.is_null() {
            // SAFETY: release exactly the reference returned above.
            unsafe { sd_bus_creds_unref(credentials) };
        }
        if trusted {
            Ok(())
        } else {
            Err(Error::Unavailable)
        }
    }
    pub(super) fn unique_name(&self) -> Result<String, Error> {
        let mut name = ptr::null();
        // SAFETY: live bus and writable pointer; borrowed string lasts with bus.
        checked(unsafe { sd_bus_get_unique_name(self.0.as_ptr(), &raw mut name) })?;
        // SAFETY: live nullable bus-owned string, copied before returning.
        unsafe { text(name, 255) }
    }
    pub(super) fn process(&self) -> Result<(bool, Option<Message>), Error> {
        let mut message = ptr::null_mut();
        // SAFETY: exclusive thread owns bus; each returned message is one owned
        // reference. No callback or other caller can process it concurrently.
        let result = unsafe { sd_bus_process(self.0.as_ptr(), &raw mut message) };
        let message = NonNull::new(message).map(Message);
        checked(result)?;
        Ok((result > 0, message))
    }
    pub(super) fn wait(&self, duration: Duration) -> Result<(), Error> {
        let micros = u64::try_from(duration.as_micros()).map_err(|_| Error::Clock)?;
        // SAFETY: live bus; caller supplies a bounded remaining cleanup wait.
        checked(unsafe { sd_bus_wait(self.0.as_ptr(), micros) })
    }
    pub(super) fn call(
        &self,
        destination: &CStr,
        path: &CStr,
        interface: &CStr,
        member: &CStr,
    ) -> Result<Message, Error> {
        let mut message = ptr::null_mut();
        // SAFETY: live bus and C strings; sd-bus copies headers and returns one
        // new message reference through exclusive output storage.
        checked(unsafe {
            sd_bus_message_new_method_call(
                self.0.as_ptr(),
                &raw mut message,
                destination.as_ptr(),
                path.as_ptr(),
                interface.as_ptr(),
                member.as_ptr(),
            )
        })?;
        NonNull::new(message).map(Message).ok_or(Error::Unavailable)
    }
    pub(super) fn send(&self, message: &Message) -> Result<u64, Error> {
        let mut cookie = 0;
        // SAFETY: live bus/message. sd-bus retains queued bytes as necessary;
        // the cookie is only a scalar and no Rust pointer is retained.
        checked(unsafe { sd_bus_send(self.0.as_ptr(), message.0.as_ptr(), &raw mut cookie) })?;
        if cookie == 0 {
            Err(Error::Unavailable)
        } else {
            Ok(cookie)
        }
    }
    #[cfg(test)]
    pub(super) fn raw(&self) -> Raw {
        self.0.as_ptr()
    }
}
impl Drop for Bus {
    fn drop(&mut self) {
        // SAFETY: this private connection has one owner and no floating slots.
        // close discards queued messages before unref; no default bus is touched.
        unsafe {
            sd_bus_close_unref(self.0.as_ptr());
        }
    }
}

pub(super) struct Message(NonNull<c_void>);
impl Message {
    fn kind(&self) -> Result<u8, Error> {
        let mut kind = 0;
        // SAFETY: live owned message and writable byte output.
        checked(unsafe { sd_bus_message_get_type(self.0.as_ptr(), &raw mut kind) })?;
        Ok(kind)
    }
    pub(super) fn reply_cookie(&self) -> Result<Option<u64>, Error> {
        if !matches!(self.kind()?, 2 | 3) {
            return Ok(None);
        }
        let mut cookie = 0;
        // SAFETY: live reply message and writable u64 output.
        checked(unsafe { sd_bus_message_get_reply_cookie(self.0.as_ptr(), &raw mut cookie) })?;
        Ok(Some(cookie))
    }
    pub(super) fn from(&self, sender: &CStr) -> Result<bool, Error> {
        // SAFETY: returned nullable sender string belongs to this live message.
        let value = unsafe { text(sd_bus_message_get_sender(self.0.as_ptr()), 255) }?;
        Ok(value.as_bytes() == sender.to_bytes())
    }
    pub(super) fn on_path(&self, path: &CStr) -> Result<bool, Error> {
        // SAFETY: returned nullable path belongs to this live message.
        let value = unsafe { text(sd_bus_message_get_path(self.0.as_ptr()), 512) }?;
        Ok(value.as_bytes() == path.to_bytes())
    }
    pub(super) fn success_from(&self, sender: &CStr) -> Result<(), Error> {
        if self.kind()? != 2 || !self.from(sender)? {
            return Err(Error::Unavailable);
        }
        Ok(())
    }
    pub(super) fn is_error(&self, name: &CStr) -> Result<bool, Error> {
        // SAFETY: live message and static NUL-terminated error name.
        let value = unsafe { sd_bus_message_is_method_error(self.0.as_ptr(), name.as_ptr()) };
        checked(value)?;
        Ok(value > 0)
    }
    pub(super) fn is_signal(&self, interface: &CStr, member: &CStr) -> Result<bool, Error> {
        // SAFETY: live message and NUL-terminated static header strings.
        let value = unsafe {
            sd_bus_message_is_signal(self.0.as_ptr(), interface.as_ptr(), member.as_ptr())
        };
        checked(value)?;
        Ok(value > 0)
    }
    pub(super) fn signature(&self, signature: &CStr) -> Result<(), Error> {
        // SAFETY: live message and static bounded signature string.
        read(unsafe { sd_bus_message_has_signature(self.0.as_ptr(), signature.as_ptr()) })
    }
    pub(super) fn text(&self, value: &CStr) -> Result<(), Error> {
        // SAFETY: for string basic types sd-bus takes the string directly and
        // copies it into the live writable message; no pointer is retained.
        checked(unsafe {
            sd_bus_message_append_basic(self.0.as_ptr(), code(b's'), value.as_ptr().cast())
        })
    }
    pub(super) fn number(&self, value: u32) -> Result<(), Error> {
        // SAFETY: 'u' reads one uint32_t and copies it into this live message.
        checked(unsafe {
            sd_bus_message_append_basic(self.0.as_ptr(), code(b'u'), (&raw const value).cast())
        })
    }
    pub(super) fn boolean_option(&self, name: &CStr, value: bool) -> Result<(), Error> {
        self.open(b'e', c"sv")?;
        self.text(name)?;
        self.open(b'v', c"b")?;
        let value = c_int::from(value);
        // SAFETY: D-Bus boolean uses a C int; sd-bus copies the scalar.
        checked(unsafe {
            sd_bus_message_append_basic(self.0.as_ptr(), code(b'b'), (&raw const value).cast())
        })?;
        self.close()?;
        self.close()
    }
    pub(super) fn string_option(&self, name: &CStr, value: &CStr) -> Result<(), Error> {
        self.open(b'e', c"sv")?;
        self.text(name)?;
        self.open(b'v', c"s")?;
        self.text(value)?;
        self.close()?;
        self.close()
    }
    pub(super) fn open(&self, kind: u8, contents: &CStr) -> Result<(), Error> {
        // SAFETY: live writable message; library validates container structure.
        checked(unsafe {
            sd_bus_message_open_container(self.0.as_ptr(), code(kind), contents.as_ptr())
        })
    }
    pub(super) fn close(&self) -> Result<(), Error> {
        // SAFETY: live writable message; sd-bus validates current container.
        checked(unsafe { sd_bus_message_close_container(self.0.as_ptr()) })
    }
    pub(super) fn enter(&self, kind: u8, contents: &CStr) -> Result<bool, Error> {
        // SAFETY: live sealed message; library checks the requested signature.
        let value = unsafe {
            sd_bus_message_enter_container(self.0.as_ptr(), code(kind), contents.as_ptr())
        };
        if value < 0 {
            return Err(Error::InvalidSelection);
        }
        Ok(value > 0)
    }
    pub(super) fn require_container(&self, kind: u8, contents: &CStr) -> Result<(), Error> {
        if self.enter(kind, contents)? {
            Ok(())
        } else {
            Err(Error::InvalidSelection)
        }
    }
    pub(super) fn leave(&self) -> Result<(), Error> {
        // SAFETY: live message; sd-bus validates reader container nesting.
        checked(unsafe { sd_bus_message_exit_container(self.0.as_ptr()) })
            .map_err(|_| Error::InvalidSelection)
    }
    pub(super) fn read_number(&self) -> Result<u32, Error> {
        let mut value = 0_u32;
        // SAFETY: 'u' writes one uint32_t to this exclusive scalar.
        read(unsafe {
            sd_bus_message_read_basic(self.0.as_ptr(), code(b'u'), (&raw mut value).cast())
        })?;
        Ok(value)
    }
    pub(super) fn read_text(&self, kind: u8, limit: usize) -> Result<String, Error> {
        if !matches!(kind, b's' | b'o') {
            return Err(Error::InvalidSelection);
        }
        let mut value: *const c_char = ptr::null();
        // SAFETY: string/object-path types write one borrowed const char*;
        // this message remains alive through the bounded copy below.
        read(unsafe {
            sd_bus_message_read_basic(self.0.as_ptr(), code(kind), (&raw mut value).cast())
        })?;
        // SAFETY: nullable borrowed message string, still alive on this thread.
        unsafe { text(value, limit) }
    }
    pub(super) fn skip(&self, signature: &CStr) -> Result<(), Error> {
        // SAFETY: live reader; static signature and library-checked structure.
        read(unsafe { sd_bus_message_skip(self.0.as_ptr(), signature.as_ptr()) })
    }
    pub(super) fn at_end(&self) -> Result<bool, Error> {
        // SAFETY: live reader; query only, at current container depth.
        let value = unsafe { sd_bus_message_at_end(self.0.as_ptr(), 0) };
        checked(value)?;
        Ok(value > 0)
    }
    #[cfg(test)]
    pub(super) fn raw(&self) -> Raw {
        self.0.as_ptr()
    }
    /// # Safety
    /// The caller transfers exactly one owned non-null sd-bus message reference.
    #[cfg(test)]
    pub(super) unsafe fn from_raw(raw: Raw) -> Self {
        Self(NonNull::new(raw).expect("test transferred owned message"))
    }
}
impl Drop for Message {
    fn drop(&mut self) {
        // SAFETY: exactly one owned message reference, with no borrowed users.
        unsafe {
            sd_bus_message_unref(self.0.as_ptr());
        }
    }
}

#[repr(C)]
struct Timespec {
    sec: c_long,
    nanos: c_long,
}
pub(super) fn now() -> Result<u64, Error> {
    let mut value = Timespec { sec: 0, nanos: 0 };
    // SAFETY: Linux CLOCK_BOOTTIME=7, writable ABI-correct timespec. Including
    // suspend prevents an old dialog from receiving a new lifetime after wake.
    if unsafe { clock_gettime(7, &raw mut value) } != 0
        || !(0..1_000_000_000).contains(&value.nanos)
    {
        return Err(Error::Clock);
    }
    u64::try_from(value.sec)
        .ok()
        .and_then(|seconds| seconds.checked_mul(1_000_000))
        .and_then(|micros| micros.checked_add(u64::try_from(value.nanos).ok()? / 1_000))
        .ok_or(Error::Clock)
}
pub(super) fn effective_uid() -> u32 {
    // SAFETY: no arguments, pointers or mutation in geteuid.
    unsafe { geteuid() }
}
pub(super) fn token() -> Result<CString, Error> {
    let mut bytes = [0_u8; 16];
    // SAFETY: exclusive fixed buffer; Linux GRND_NONBLOCK=1. Missing kernel
    // entropy refuses this original choice rather than guessing request IDs.
    let count = unsafe { getrandom(bytes.as_mut_ptr().cast(), bytes.len(), 1) };
    if count != 16 {
        return Err(Error::Unavailable);
    }
    let mut value = String::with_capacity(48);
    value.push_str("fr_receive_");
    for byte in bytes {
        use std::fmt::Write;
        write!(value, "{byte:02x}").map_err(|_| Error::Unavailable)?;
    }
    CString::new(value).map_err(|_| Error::Unavailable)
}
