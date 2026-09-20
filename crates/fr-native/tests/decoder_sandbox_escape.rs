#![cfg(all(target_os = "linux", feature = "linux-media"))]

use fr_core::limits::ProtocolLimits;
use fr_native::X11Surface;
use std::{
    io::{BufRead, BufReader},
    os::fd::AsFd,
    process::{Child, Command, Stdio},
};

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

fn run_child_escape_attempts() {
    let display_name = std::env::var("DISPLAY").expect("DISPLAY required in child");
    let mut surface =
        X11Surface::presenter(Some(&display_name), 320, 240, ProtocolLimits::ABSOLUTE)
            .expect("open X11 presenter surface");

    // Dummy pipe for input
    let (pipe_r, _pipe_w) = std::os::unix::net::UnixStream::pair().expect("create pipe pair");

    let stdout = std::io::stdout();
    let stderr = std::io::stderr();

    // Enter seccomp BPF confinement
    surface
        .confine_decoder_process(pipe_r.as_fd(), stdout.as_fd(), stderr.as_fd())
        .expect("confine_decoder_process must succeed");

    eprintln!("Child: successfully entered seccomp sandbox confinement");

    // 1. Escape attempt: TCP connect
    let tcp_res = std::net::TcpStream::connect("127.0.0.1:80");
    println!("ESCAPE:tcp_denied={}", tcp_res.is_err());
    println!("ESCAPE:tcp_error={:?}", tcp_res.err().map(|e| e.kind()));

    // 2. Escape attempt: UDP bind
    let udp_res = std::net::UdpSocket::bind("127.0.0.1:0");
    println!("ESCAPE:udp_denied={}", udp_res.is_err());
    println!("ESCAPE:udp_error={:?}", udp_res.err().map(|e| e.kind()));

    // 3. Escape attempt: read sensitive host file outside allowed paths
    let fs_read_res = std::fs::File::open("/etc/passwd");
    println!("ESCAPE:fs_read_denied={}", fs_read_res.is_err());
    println!(
        "ESCAPE:fs_read_error={:?}",
        fs_read_res.err().map(|e| e.kind())
    );

    // 4. Escape attempt: write outside allowed paths
    let fs_write_res = std::fs::File::create("/tmp/fr_sandbox_escape_probe.txt");
    println!("ESCAPE:fs_write_denied={}", fs_write_res.is_err());
    println!(
        "ESCAPE:fs_write_error={:?}",
        fs_write_res.err().map(|e| e.kind())
    );

    // 5. Escape attempt: connect to approval IPC socket
    let ipc_res = std::os::unix::net::UnixStream::connect("/tmp/frd-approval.sock");
    println!("ESCAPE:ipc_denied={}", ipc_res.is_err());
    println!("ESCAPE:ipc_error={:?}", ipc_res.err().map(|e| e.kind()));

    // 6. Escape attempt: spawn arbitrary process
    let proc_res = Command::new("/bin/sh").spawn();
    println!("ESCAPE:proc_denied={}", proc_res.is_err());
    println!("ESCAPE:proc_error={:?}", proc_res.err().map(|e| e.kind()));

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
    let self_image = std::env::current_exe().expect("current_exe");

    // ubs:ignore[rust.security.command-executable] Test harness re-executes itself in child probe mode
    let output = Command::new(&self_image)
        .arg("decoder_sandbox_escape_attempts_are_strictly_enforced_by_kernel")
        .arg("--exact")
        .arg("--nocapture")
        .env("FR_TEST_DECODER_SANDBOX_CHILD", "1")
        .env("DISPLAY", &display.name)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("spawn confined child probe");

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

    // Assert every single escape attempt was denied by kernel seccomp
    assert!(
        stdout.contains("ESCAPE:tcp_denied=true"),
        "TCP connect must be denied by sandbox"
    );
    assert!(
        stdout.contains("ESCAPE:udp_denied=true"),
        "UDP bind must be denied by sandbox"
    );
    assert!(
        stdout.contains("ESCAPE:fs_read_denied=true"),
        "Filesystem open/read must be denied by sandbox"
    );
    assert!(
        stdout.contains("ESCAPE:fs_write_denied=true"),
        "Filesystem create/write must be denied by sandbox"
    );
    assert!(
        stdout.contains("ESCAPE:ipc_denied=true"),
        "Approval IPC connect must be denied by sandbox"
    );
    assert!(
        stdout.contains("ESCAPE:proc_denied=true"),
        "Process execution must be denied by sandbox"
    );

    println!("All 6 sandbox escape attempts successfully refused with typed OS error!");
}
