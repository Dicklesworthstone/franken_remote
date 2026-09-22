//! Actual private dbus-daemon and libsystemd peer. All logind data is SYNTHETIC;
//! never impersonates a service on the installed system bus or changes OS state.
use super::*;
use std::{
    fs,
    io::{BufRead, BufReader},
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::{
        Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
unsafe extern "C" {
    fn sd_bus_request_name(bus: Raw, name: *const c_char, flags: u64) -> c_int;
    fn sd_bus_release_name(bus: Raw, name: *const c_char) -> c_int;
    fn sd_bus_add_object(
        bus: Raw,
        slot: *mut Raw,
        path: *const c_char,
        handler: Option<Handler>,
        data: Raw,
    ) -> c_int;
    fn sd_bus_message_new_method_return(call: Raw, message: *mut Raw) -> c_int;
    fn sd_bus_message_new_signal(
        bus: Raw,
        message: *mut Raw,
        path: *const c_char,
        interface: *const c_char,
        member: *const c_char,
    ) -> c_int;
    fn sd_bus_message_append_basic(message: Raw, ty: c_char, value: *const c_void) -> c_int;
    fn sd_bus_message_open_container(message: Raw, ty: c_char, contents: *const c_char) -> c_int;
    fn sd_bus_message_close_container(message: Raw) -> c_int;
    fn sd_bus_send(bus: Raw, message: Raw, cookie: *mut u64) -> c_int;
    fn sd_bus_flush(bus: Raw) -> c_int;
    fn geteuid() -> u32;
}
const PATH: &CStr = c"/org/freedesktop/login1/session/c1";
static NEXT: AtomicU64 = AtomicU64::new(0);
pub(in crate::logind) static SERIAL: Mutex<()> = Mutex::new(());
pub(in crate::logind) fn uid() -> u32 {
    unsafe { geteuid() }
}
pub(in crate::logind) fn selection() -> Selection {
    Selection {
        session: "c1".into(),
        uid: 1000,
        seat: "seat0".into(),
        display: ":7".into(),
    }
}
#[allow(clippy::struct_excessive_bools)]
#[derive(Clone)]
pub(in crate::logind) struct Data {
    pub active: bool,
    pub locked: bool,
    pub can_lock: bool,
    pub remote: bool,
    pub suspend: bool,
    pub uid: u32,
    pub display: String,
    pub stamp: u64,
    pub omit: Option<&'static str>,
    pub initial_signal: bool,
    pub no_reply: bool,
}
impl Default for Data {
    fn default() -> Self {
        Self {
            active: true,
            locked: false,
            can_lock: true,
            remote: false,
            suspend: false,
            uid: 1000,
            display: ":7".into(),
            stamp: 12345,
            omit: None,
            initial_signal: false,
            no_reply: false,
        }
    }
}
#[derive(Clone, Copy)]
pub(in crate::logind) enum Event {
    LockUnlock,
    Inactive,
    Suspend,
    Removed,
    OwnerLost,
    Invalidated,
    Unrelated,
}
pub(in crate::logind) struct Peer {
    pub address: String,
    pub data: Arc<Mutex<Data>>,
    root: PathBuf,
    daemon: Child,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
    events: mpsc::SyncSender<Event>,
}
struct ServerState {
    bus: Raw,
    data: Arc<Mutex<Data>>,
}
// Private test message writer. Owned messages unref after sd_bus_send retains them.
struct Writer(Message);
impl Writer {
    fn raw(&self) -> Raw {
        self.0.0.as_ptr()
    }
    fn open(&self, ty: u8, signature: &CStr) {
        assert!(
            unsafe {
                sd_bus_message_open_container(self.raw(), ty.cast_signed(), signature.as_ptr())
            } >= 0
        );
    }
    fn close(&self) {
        assert!(unsafe { sd_bus_message_close_container(self.raw()) } >= 0);
    }
    fn text(&self, ty: u8, text: &str) {
        let text = CString::new(text).unwrap();
        assert!(
            unsafe {
                sd_bus_message_append_basic(self.raw(), ty.cast_signed(), text.as_ptr().cast())
            } >= 0
        );
    }
    fn boolean(&self, value: bool) {
        let n = i32::from(value);
        assert!(
            unsafe {
                sd_bus_message_append_basic(self.raw(), b'b'.cast_signed(), (&raw const n).cast())
            } >= 0
        );
    }
    fn number(&self, n: u32) {
        assert!(
            unsafe {
                sd_bus_message_append_basic(self.raw(), b'u'.cast_signed(), (&raw const n).cast())
            } >= 0
        );
    }
    fn timestamp(&self, n: u64) {
        assert!(
            unsafe {
                sd_bus_message_append_basic(self.raw(), b't'.cast_signed(), (&raw const n).cast())
            } >= 0
        );
    }
    fn field(&self, field: &str, sig: &CStr, f: impl FnOnce(&Self)) {
        self.open(b'e', c"sv");
        self.text(b's', field);
        self.open(b'v', sig);
        f(self);
        self.close();
        self.close();
    }
    fn send(self, bus: Raw) {
        assert!(unsafe { sd_bus_send(bus, self.raw(), ptr::null_mut()) } >= 0);
    }
}
fn new_signal(bus: Raw, path: &CStr, interface: &CStr, member: &CStr) -> Writer {
    let mut raw = ptr::null_mut();
    assert!(
        unsafe {
            sd_bus_message_new_signal(
                bus,
                &raw mut raw,
                path.as_ptr(),
                interface.as_ptr(),
                member.as_ptr(),
            )
        } >= 0
    );
    Writer(Message(NonNull::new(raw).unwrap()))
}
fn emit(bus: Raw, event: Event) {
    match event {
        Event::OwnerLost => {
            assert!(unsafe { sd_bus_release_name(bus, LOGIN.as_ptr()) } >= 0);
        }
        Event::LockUnlock => {
            new_signal(bus, PATH, SESSION, c"Lock").send(bus);
            new_signal(bus, PATH, SESSION, c"Unlock").send(bus);
        }
        Event::Suspend => {
            let w = new_signal(
                bus,
                MANAGER,
                c"org.freedesktop.login1.Manager",
                c"PrepareForSleep",
            );
            w.boolean(true);
            w.send(bus);
        }
        Event::Removed => {
            let w = new_signal(
                bus,
                MANAGER,
                c"org.freedesktop.login1.Manager",
                c"SessionRemoved",
            );
            w.text(b's', "c1");
            w.text(b'o', PATH.to_str().unwrap());
            w.send(bus);
        }
        Event::Inactive | Event::Invalidated | Event::Unrelated => {
            let unrelated = matches!(event, Event::Unrelated);
            let w = new_signal(
                bus,
                if unrelated {
                    c"/org/freedesktop/login1/session/other"
                } else {
                    PATH
                },
                PROPERTIES,
                c"PropertiesChanged",
            );
            w.text(b's', SESSION.to_str().unwrap());
            w.open(b'a', c"{sv}");
            if !matches!(event, Event::Invalidated) {
                w.field("Active", c"b", |w| w.boolean(false));
            }
            w.close();
            w.open(b'a', c"s");
            if matches!(event, Event::Invalidated) {
                w.text(b's', "LockedHint");
            }
            w.close();
            w.send(bus);
        }
    }
    assert!(unsafe { sd_bus_flush(bus) } >= 0);
}
fn reply(call: Raw, state: &ServerState) -> Result<(), StopReason> {
    let member = unsafe { string(sd_bus_message_get_member(call)) }?;
    let cursor = Cursor(call);
    let data = state.data.lock().unwrap().clone();
    if data.no_reply {
        return Ok(());
    }
    let mut raw = ptr::null_mut();
    ok(unsafe { sd_bus_message_new_method_return(call, &raw mut raw) })?;
    let w = Writer(Message(NonNull::new(raw).ok_or(StopReason::Malformed)?));
    match member.as_str() {
        "GetSession" => {
            assert_eq!(cursor.text(b's')?, "c1");
            w.text(b'o', PATH.to_str().unwrap());
        }
        "Get" => {
            assert_eq!(cursor.text(b's')?, "org.freedesktop.login1.Manager");
            let p = cursor.text(b's')?;
            assert!(p == "PreparingForSleep" || p == "PreparingForShutdown");
            w.open(b'v', c"b");
            w.boolean(data.suspend);
            w.close();
        }
        "GetAll" => {
            assert_eq!(cursor.text(b's')?, SESSION.to_str().unwrap());
            if data.initial_signal {
                emit(state.bus, Event::LockUnlock);
            }
            w.open(b'a', c"{sv}");
            for (key, value) in [
                ("Id", "c1"),
                ("Display", data.display.as_str()),
                ("Type", "x11"),
                ("Class", "user"),
                ("State", "active"),
            ] {
                if data.omit != Some(key) {
                    w.field(key, c"s", |w| w.text(b's', value));
                }
            }
            for (key, value) in [
                ("Active", data.active),
                ("Remote", data.remote),
                ("LockedHint", data.locked),
                ("CanLock", data.can_lock),
            ] {
                if data.omit != Some(key) {
                    w.field(key, c"b", |w| w.boolean(value));
                }
            }
            if data.omit != Some("User") {
                w.field("User", c"(uo)", |w| {
                    w.open(b'r', c"uo");
                    w.number(data.uid);
                    w.text(b'o', "/org/freedesktop/login1/user/_1000");
                    w.close();
                });
            }
            if data.omit != Some("Seat") {
                w.field("Seat", c"(so)", |w| {
                    w.open(b'r', c"so");
                    w.text(b's', "seat0");
                    w.text(b'o', "/org/freedesktop/login1/seat/seat0");
                    w.close();
                });
            }
            if data.omit != Some("TimestampMonotonic") {
                w.field("TimestampMonotonic", c"t", |w| w.timestamp(data.stamp));
            }
            if data.omit != Some("Leader") {
                w.field("Leader", c"u", |w| w.number(123));
            }
            w.close();
        }
        _ => return Err(StopReason::Malformed),
    }
    w.send(state.bus);
    Ok(())
}
unsafe extern "C" fn method(call: Raw, data: Raw, _: Raw) -> c_int {
    let state = unsafe { &*data.cast::<ServerState>() };
    // Test failures are signalled by withholding a reply, never unwind across C.
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| reply(call, state).unwrap()));
    1
}
impl Peer {
    pub fn new(data: Data) -> Self {
        let root = std::env::temp_dir().join(format!(
            "fr-logind-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        let mut daemon = Command::new("/usr/bin/dbus-daemon")
            .env_clear()
            .args(["--session", "--nofork", "--print-address=1"])
            .arg(format!("--address=unix:path={}/bus", root.display()))
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let mut address = String::new();
        BufReader::new(daemon.stdout.take().unwrap())
            .read_line(&mut address)
            .unwrap();
        let address = address.trim().to_owned();
        let endpoint = address.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let quit = stop.clone();
        let data = Arc::new(Mutex::new(data));
        let values = data.clone();
        let (ready, waiting) = mpsc::sync_channel(1);
        let (events, pending) = mpsc::sync_channel(8);
        let worker = thread::spawn(move || {
            let mut raw = ptr::null_mut();
            assert!(unsafe { sd_bus_new(&raw mut raw) } >= 0);
            let mut state = Box::new(ServerState {
                bus: raw,
                data: values,
            });
            let endpoint = CString::new(endpoint).unwrap();
            assert!(unsafe { sd_bus_set_address(raw, endpoint.as_ptr()) } >= 0);
            assert!(unsafe { sd_bus_set_bus_client(raw, 1) } >= 0);
            assert!(unsafe { sd_bus_start(raw) } >= 0);
            assert!(unsafe { sd_bus_request_name(raw, LOGIN.as_ptr(), 0) } >= 0);
            for path in [MANAGER, PATH] {
                assert!(
                    unsafe {
                        sd_bus_add_object(
                            raw,
                            ptr::null_mut(),
                            path.as_ptr(),
                            Some(method),
                            (&raw mut *state).cast(),
                        )
                    } >= 0
                );
            }
            ready.send(()).unwrap();
            while !quit.load(Ordering::Acquire) {
                if let Ok(event) = pending.try_recv() {
                    emit(raw, event);
                }
                let result = unsafe { sd_bus_process(raw, ptr::null_mut()) };
                if result < 0 {
                    break;
                }
                if result == 0 {
                    thread::sleep(Duration::from_millis(1));
                }
            }
            unsafe {
                sd_bus_close_unref(raw);
            }
        });
        waiting.recv_timeout(Duration::from_secs(3)).unwrap();
        Self {
            address,
            data,
            root,
            daemon,
            stop,
            worker: Some(worker),
            events,
        }
    }
    pub fn spoof_lock(&self) {
        // Same UID and bus, but NOT the uniquely authenticated login1 owner.
        let mut raw = ptr::null_mut();
        let address = CString::new(self.address.as_bytes()).unwrap();
        assert!(unsafe { sd_bus_new(&raw mut raw) } >= 0);
        assert!(unsafe { sd_bus_set_address(raw, address.as_ptr()) } >= 0);
        assert!(unsafe { sd_bus_set_bus_client(raw, 1) } >= 0);
        assert!(unsafe { sd_bus_start(raw) } >= 0);
        emit(raw, Event::LockUnlock);
        unsafe {
            sd_bus_close_unref(raw);
        }
    }
    pub fn emit(&self, event: Event) {
        self.events.try_send(event).unwrap();
    }
}
impl Drop for Peer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        let until = Instant::now() + Duration::from_secs(3);
        while self.worker.as_ref().is_some_and(|w| !w.is_finished()) {
            assert!(
                Instant::now() < until,
                "test peer retirement exceeded bound"
            );
            thread::sleep(Duration::from_millis(1));
        }
        if let Some(w) = self.worker.take() {
            w.join().unwrap();
        }
        self.daemon.kill().unwrap();
        self.daemon.wait().unwrap();
        fs::remove_dir_all(&self.root).unwrap();
    }
}
