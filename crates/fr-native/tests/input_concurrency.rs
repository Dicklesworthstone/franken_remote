#![cfg(all(target_os = "linux", feature = "linux-input"))]
use fr_core::{
    input::DesktopPoint,
    input_submission::{InputSink, Operation, Submission},
};
use fr_native::input::X11Pointer;
use std::{
    io::{BufRead, BufReader},
    process::{Child, Command, Stdio},
    sync::Barrier,
};
struct Server(Child);
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
#[test]
fn concurrent_extension_connections_and_teardown_preserve_native_memory() {
    let mut server = Server(
        Command::new("Xvfb")
            .args([
                "-displayfd",
                "1",
                "-screen",
                "0",
                "320x240x24",
                "-nolisten",
                "tcp",
                "-noreset",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap(),
    );
    let mut number = String::new();
    BufReader::new(server.0.stdout.take().unwrap())
        .read_line(&mut number)
        .unwrap();
    let name = format!(":{}", number.trim().parse::<u16>().unwrap());
    // Keep the private server from resetting between connections. Each worker
    // owns its own display; this deliberately runs concurrently, not serially.
    let _anchor = X11Pointer::open(&name).unwrap();
    let barrier = Barrier::new(8);
    std::thread::scope(|scope| {
        for _ in 0..8 {
            scope.spawn(|| {
                barrier.wait();
                for _ in 0..64 {
                    let mut sink = X11Pointer::open(&name).unwrap();
                    let op = Operation::Absolute(DesktopPoint { x: 10, y: 15 });
                    // Direct native-boundary stress on a private test server.
                    // Authority and wire integration are exercised separately.
                    sink.prepare(op).unwrap();
                    assert_eq!(sink.submit(op), Submission::Submitted);
                    sink.cancel_prepared();
                    assert!(sink.cleanup_native());
                }
            });
        }
    });
}
