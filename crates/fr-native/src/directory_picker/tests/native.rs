//! Real sd-bus framing and ownership against a private dbus-daemon.
//!
//! The portal service is a test fixture, not a desktop chooser. TCP/ANONYMOUS
//! applies only to this private broker because some runners prohibit `AF_UNIX`.
//! The production entry point still accepts only the installed local Unix bus.
use super::super::{Bus, DirectoryPicker, Error, FilePicker, Message, Phase, SelectionMode, bus};
use std::{
    ffi::{CStr, CString, c_char, c_int, c_void},
    fs::File,
    io::{BufRead, BufReader, Seek, SeekFrom, Write},
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
    path::PathBuf,
    process::{Child, Command, Stdio},
    ptr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, SyncSender},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

type Raw = *mut c_void;

#[link(name = "libsystemd.so.0", kind = "dylib", modifiers = "+verbatim")]
unsafe extern "C" {
    fn memfd_create(name: *const c_char, flags: u32) -> c_int;
    fn sd_bus_set_method_call_timeout(bus: Raw, micros: u64) -> c_int;
    fn sd_bus_request_name(bus: Raw, name: *const c_char, flags: u64) -> c_int;
    fn sd_bus_message_is_method_call(
        message: Raw,
        interface: *const c_char,
        member: *const c_char,
    ) -> c_int;
    fn sd_bus_message_get_sender(message: Raw) -> *const c_char;
    fn sd_bus_message_get_path(message: Raw) -> *const c_char;
    fn sd_bus_message_new_method_return(call: Raw, reply: *mut Raw) -> c_int;
    fn sd_bus_message_new_signal(
        bus: Raw,
        message: *mut Raw,
        path: *const c_char,
        interface: *const c_char,
        member: *const c_char,
    ) -> c_int;
    fn sd_bus_message_set_destination(message: Raw, destination: *const c_char) -> c_int;
    fn sd_bus_message_append_basic(message: Raw, kind: c_char, value: *const c_void) -> c_int;
    fn sd_bus_message_read_basic(message: Raw, kind: c_char, value: Raw) -> c_int;
}

const TEST_TIMEOUT: Duration = Duration::from_secs(8);
const BROKER: &[u8] = br#"<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN" "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
<busconfig>
 <type>session</type>
 <listen>tcp:host=127.0.0.1,port=0</listen>
 <auth>ANONYMOUS</auth><allow_anonymous/>
 <policy context="default">
  <allow user="*"/><allow own="*"/>
  <allow send_destination="*"/><allow receive_sender="*"/>
 </policy>
 <limit name="max_message_size">1048576</limit>
 <limit name="max_incoming_bytes">4194304</limit>
 <limit name="max_outgoing_bytes">4194304</limit>
 <limit name="max_completed_connections">16</limit>
 <limit name="max_incomplete_connections">16</limit>
</busconfig>"#;

struct Broker {
    child: Child,
    _configuration: File,
}
impl Broker {
    fn start() -> (Self, CString) {
        // SAFETY: static name and MFD_CLOEXEC; success returns one owned fd.
        let fd = unsafe { memfd_create(c"fr-portal-test-config".as_ptr(), 1) };
        assert!(fd >= 0, "private broker configuration allocation");
        // SAFETY: transfer the fresh uniquely owned fd to its Rust owner.
        let mut configuration = File::from(unsafe { OwnedFd::from_raw_fd(fd) });
        configuration.write_all(BROKER).unwrap();
        configuration.seek(SeekFrom::Start(0)).unwrap();
        // dbus-daemon loads a regular, sized configuration. Keeping this fd
        // alive avoids a named temporary file and prevents a replacement race.
        let config = format!(
            "--config-file=/proc/{}/fd/{}",
            std::process::id(),
            configuration.as_raw_fd()
        );
        let mut child = Command::new("dbus-daemon")
            .args([config.as_str(), "--nofork", "--print-address=1"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("native portal tests require installed dbus-daemon");
        let output = child.stdout.take().unwrap();
        let broker = Self {
            child,
            _configuration: configuration,
        };
        let (tx, rx) = mpsc::sync_channel(1);
        let reader = thread::spawn(move || {
            let mut address = String::new();
            BufReader::new(output).read_line(&mut address).unwrap();
            tx.send(address).unwrap();
        });
        let address = rx.recv_timeout(TEST_TIMEOUT).expect("private bus startup");
        reader.join().unwrap();
        assert!(address.starts_with("tcp:host=127.0.0.1,"));
        (broker, CString::new(address.trim()).unwrap())
    }
}
impl Drop for Broker {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[derive(Clone, Debug)]
struct Opening {
    sender: CString,
    path: CString,
    directory: bool,
    multiple: bool,
}
#[derive(Clone, Debug, Default)]
struct Observed {
    version_queries: usize,
    opening: Option<Opening>,
    opening_replies: usize,
    signals: usize,
    close_paths: Vec<String>,
    close_before_open_reply: bool,
}

enum Action {
    MalformedResponse,
    Reply {
        replacement: bool,
        delay: Duration,
    },
    Response {
        uris: Vec<String>,
        code: u32,
        foreign: bool,
        wrong_path: bool,
    },
}

struct Portal {
    // Keep the broker alive until the owning service thread has stopped.
    _broker: Broker,
    address: CString,
    actions: SyncSender<Action>,
    observed: Arc<Mutex<Observed>>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}
impl Portal {
    fn start(version: u32, acknowledge_close: bool) -> Self {
        let (broker, address) = Broker::start();
        let thread_address = address.clone();
        let (tx, rx) = mpsc::sync_channel(16);
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let observed = Arc::new(Mutex::new(Observed::default()));
        let thread_observed = Arc::clone(&observed);
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let worker = thread::spawn(move || {
            serve(
                &thread_address,
                version,
                acknowledge_close,
                &rx,
                &thread_observed,
                &thread_stop,
                &ready_tx,
            );
        });
        let portal = Self {
            _broker: broker,
            address,
            actions: tx,
            observed,
            stop,
            worker: Some(worker),
        };
        ready_rx
            .recv_timeout(TEST_TIMEOUT)
            .expect("private portal ownership");
        portal
    }
    fn picker(&self) -> DirectoryPicker {
        DirectoryPicker::open_at(&self.address, bus::now().unwrap()).unwrap()
    }
    fn state(&self) -> Observed {
        self.observed.lock().unwrap().clone()
    }
    fn reply(&self, replacement: bool) {
        self.actions
            .send(Action::Reply {
                replacement,
                delay: Duration::ZERO,
            })
            .unwrap();
    }
    fn chosen(&self, uris: Vec<&str>) {
        self.actions
            .send(Action::Response {
                uris: uris.into_iter().map(str::to_owned).collect(),
                code: 0,
                foreign: false,
                wrong_path: false,
            })
            .unwrap();
    }
    fn wait_open(&self, picker: &mut DirectoryPicker) {
        pending_until(picker, |_| self.state().opening.is_some());
    }
}
impl Drop for Portal {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let result = worker.join();
            if !thread::panicking() {
                result.unwrap();
            }
        }
    }
}

fn is_method(message: &Message, interface: &CStr, member: &CStr) -> bool {
    // SAFETY: live owned message and static NUL-terminated header strings.
    unsafe { sd_bus_message_is_method_call(message.raw(), interface.as_ptr(), member.as_ptr()) > 0 }
}
fn header(message: &Message, sender: bool) -> CString {
    // SAFETY: borrowed header remains valid while this owned message lives.
    let raw = unsafe {
        if sender {
            sd_bus_message_get_sender(message.raw())
        } else {
            sd_bus_message_get_path(message.raw())
        }
    };
    assert!(!raw.is_null());
    // SAFETY: sd-bus returns a terminated string, copied before message release.
    unsafe { CStr::from_ptr(raw) }.to_owned()
}
fn reply_message(message: &Message) -> Message {
    let mut raw = ptr::null_mut();
    // SAFETY: live method call, exclusive output; success returns one reference.
    assert!(unsafe { sd_bus_message_new_method_return(message.raw(), &raw mut raw) } >= 0);
    // SAFETY: the successfully returned owned reference is transferred once.
    unsafe { Message::from_raw(raw) }
}
fn object_path(message: &Message, path: &CStr) {
    // SAFETY: 'o' copies the C string into this live writable reply.
    let status = unsafe {
        sd_bus_message_append_basic(
            message.raw(),
            c_char::from_ne_bytes(*b"o"),
            path.as_ptr().cast(),
        )
    };
    assert!(status >= 0);
}
fn boolean(message: &Message) -> bool {
    let mut value: c_int = -1;
    // SAFETY: D-Bus boolean writes exactly one int into exclusive storage.
    let status = unsafe {
        sd_bus_message_read_basic(
            message.raw(),
            c_char::from_ne_bytes(*b"b"),
            (&raw mut value).cast(),
        )
    };
    assert!(status > 0);
    assert!(matches!(value, 0 | 1));
    value != 0
}

fn read_opening(message: &Message) -> Opening {
    message.signature(c"ssa{sv}").unwrap();
    assert_eq!(message.read_text(b's', 256).unwrap(), "");
    let title = message.read_text(b's', 256).unwrap();
    message.require_container(b'a', c"{sv}").unwrap();
    let (mut token, mut directory, mut multiple) = (None, None, None);
    let mut label = None;
    while message.enter(b'e', c"sv").unwrap() {
        match message.read_text(b's', 128).unwrap().as_str() {
            "handle_token" => {
                assert!(token.is_none());
                message.require_container(b'v', c"s").unwrap();
                token = Some(message.read_text(b's', 128).unwrap());
            }
            "directory" => {
                assert!(directory.is_none());
                message.require_container(b'v', c"b").unwrap();
                directory = Some(boolean(message));
            }
            "multiple" => {
                assert!(multiple.is_none());
                message.require_container(b'v', c"b").unwrap();
                multiple = Some(boolean(message));
            }
            "accept_label" => {
                assert!(label.is_none());
                message.require_container(b'v', c"s").unwrap();
                label = Some(message.read_text(b's', 128).unwrap());
            }
            name => panic!("unexpected option {name}"),
        }
        message.leave().unwrap();
        message.leave().unwrap();
    }
    message.leave().unwrap();
    assert!(message.at_end().unwrap());
    let directory = directory.unwrap_or(false);
    let multiple = multiple.unwrap();
    let (expected_title, expected_label) = match (directory, multiple) {
        (true, false) => ("Choose a folder to receive files", "Receive Here"),
        (false, true) => ("Choose files to send", "Send Files"),
        _ => panic!("chooser requested an unsupported selection mode"),
    };
    assert_eq!(title, expected_title);
    assert_eq!(label.as_deref(), Some(expected_label));
    let sender = header(message, true);
    let token = token.unwrap();
    assert!(token.starts_with("fr_receive_"));
    assert!(
        token
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_')
    );
    let path = CString::new(format!(
        "/org/freedesktop/portal/desktop/request/{}/{}",
        sender
            .to_str()
            .unwrap()
            .strip_prefix(':')
            .unwrap()
            .replace('.', "_"),
        token,
    ))
    .unwrap();
    Opening {
        sender,
        path,
        directory,
        multiple,
    }
}

fn respond(bus: &Bus, opening: &Opening, uris: &[String], code: u32, wrong_path: bool) {
    let path = if wrong_path {
        c"/org/freedesktop/portal/desktop/request/wrong/owner"
    } else {
        &opening.path
    };
    let mut raw = ptr::null_mut();
    // SAFETY: all pointers are live, and output receives one signal reference.
    let status = unsafe {
        sd_bus_message_new_signal(
            bus.raw(),
            &raw mut raw,
            path.as_ptr(),
            c"org.freedesktop.portal.Request".as_ptr(),
            c"Response".as_ptr(),
        )
    };
    assert!(status >= 0);
    // SAFETY: transfer the newly returned signal reference exactly once.
    let signal = unsafe { Message::from_raw(raw) };
    // SAFETY: signal and destination are live; library copies the header.
    assert!(unsafe { sd_bus_message_set_destination(signal.raw(), opening.sender.as_ptr()) } >= 0);
    signal.number(code).unwrap();
    signal.open(b'a', c"{sv}").unwrap();
    signal.open(b'e', c"sv").unwrap();
    signal.text(c"uris").unwrap();
    signal.open(b'v', c"as").unwrap();
    signal.open(b'a', c"s").unwrap();
    for uri in uris {
        signal.text(&CString::new(uri.as_str()).unwrap()).unwrap();
    }
    for _ in 0..4 {
        signal.close().unwrap();
    }
    bus.send(&signal).unwrap();
}

fn malformed_response(bus: &Bus, opening: &Opening) {
    let mut raw = ptr::null_mut();
    // SAFETY: live fixture bus, static headers and writable message output.
    let status = unsafe {
        sd_bus_message_new_signal(
            bus.raw(),
            &raw mut raw,
            opening.path.as_ptr(),
            c"org.freedesktop.portal.Request".as_ptr(),
            c"Response".as_ptr(),
        )
    };
    assert!(status >= 0);
    // SAFETY: transfer exactly the new owned signal reference.
    let message = unsafe { Message::from_raw(raw) };
    // SAFETY: live message and original destination; the library copies it.
    assert!(unsafe { sd_bus_message_set_destination(message.raw(), opening.sender.as_ptr()) } >= 0);
    message.text(c"invalid-terminal-signature").unwrap();
    bus.send(&message).unwrap();
}

struct Service<'a> {
    bus: Bus,
    address: &'a CStr,
    version: u32,
    acknowledge_close: bool,
    observed: &'a Mutex<Observed>,
    opening_call: Option<Message>,
    delayed_reply: Option<(Instant, bool)>,
    foreign_bus: Option<Bus>,
}

impl Service<'_> {
    fn action(&mut self, action: Action) {
        match action {
            Action::MalformedResponse => {
                let opening = self.observed.lock().unwrap().opening.clone().unwrap();
                malformed_response(&self.bus, &opening);
                self.observed.lock().unwrap().signals += 1;
            }
            Action::Reply { replacement, delay } => {
                assert!(self.delayed_reply.is_none());
                self.delayed_reply = Some((Instant::now() + delay, replacement));
            }
            Action::Response {
                uris,
                code,
                foreign,
                wrong_path,
            } => {
                let opening = self.observed.lock().unwrap().opening.clone().unwrap();
                if foreign && self.foreign_bus.is_none() {
                    self.foreign_bus = Some(Bus::open(self.address).unwrap());
                }
                let sender = if foreign {
                    self.foreign_bus.as_ref().unwrap()
                } else {
                    &self.bus
                };
                respond(sender, &opening, &uris, code, wrong_path);
                self.observed.lock().unwrap().signals += 1;
            }
        }
    }

    fn reply_when_due(&mut self) {
        let Some((at, replacement)) = self.delayed_reply else {
            return;
        };
        if Instant::now() < at {
            return;
        }
        let call = self
            .opening_call
            .take()
            .expect("one original OpenFile call");
        let reply = reply_message(&call);
        let opening = self.observed.lock().unwrap().opening.clone().unwrap();
        let path = if replacement {
            c"/org/freedesktop/portal/desktop/request/actual/replacement"
        } else {
            &opening.path
        };
        object_path(&reply, path);
        self.bus.send(&reply).unwrap();
        self.observed.lock().unwrap().opening_replies += 1;
        self.delayed_reply = None;
    }

    fn message(&mut self, message: Message) {
        if is_method(&message, c"org.freedesktop.DBus.Properties", c"Get") {
            message.signature(c"ss").unwrap();
            assert_eq!(
                message.read_text(b's', 128).unwrap(),
                "org.freedesktop.portal.FileChooser"
            );
            assert_eq!(message.read_text(b's', 128).unwrap(), "version");
            let reply = reply_message(&message);
            reply.open(b'v', c"u").unwrap();
            reply.number(self.version).unwrap();
            reply.close().unwrap();
            self.bus.send(&reply).unwrap();
            self.observed.lock().unwrap().version_queries += 1;
        } else if is_method(&message, c"org.freedesktop.portal.FileChooser", c"OpenFile") {
            assert!(self.opening_call.is_none());
            let opening = read_opening(&message);
            assert!(
                self.observed
                    .lock()
                    .unwrap()
                    .opening
                    .replace(opening)
                    .is_none()
            );
            self.opening_call = Some(message);
        } else if is_method(&message, c"org.freedesktop.portal.Request", c"Close") {
            message.signature(c"").unwrap();
            let mut state = self.observed.lock().unwrap();
            state
                .close_paths
                .push(header(&message, false).to_str().unwrap().to_owned());
            state.close_before_open_reply |= self.opening_call.is_some();
            drop(state);
            if self.acknowledge_close {
                self.bus.send(&reply_message(&message)).unwrap();
            }
        }
    }

    fn turn(&mut self) {
        for _ in 0..32 {
            let (progress, message) = self.bus.process().unwrap();
            if let Some(message) = message {
                self.message(message);
            }
            if !progress {
                break;
            }
        }
        if let Some(bus) = self.foreign_bus.as_ref() {
            for _ in 0..8 {
                if !bus.process().unwrap().0 {
                    break;
                }
            }
        }
        self.bus.wait(Duration::from_millis(1)).unwrap();
    }
}

fn serve(
    address: &CStr,
    version: u32,
    acknowledge_close: bool,
    actions: &Receiver<Action>,
    observed: &Mutex<Observed>,
    stop: &AtomicBool,
    ready: &SyncSender<()>,
) {
    let bus = Bus::open(address).unwrap();
    // SAFETY: this newly created fixture bus is thread confined; timeout is
    // bounded before the only synchronous fixture-name request.
    assert!(unsafe { sd_bus_set_method_call_timeout(bus.raw(), 2_000_000) } >= 0);
    // SAFETY: live thread-owned bus and static name; no Rust callback retained.
    let claimed =
        unsafe { sd_bus_request_name(bus.raw(), c"org.freedesktop.portal.Desktop".as_ptr(), 0) };
    assert!(claimed > 0);
    ready.send(()).unwrap();
    let mut service = Service {
        bus,
        address,
        version,
        acknowledge_close,
        observed,
        opening_call: None,
        delayed_reply: None,
        foreign_bus: None,
    };
    let deadline = Instant::now() + Duration::from_secs(20);
    while !stop.load(Ordering::Acquire) && Instant::now() < deadline {
        for _ in 0..16 {
            let Ok(action) = actions.try_recv() else {
                break;
            };
            service.action(action);
        }
        service.reply_when_due();
        service.turn();
    }
}

fn pending_until(picker: &mut DirectoryPicker, ready: impl Fn(&DirectoryPicker) -> bool) {
    let deadline = Instant::now() + TEST_TIMEOUT;
    while !ready(picker) {
        assert!(
            Instant::now() < deadline,
            "original picker did not progress"
        );
        let state = match picker.phase {
            Phase::Connecting => "connecting",
            Phase::Activating(_) => "activating",
            Phase::Owner(_) => "owner",
            Phase::Version(_) => "version",
            Phase::Subscribing(_) => "subscribing",
            Phase::Opening(_) => "opening",
            Phase::Waiting => "waiting",
            Phase::Closed => "closed",
            Phase::PortalUid(_) => "owner credentials",
        };
        assert_eq!(
            picker.poll(),
            Ok(None),
            "phase before poll: {state}; messages: {}",
            picker.messages
        );
        thread::sleep(Duration::from_millis(1));
    }
}
fn result(picker: &mut DirectoryPicker) -> Result<PathBuf, Error> {
    let deadline = Instant::now() + TEST_TIMEOUT;
    loop {
        assert!(
            Instant::now() < deadline,
            "original picker did not complete"
        );
        match picker.poll() {
            Ok(None) => thread::sleep(Duration::from_millis(1)),
            Ok(Some(path)) => return Ok(path),
            Err(error) => return Err(error),
        }
    }
}

#[test]
fn real_portal_roundtrip_requests_one_directory_and_releases_only_completed_choice() {
    let portal = Portal::start(3, true);
    let mut picker = portal.picker();
    portal.wait_open(&mut picker);
    assert_eq!(portal.state().version_queries, 1);
    let opening = portal.state().opening.unwrap();
    assert!(opening.directory);
    assert!(!opening.multiple);
    portal.reply(false);
    pending_until(&mut picker, |p| p.phase == Phase::Waiting);
    portal.chosen(vec!["file:///tmp/received%20files"]);
    assert_eq!(
        result(&mut picker),
        Ok(PathBuf::from("/tmp/received files"))
    );
    assert!(picker.bus.is_none());
    assert_eq!(picker.poll(), Err(Error::AlreadyUsed));
    assert_eq!(picker.close(), Ok(()));
    assert_eq!(portal.state().close_paths, Vec::<String>::new());
}

#[test]
fn early_response_waits_for_the_exact_original_opening_reply() {
    let portal = Portal::start(3, true);
    let mut picker = portal.picker();
    portal.wait_open(&mut picker);
    portal.chosen(vec!["file:///tmp/received"]);
    pending_until(&mut picker, |p| p.response_received);
    assert!(matches!(picker.phase, Phase::Opening(_)));
    assert_eq!(picker.poll(), Ok(None));
    assert_eq!(portal.state().opening_replies, 0);
    portal.reply(false);
    assert_eq!(result(&mut picker), Ok(PathBuf::from("/tmp/received")));
}

#[test]
fn early_response_with_replaced_handle_refuses_and_closes_the_returned_owner() {
    let portal = Portal::start(3, true);
    let mut picker = portal.picker();
    portal.wait_open(&mut picker);
    portal.chosen(vec!["file:///tmp/should-not-escape"]);
    pending_until(&mut picker, |p| p.response_received);
    portal.reply(true);
    assert_eq!(result(&mut picker), Err(Error::InvalidSelection));
    assert_eq!(picker.close(), Ok(()));
    assert_eq!(
        portal.state().close_paths,
        ["/org/freedesktop/portal/desktop/request/actual/replacement"]
    );
    assert!(!portal.state().close_before_open_reply);
}

#[test]
fn cancelling_pending_open_waits_then_acknowledges_close_of_that_exact_request() {
    let portal = Portal::start(3, true);
    let mut picker = portal.picker();
    portal.wait_open(&mut picker);
    portal
        .actions
        .send(Action::Reply {
            replacement: false,
            delay: Duration::from_millis(40),
        })
        .unwrap();
    assert_eq!(picker.close(), Ok(()));
    let state = portal.state();
    assert_eq!(state.opening_replies, 1);
    assert_eq!(
        state.close_paths,
        [state.opening.unwrap().path.to_str().unwrap()]
    );
    assert!(!state.close_before_open_reply);
    assert_eq!(picker.poll(), Err(Error::Cancelled));
}

#[test]
fn cancellation_after_early_response_still_settles_the_opening_reply() {
    let portal = Portal::start(3, true);
    let mut picker = portal.picker();
    portal.wait_open(&mut picker);
    portal.chosen(vec!["file:///tmp/should-not-escape"]);
    pending_until(&mut picker, |p| p.response_received);
    portal
        .actions
        .send(Action::Reply {
            replacement: false,
            delay: Duration::from_millis(40),
        })
        .unwrap();
    assert_eq!(picker.close(), Ok(()));
    assert_eq!(portal.state().opening_replies, 1);
    assert_eq!(portal.state().close_paths, Vec::<String>::new());
    assert_eq!(picker.poll(), Err(Error::Cancelled));
}

#[test]
fn unicast_responses_from_another_owner_or_request_cannot_complete_the_choice() {
    let portal = Portal::start(3, true);
    let mut picker = portal.picker();
    portal.wait_open(&mut picker);
    portal.reply(false);
    pending_until(&mut picker, |p| p.phase == Phase::Waiting);
    let original_messages = picker.messages;
    for (foreign, wrong_path) in [(true, false), (false, true)] {
        portal
            .actions
            .send(Action::Response {
                uris: vec!["file:///tmp/foreign".to_owned()],
                code: 0,
                foreign,
                wrong_path,
            })
            .unwrap();
    }
    pending_until(&mut picker, |p| p.messages >= original_messages + 2);
    assert!(!picker.response_received);
    assert_eq!(picker.poll(), Ok(None));
    portal.chosen(vec!["file:///tmp/original"]);
    assert_eq!(result(&mut picker), Ok(PathBuf::from("/tmp/original")));
}

#[test]
fn old_file_chooser_version_refuses_before_any_native_interaction_opens() {
    let portal = Portal::start(2, true);
    let mut picker = portal.picker();
    assert_eq!(result(&mut picker), Err(Error::Unavailable));
    assert_eq!(portal.state().version_queries, 1);
    assert!(portal.state().opening.is_none());
    assert_eq!(picker.close(), Ok(()));
    assert_eq!(portal.state().close_paths, Vec::<String>::new());
}

#[test]
fn missing_portal_gets_one_bounded_activation_attempt_and_no_native_choice() {
    let (_broker, address) = Broker::start();
    let mut picker = DirectoryPicker::open_at(&address, bus::now().unwrap()).unwrap();
    assert_eq!(result(&mut picker), Err(Error::Unavailable));
    assert!(picker.activation_attempted);
    assert!(!picker.request_sent);
    assert!(picker.owner.is_none());
    assert_eq!(picker.close(), Ok(()));
}

#[test]
fn real_response_payload_rejects_remote_multiple_and_ambiguous_directories() {
    for uris in [
        vec!["file://unrelated-host/tmp/received"],
        vec!["file:///tmp/first", "file:///tmp/second"],
        vec!["file:///tmp/first\nfile:///tmp/second"],
    ] {
        let portal = Portal::start(3, true);
        let mut picker = portal.picker();
        portal.wait_open(&mut picker);
        portal.reply(false);
        portal.chosen(uris);
        assert_eq!(result(&mut picker), Err(Error::InvalidSelection));
        assert_eq!(picker.close(), Ok(()));
        assert_eq!(portal.state().close_paths, Vec::<String>::new());
    }
}

#[test]
fn native_user_cancellation_never_releases_a_path() {
    let portal = Portal::start(3, true);
    let mut picker = portal.picker();
    portal.wait_open(&mut picker);
    portal.reply(false);
    portal
        .actions
        .send(Action::Response {
            uris: Vec::new(),
            code: 1,
            foreign: false,
            wrong_path: false,
        })
        .unwrap();
    assert_eq!(result(&mut picker), Err(Error::Cancelled));
    assert!(picker.bus.is_none());
    assert_eq!(picker.close(), Ok(()));
}

#[test]
fn expired_choice_retains_cleanup_failure_when_the_original_close_is_unacknowledged() {
    let portal = Portal::start(3, false);
    let mut picker = portal.picker();
    portal.wait_open(&mut picker);
    portal.reply(false);
    pending_until(&mut picker, |p| p.phase == Phase::Waiting);
    // Advance the owner's existing absolute deadline, not the native clock;
    // expiry is still evaluated by the production poll and cleanup paths.
    picker.until = bus::now().unwrap();
    assert_eq!(picker.poll(), Err(Error::Expired));
    assert_eq!(picker.close(), Err(Error::CleanupIncomplete));
    assert_eq!(picker.close(), Err(Error::CleanupIncomplete));
    assert_eq!(picker.poll(), Err(Error::CleanupIncomplete));
    assert_eq!(portal.state().close_paths.len(), 1);
    assert!(picker.bus.is_none());
}

#[test]
fn cancelling_pending_open_with_mismatched_handle_still_closes_the_returned_owner() {
    let portal = Portal::start(3, true);
    let mut picker = portal.picker();
    portal.wait_open(&mut picker);
    portal
        .actions
        .send(Action::Reply {
            replacement: true,
            delay: Duration::from_millis(40),
        })
        .unwrap();
    assert_eq!(picker.close(), Ok(()));
    let state = portal.state();
    assert_eq!(state.opening_replies, 1);
    assert_eq!(
        state.close_paths,
        ["/org/freedesktop/portal/desktop/request/actual/replacement"]
    );
    assert!(!state.close_before_open_reply);
    assert_eq!(picker.poll(), Err(Error::InvalidSelection));
}

#[test]
fn malformed_terminal_signatures_and_codes_do_not_acknowledge_native_cleanup() {
    for action in [
        Action::MalformedResponse,
        Action::Response {
            uris: Vec::new(),
            code: 99,
            foreign: false,
            wrong_path: false,
        },
    ] {
        let portal = Portal::start(3, true);
        let mut picker = portal.picker();
        portal.wait_open(&mut picker);
        portal.reply(false);
        pending_until(&mut picker, |p| p.phase == Phase::Waiting);
        portal.actions.send(action).unwrap();
        assert_eq!(result(&mut picker), Err(Error::InvalidSelection));
        assert!(!picker.response_received);
        assert_eq!(picker.close(), Ok(()));
        let state = portal.state();
        assert_eq!(
            state.close_paths,
            [state.opening.unwrap().path.to_str().unwrap()]
        );
    }
}

#[path = "native/files.rs"]
mod files;
