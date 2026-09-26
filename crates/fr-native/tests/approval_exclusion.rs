//! Real Xvfb, the production approval window and production `XTest` input sink.
//! No peer, physical-user or live-tailnet identity is claimed by this fixture.
#![cfg(all(target_os = "linux", feature = "linux-input"))]
use fr_core::{
    input::{DesktopPoint, KeyTransition, PhysicalKey, PointerButton},
    input_submission::{InputSink, Operation, PlatformError, Submission, scroll::WheelDirection},
};
use fr_native::input::X11Pointer;
use std::{
    ffi::{CString, c_char, c_void},
    io::{BufRead, BufReader, Read},
    process::{Child, Command, Stdio},
    ptr::NonNull,
};

unsafe extern "C" {
    fn fr_approval_open(display: *const c_char, role: u32, window: *mut u32) -> *mut c_void;
    fn fr_indicator_close(handle: *mut c_void);
}
struct Server {
    process: Child,
    display: String,
}
impl Server {
    fn start() -> Self {
        let mut process = Command::new("Xvfb")
            .args([
                "-displayfd",
                "1",
                "-screen",
                "0",
                "640x480x24",
                "-nolisten",
                "tcp",
                "-noreset",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("Xvfb is required for native approval exclusion");
        let mut number = String::new();
        BufReader::new(process.stdout.take().unwrap().take(16))
            .read_line(&mut number)
            .unwrap();
        Self {
            process,
            display: format!(":{}", number.trim().parse::<u16>().unwrap()),
        }
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.process.kill();
        let _ = self.process.wait();
    }
}
struct Prompt(NonNull<c_void>);
impl Prompt {
    fn open(display: &str, role: u32) -> Option<Self> {
        let name = CString::new(display).unwrap();
        let mut window = 0;
        // SAFETY: live NUL string and scalar output; the owned C handle retains neither.
        let prompt =
            NonNull::new(unsafe { fr_approval_open(name.as_ptr(), role, &raw mut window) });
        if prompt.is_some() {
            assert_ne!(window, 0);
        } else {
            assert_eq!(window, 0, "refusal must precede native mapping");
        }
        prompt.map(Self)
    }
}
impl Drop for Prompt {
    fn drop(&mut self) {
        // SAFETY: exact uniquely owned allocation, released once on this thread.
        unsafe {
            fr_indicator_close(self.0.as_ptr());
        }
    }
}
fn move_to(x: i32, y: i32) -> Operation {
    Operation::Absolute(DesktopPoint { x, y })
}
fn submit(sink: &mut X11Pointer, operation: Operation) {
    sink.prepare(operation).unwrap();
    assert_eq!(sink.submit(operation), Submission::Submitted);
}

#[test]
fn pending_approval_prevents_input_not_just_its_allow_decision() {
    let server = Server::start();
    for role in [0, 1] {
        let mut input = X11Pointer::open(&server.display).unwrap();
        let mut observer = X11Pointer::open(&server.display).unwrap();
        submit(&mut input, move_to(550, 350));
        let before = observer.query_pointer().unwrap();
        let prompt = Prompt::open(&server.display, role).unwrap();
        for operation in [
            move_to(320, 95),
            Operation::Relative { x: 1, y: 0 },
            Operation::Button {
                button: PointerButton::Primary,
                pressed: true,
            },
            Operation::Key {
                key: PhysicalKey::new(4).unwrap(),
                transition: KeyTransition::Press,
            },
            Operation::Wheel {
                direction: WheelDirection::Down,
                pressed: true,
            },
        ] {
            let prepared = input.prepare(operation);
            input.cancel_prepared();
            assert_eq!(prepared, Err(PlatformError::Permission));
        }
        assert_eq!(observer.query_pointer().unwrap(), before);
        drop(prompt);
        assert_eq!(
            input.prepare(move_to(40, 40)),
            Err(PlatformError::Permission),
            "a fenced native owner must never resume after approval closes"
        );
        let mut replacement = X11Pointer::open(&server.display).unwrap();
        submit(&mut replacement, move_to(40, 40));
        assert_eq!(
            observer.query_pointer().unwrap().0,
            DesktopPoint { x: 40, y: 40 }
        );
    }
}

#[test]
fn prepared_input_excludes_mapping_until_submit_or_cancellation() {
    let server = Server::start();
    let mut input = X11Pointer::open(&server.display).unwrap();
    for cancel in [false, true] {
        let operation = move_to(510, 310);
        input.prepare(operation).unwrap();
        let refused = Prompt::open(&server.display, 0);
        // Release preparation before any assertion can unwind another native owner.
        if cancel {
            input.cancel_prepared();
        } else {
            assert_eq!(input.submit(operation), Submission::Submitted);
        }
        assert!(
            refused.is_none(),
            "a prompt mapped over an admitted native operation"
        );
        let prompt = Prompt::open(&server.display, 1).unwrap();
        assert!(
            Prompt::open(&server.display, 0).is_none(),
            "only one positive-consent owner"
        );
        drop(prompt);
    }
    let result = input.prepare(move_to(-1, 20));
    input.cancel_prepared();
    assert_eq!(result, Err(PlatformError::GeometryChanged));
    assert!(
        Prompt::open(&server.display, 0).is_some(),
        "failed preparation released its permit"
    );
}

#[test]
fn held_release_and_native_cleanup_remain_possible_during_approval() {
    let server = Server::start();
    let mut input = X11Pointer::open(&server.display).unwrap();
    let mut observer = X11Pointer::open(&server.display).unwrap();
    submit(&mut input, move_to(550, 350));
    submit(
        &mut input,
        Operation::Button {
            button: PointerButton::Primary,
            pressed: true,
        },
    );
    let key = PhysicalKey::new(225).unwrap(); // Shift; release must restore native repeat too.
    submit(
        &mut input,
        Operation::Key {
            key,
            transition: KeyTransition::Press,
        },
    );
    assert_ne!(observer.query_pointer().unwrap().1 & 0x100, 0);
    let prompt = Prompt::open(&server.display, 0).unwrap();
    let result = input.prepare(move_to(352, 97));
    input.cancel_prepared();
    assert_eq!(result, Err(PlatformError::Permission));
    assert!(input.locally_revoked());
    submit(
        &mut input,
        Operation::Button {
            button: PointerButton::Primary,
            pressed: false,
        },
    );
    submit(
        &mut input,
        Operation::Key {
            key,
            transition: KeyTransition::Release,
        },
    );
    assert!(input.cleanup_native());
    assert_eq!(observer.query_pointer().unwrap().1 & 0x100, 0);
    drop(prompt);
    assert!(
        input.locally_revoked(),
        "cleanup and prompt closure cannot restore authority"
    );
}

#[test]
fn mismatched_submission_releases_exclusion_without_dispatch() {
    let server = Server::start();
    let mut input = X11Pointer::open(&server.display).unwrap();
    submit(&mut input, move_to(500, 300));
    let before = input.query_pointer().unwrap();
    input.prepare(move_to(40, 40)).unwrap();
    let result = input.submit(move_to(80, 80));
    input.cancel_prepared();
    assert_eq!(result, Submission::NotSubmitted(PlatformError::Unsupported));
    assert_eq!(input.query_pointer().unwrap(), before);
    assert!(Prompt::open(&server.display, 0).is_some());
}

#[test]
fn cross_process_prompt_and_display_aliases_share_one_exclusion_owner() {
    use std::io::Write;
    const CHILD: &str = "FR_CONSENT_EXCLUSION_CHILD_DISPLAY";
    if let Ok(display) = std::env::var(CHILD) {
        let prompt = Prompt::open(&display, 0).unwrap();
        println!("PROMPT_READY");
        std::io::stdout().flush().unwrap();
        let mut command = String::new();
        std::io::stdin().read_line(&mut command).unwrap();
        drop(prompt);
        return;
    }
    let server = Server::start();
    let mut input = X11Pointer::open(&server.display).unwrap();
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "cross_process_prompt_and_display_aliases_share_one_exclusion_owner",
            "--nocapture",
        ])
        .env(
            CHILD,
            format!(":0{}.0", server.display.trim_start_matches(':')),
        )
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let mut output = BufReader::new(child.stdout.take().unwrap());
    loop {
        let mut line = String::new();
        assert_ne!(
            output.read_line(&mut line).unwrap(),
            0,
            "prompt child exited before mapping"
        );
        if line.trim() == "PROMPT_READY" {
            break;
        }
    }
    let refused = input.prepare(move_to(352, 97));
    input.cancel_prepared();
    // Kill the actual gate owner; no file unlink or stale-lock cleanup is used.
    child.kill().unwrap();
    child.wait().unwrap();
    assert_eq!(refused, Err(PlatformError::Permission));
    assert!(input.locally_revoked());
    assert_eq!(
        input.prepare(move_to(50, 50)),
        Err(PlatformError::Permission)
    );
    // This is a fresh native owner only, not an automatic session/grant retry.
    let mut replacement = X11Pointer::open(&server.display).unwrap();
    submit(&mut replacement, move_to(50, 50));
}

#[test]
fn malformed_local_display_cannot_create_an_alternate_gate() {
    for display in [
        "",
        ":",
        "remote:0",
        ":999999",
        ":1.",
        ":1.2.3",
        ":1/2",
        ":1.999999",
    ] {
        assert!(
            matches!(X11Pointer::open(display), Err(PlatformError::Unsupported)),
            "{display}"
        );
        assert!(Prompt::open(display, 0).is_none());
    }
}
