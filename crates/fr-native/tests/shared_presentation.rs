#![cfg(all(target_os = "linux", feature = "linux-media"))]
//! Real image submission, independent pixel readback and a confined subprocess.
#[allow(dead_code)]
#[path = "presentation_support/mod.rs"]
mod support;
use fr_core::limits::ProtocolLimits;
use fr_native::{NativeError, X11Surface, image_transfer::Path};
use support::{Server, picture};

#[test]
fn shared_presenter_reuses_one_image_and_idle_repaint_does_not_copy_again() {
    let server = Server::start();
    let mut window = server.window();
    let target = window.presentation_target().unwrap();
    let mut renderer =
        X11Surface::present_in(Some(&server.name), target, ProtocolLimits::ABSOLUTE).unwrap();
    assert_eq!(
        renderer.transfer_statistics().unwrap().path,
        Path::NotInitialized
    );
    for (index, value) in [31, 77, 173].into_iter().enumerate() {
        let frame = picture(value);
        renderer.present(&frame).unwrap();
        assert_eq!(window.snapshot().unwrap().pixels(), frame.pixels());
        let copied = u64::try_from(index + 1).unwrap() * 64 * 64 * 4;
        let before = renderer.transfer_statistics().unwrap();
        assert_eq!(before.path, Path::SharedMemory);
        assert_eq!(before.copied_bytes, copied);
        assert_eq!(before.socket_pixel_bytes, 0);
        assert_eq!(before.retained_bytes, 64 * 64 * 4);
        server.clear(target.window(), 2);
        assert_ne!(window.snapshot().unwrap().pixels(), frame.pixels());
        assert!(renderer.maintain_presentation().unwrap().repainted);
        assert_eq!(window.snapshot().unwrap().pixels(), frame.pixels());
        let after = renderer.transfer_statistics().unwrap();
        assert_eq!(after.images, before.images + 1);
        assert_eq!(after.copied_bytes, copied);
        assert_eq!(after.retained_bytes, before.retained_bytes);
    }
    server.unmap_roundtrip(target.window());
    assert_eq!(
        renderer.maintain_presentation(),
        Err(NativeError::GeometryChanged)
    );
    let retired = renderer.transfer_statistics().unwrap();
    assert_eq!(retired.retained_bytes, 0);
    assert_eq!(retired.images, 6);
    assert_eq!(
        renderer.present(&picture(201)),
        Err(NativeError::GeometryChanged)
    );
    assert_eq!(renderer.transfer_statistics().unwrap(), retired);
}

fn confined_child() {
    use std::io::{Read, Write};
    use std::os::fd::AsFd;
    let display = std::env::var("DISPLAY").unwrap();
    let window = std::env::var("FR_SHM_WINDOW").unwrap().parse().unwrap();
    let target = fr_media::worker::presentation::X11Target::new(window, 64, 64).unwrap();
    let mut renderer =
        X11Surface::present_in(Some(&display), target, ProtocolLimits::ABSOLUTE).unwrap();
    let (stdin, stdout, stderr) = (std::io::stdin(), std::io::stdout(), std::io::stderr());
    renderer
        .confine_decoder_process(stdin.as_fd(), stdout.as_fd(), stderr.as_fd())
        .unwrap();
    assert_eq!(
        renderer.transfer_statistics().unwrap().path,
        Path::SharedMemory
    );
    let initial = renderer.maintain_presentation().unwrap();
    assert!(!initial.repainted);
    assert_eq!(initial.retained_bytes, 64 * 64 * 4);
    assert_eq!(renderer.transfer_statistics().unwrap().images, 0);
    // New descriptors remain forbidden after the pre-confinement reservation.
    assert_eq!(
        std::fs::File::open("/etc/passwd")
            .err()
            .unwrap()
            .raw_os_error(),
        Some(1)
    );
    assert_eq!(
        std::os::unix::net::UnixStream::pair()
            .err()
            .unwrap()
            .raw_os_error(),
        Some(1)
    );
    println!("SHM:ready");
    stdout.lock().flush().unwrap();
    let mut input = stdin.lock();
    loop {
        let mut command = [0];
        input.read_exact(&mut command).unwrap();
        match command[0] {
            n @ 1..=3 => renderer.present(&picture(n * 37)).unwrap(),
            4 => {
                let before = renderer.transfer_statistics().unwrap();
                assert!(renderer.maintain_presentation().unwrap().repainted);
                let after = renderer.transfer_statistics().unwrap();
                assert_eq!(after.images, before.images + 1);
                assert_eq!(after.copied_bytes, before.copied_bytes);
            }
            5 => break,
            _ => panic!("invalid test command"),
        }
        assert_eq!(
            renderer.transfer_statistics().unwrap().socket_pixel_bytes,
            0
        );
        println!("SHM:painted");
        stdout.lock().flush().unwrap();
    }
    drop(renderer); // Checked detach, unmap and private drawable removal under seccomp.
    println!("SHM:stopped");
    stdout.lock().flush().unwrap();
}

#[test]
fn confined_decoder_can_present_and_repaint_without_new_syscall_permissions() {
    use std::{
        io::{BufRead, BufReader, Write},
        process::{Child, Command, Stdio},
        sync::mpsc,
        time::{Duration, Instant},
    };
    struct ChildOwner(Child);
    impl Drop for ChildOwner {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    if std::env::var("FR_SHM_WINDOW").is_ok() {
        confined_child();
        return;
    }
    let server = Server::start();
    let mut window = server.window();
    let target = window.presentation_target().unwrap();
    let mut child = ChildOwner(
        Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "confined_decoder_can_present_and_repaint_without_new_syscall_permissions",
                "--nocapture",
            ])
            .env("DISPLAY", &server.name)
            .env("FR_SHM_WINDOW", target.window().to_string())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap(),
    );
    let stdout = child.0.stdout.take().unwrap();
    let (send, receive) = mpsc::sync_channel(8);
    let reader = std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            if send.send(line.unwrap()).is_err() {
                break;
            }
        }
    });
    let wait = |marker: &str| {
        let until = Instant::now() + Duration::from_secs(5);
        loop {
            let line = receive
                .recv_timeout(until.saturating_duration_since(Instant::now()))
                .unwrap();
            if line.ends_with(marker) {
                break;
            }
        }
    };
    wait("SHM:ready");
    for value in 1..=3 {
        child.0.stdin.as_mut().unwrap().write_all(&[value]).unwrap();
        wait("SHM:painted");
        assert_eq!(
            window.snapshot().unwrap().pixels(),
            picture(value * 37).pixels()
        );
    }
    server.clear(target.window(), 1);
    assert_ne!(window.snapshot().unwrap().pixels(), picture(111).pixels());
    child.0.stdin.as_mut().unwrap().write_all(&[4]).unwrap();
    wait("SHM:painted");
    assert_eq!(window.snapshot().unwrap().pixels(), picture(111).pixels());
    child.0.stdin.as_mut().unwrap().write_all(&[5]).unwrap();
    wait("SHM:stopped");
    assert_eq!(window.snapshot().unwrap().pixels(), picture(0).pixels());
    let until = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            assert!(status.success());
            break;
        }
        assert!(Instant::now() < until);
        std::thread::sleep(Duration::from_millis(5));
    }
    drop(receive);
    reader.join().unwrap();
}
