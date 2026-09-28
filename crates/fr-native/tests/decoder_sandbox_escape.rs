#![cfg(all(target_os = "linux", feature = "linux-media"))]
//! The decoder/presenter confinement is proven by probes that SUCCEED without
//! it: the parent opens a TCP listener, a Unix listener, a readable file and a
//! writable directory, and the child first runs every probe unconfined (the
//! control run must succeed), then enters `confine_decoder_process` and runs the
//! same probes against the same targets, which must fail with `EPERM` (errno 1)
//! from the seccomp filter, not with "connection refused" or "not found".

use fr_core::limits::ProtocolLimits;
use fr_native::X11Surface;
use std::{
    io::{BufRead, BufReader},
    os::fd::AsFd,
    path::PathBuf,
    process::{Child, Command, Stdio},
};

const EPERM: i32 = 1;

struct Display {
    child: Child,
    name: String,
}

impl Display {
    fn start() -> Self {
        let mut child = Command::new("Xvfb")
            .args([
                "-displayfd",
                "1",
                "-screen",
                "0",
                "320x240x24",
                "-nolisten",
                "tcp",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("Xvfb is a native test prerequisite");
        let mut name = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut name)
            .unwrap();
        let number = name.trim().parse::<u32>().expect("Xvfb displayfd response");
        Self {
            child,
            name: format!(":{number}"),
        }
    }
}

impl Drop for Display {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Targets the parent created; every probe succeeds against them unconfined.
struct Targets {
    tcp: String,
    unix: PathBuf,
    readable: PathBuf,
    writable_dir: PathBuf,
}
impl Targets {
    fn from_env() -> Self {
        let var = |name: &str| std::env::var(name).expect("target from the parent");
        Self {
            tcp: var("FR_ESCAPE_TCP"),
            unix: var("FR_ESCAPE_UNIX").into(),
            readable: var("FR_ESCAPE_READABLE").into(),
            writable_dir: var("FR_ESCAPE_DIR").into(),
        }
    }
}

/// `ok` or the raw errno of each probe, one line per probe.
fn probe(phase: &str, targets: &Targets) {
    fn line<T>(phase: &str, name: &str, result: std::io::Result<T>) {
        match result {
            Ok(_) => println!("ESCAPE:{phase}:{name}=ok"),
            Err(error) => println!(
                "ESCAPE:{phase}:{name}=errno{}",
                error.raw_os_error().unwrap_or(-1)
            ),
        }
    }
    line(phase, "tcp", std::net::TcpStream::connect(&targets.tcp));
    line(phase, "udp", std::net::UdpSocket::bind("127.0.0.1:0"));
    line(phase, "read", std::fs::File::open(&targets.readable));
    line(
        phase,
        "write",
        std::fs::File::create(targets.writable_dir.join(format!("{phase}.probe"))),
    );
    line(
        phase,
        "ipc",
        std::os::unix::net::UnixStream::connect(&targets.unix),
    );
    line(
        phase,
        "spawn",
        Command::new("/bin/true")
            .spawn()
            .map(|mut child| child.wait()),
    );
}

fn run_child_escape_attempts() {
    let targets = Targets::from_env();
    let display_name = std::env::var("DISPLAY").expect("DISPLAY required in child");
    let mut surface =
        X11Surface::presenter(Some(&display_name), 320, 240, ProtocolLimits::ABSOLUTE)
            .expect("open X11 presenter surface");
    let (pipe_r, _pipe_w) = std::os::unix::net::UnixStream::pair().expect("create pipe pair");
    // Control run: the same probes, unconfined, against the same targets.
    probe("control", &targets);
    let stdout = std::io::stdout();
    let stderr = std::io::stderr();
    surface
        .confine_decoder_process(pipe_r.as_fd(), stdout.as_fd(), stderr.as_fd())
        .expect("confine_decoder_process must succeed");
    probe("confined", &targets);
    println!("ESCAPE:all_completed=true");
    std::process::exit(0);
}

#[test]
fn decoder_sandbox_escape_attempts_are_strictly_enforced_by_kernel() {
    if std::env::var("FR_TEST_DECODER_SANDBOX_CHILD").as_deref() == Ok("1") {
        run_child_escape_attempts();
        return;
    }

    let display = Display::start();
    let dir = std::env::temp_dir().join(format!("fr-escape-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let readable = dir.join("readable");
    std::fs::write(&readable, b"host data").unwrap();
    let unix = dir.join("approval.sock");
    let unix_listener = std::os::unix::net::UnixListener::bind(&unix).unwrap();
    let tcp_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let self_image = std::env::current_exe().expect("current_exe");

    // ubs:ignore[rust.security.command-executable] Test harness re-executes itself in child probe mode
    let output = Command::new(&self_image)
        .arg("decoder_sandbox_escape_attempts_are_strictly_enforced_by_kernel")
        .arg("--exact")
        .arg("--nocapture")
        .env("FR_TEST_DECODER_SANDBOX_CHILD", "1")
        .env("DISPLAY", &display.name)
        .env(
            "FR_ESCAPE_TCP",
            tcp_listener.local_addr().unwrap().to_string(),
        )
        .env("FR_ESCAPE_UNIX", &unix)
        .env("FR_ESCAPE_READABLE", &readable)
        .env("FR_ESCAPE_DIR", &dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("spawn confined child probe");
    drop((unix_listener, tcp_listener));
    let _ = std::fs::remove_dir_all(&dir);

    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    println!("Child stderr:\n{stderr}");
    println!("Child stdout:\n{stdout}");
    assert!(
        output.status.success(),
        "Child process failed: exit status {:?}",
        output.status
    );
    assert!(
        stdout.contains("ESCAPE:all_completed=true"),
        "Child process did not complete all probes"
    );
    for name in ["tcp", "udp", "read", "write", "ipc", "spawn"] {
        // Without confinement each probe works, so a later failure is the sandbox.
        assert!(
            stdout.contains(&format!("ESCAPE:control:{name}=ok")),
            "control probe {name} must succeed unconfined: {stdout}"
        );
        assert!(
            stdout.contains(&format!("ESCAPE:confined:{name}=errno{EPERM}")),
            "confined probe {name} must fail with EPERM from seccomp: {stdout}"
        );
    }
}
