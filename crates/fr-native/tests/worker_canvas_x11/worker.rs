//! Real HEVC and a separate production decoder process, not a fake completion.
use super::{Observer, Server, pattern};
use fr_core::{ids::CodecConfigurationGeneration, limits::ProtocolLimits};
use fr_media::{
    access_unit::{EncodedAccessUnit, FrameId},
    hevc::HevcGuard,
    worker::{
        self, Backend, Configuration, Error, Identity, Kind, Record, presentation::X11Target,
    },
};
use fr_native::{EncodeBackend, HevcEncoder, X11Surface};
use std::{
    os::unix::process::ExitStatusExt,
    process::{Child, ChildStdin, Command, Stdio},
    sync::mpsc::{Receiver, sync_channel},
    thread::JoinHandle,
    time::Duration,
};

struct Worker {
    child: Child,
    input: Option<ChildStdin>,
    output: Option<Receiver<Result<Record, Error>>>,
    reader: Option<JoinHandle<()>>,
    sequence: u64,
}
impl Worker {
    fn start(server: &Server) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_fr-media-worker"))
            .env_clear()
            .env("DISPLAY", &server.display)
            .arg("--present")
            .arg("--parent-pid")
            .arg(std::process::id().to_string())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let input = child.stdin.take();
        let mut stdout = child.stdout.take().unwrap();
        let (send, output) = sync_channel(1);
        // One bounded reader in the test fixture only. Production still uses
        // its original worker pipes; no runtime or watcher is added there.
        let reader = std::thread::spawn(move || {
            loop {
                let record = match Record::read(&mut stdout, &ProtocolLimits::ABSOLUTE) {
                    Ok(Some(record)) => Ok(record),
                    Ok(None) => break,
                    Err(error) => Err(error),
                };
                let terminal = record.is_err();
                if send.send(record).is_err() || terminal {
                    break;
                }
            }
        });
        Self {
            child,
            input,
            output: Some(output),
            reader: Some(reader),
            sequence: 0,
        }
    }
    fn transact(&mut self, kind: Kind, body: Vec<u8>) -> Record {
        let identity = Identity {
            epoch: 11,
            sequence: self.sequence,
        };
        self.sequence += 1;
        Record::new(kind, identity, body, &ProtocolLimits::ABSOLUTE)
            .unwrap()
            .write(self.input.as_mut().unwrap(), &ProtocolLimits::ABSOLUTE)
            .unwrap();
        let reply = self
            .output
            .as_ref()
            .unwrap()
            .recv_timeout(Duration::from_secs(3))
            .expect("native worker response deadline")
            .unwrap();
        assert_eq!(reply.header.identity, identity);
        reply
    }
    fn stop(&mut self) {
        assert_eq!(
            self.transact(Kind::Stop, Vec::new()).header.kind,
            Kind::Stopped
        );
        assert!(self.child.wait().unwrap().success());
    }
    fn crash(&mut self) {
        self.child.kill().unwrap();
        assert_eq!(self.child.wait().unwrap().signal(), Some(9));
    }
}
impl Drop for Worker {
    fn drop(&mut self) {
        drop(self.input.take());
        let _ = self.child.kill();
        let _ = self.child.wait();
        // Unblock a fixture reader even when a failed assertion left one reply
        // unconsumed. No background test thread survives this worker owner.
        drop(self.output.take());
        self.reader.take().unwrap().join().unwrap();
    }
}

struct Viewer {
    worker: Worker,
    owner: X11Surface,
    target: X11Target,
    encoder: HevcEncoder,
    configuration: Configuration,
    frame: u64,
}
impl Viewer {
    fn start(server: &Server, fitted: bool) -> Self {
        let configuration = Configuration {
            width: if fitted { 64 } else { 32 },
            height: 32,
            fps: 30,
            backend: Backend::SoftwareExplicit,
            bitrate: 1_000_000,
            max_access_unit_bytes: 1024 * 1024,
            generation: CodecConfigurationGeneration::INITIAL,
        };
        let limits = configuration.limits().unwrap();
        let mut owner = X11Surface::presenter(Some(&server.display), 32, 32, limits).unwrap();
        let target = owner.presentation_target().unwrap();
        let mut encoder = HevcEncoder::new(
            configuration.codec().unwrap(),
            limits,
            EncodeBackend::SoftwareExplicit,
            30,
            configuration.bitrate,
        )
        .unwrap();
        encoder
            .submit(
                &pattern(configuration.width, 32, 89),
                FrameId::FIRST,
                0,
                true,
            )
            .unwrap();
        let first = encoder.poll_output().unwrap();
        let mut guard = HevcGuard::new(configuration.codec().unwrap(), limits, 4).unwrap();
        guard.validate_length_prefixed(first.bytes(), true).unwrap();
        let record = guard.decoder_record().unwrap();
        let (configure, ready, body) = if fitted {
            (
                Kind::ConfigureFittedPresentation,
                Kind::FittedPresentationReady,
                target
                    .encode_fitted_decoder(configuration, &record)
                    .unwrap(),
            )
        } else {
            (
                Kind::ConfigurePresentation,
                Kind::PresentationReady,
                target.encode_decoder(configuration, &record).unwrap(),
            )
        };
        let mut worker = Worker::start(server);
        assert_ne!(worker.child.id(), std::process::id());
        let reply = worker.transact(configure, body.clone());
        assert_eq!(reply.header.kind, ready);
        assert_eq!(reply.body(), body);
        Self::submit(&mut worker, &first, true);
        Self {
            worker,
            owner,
            target,
            encoder,
            configuration,
            frame: 1,
        }
    }
    fn submit(worker: &mut Worker, unit: &EncodedAccessUnit, display: bool) {
        let kind = if display { Kind::Present } else { Kind::Decode };
        let reply = worker.transact(kind, worker::unit_payload(unit).unwrap());
        assert_eq!(
            reply.header.kind,
            if display {
                Kind::Presented
            } else {
                Kind::Decoded
            }
        );
        assert_eq!(reply.body(), unit.frame().as_raw().to_be_bytes());
    }
    fn next(&mut self, value: u8, display: bool) {
        let frame = FrameId::from_raw(self.frame);
        self.encoder
            .submit(
                &pattern(self.configuration.width, 32, value),
                frame,
                self.frame * 33_333,
                false,
            )
            .unwrap();
        let unit = self.encoder.poll_output().unwrap();
        assert_eq!(unit.frame(), frame);
        assert!(
            !unit.is_idr(),
            "reference-only decode must exercise dependent HEVC"
        );
        Self::submit(&mut self.worker, &unit, display);
        self.frame += 1;
    }
    fn canvas(&self, observer: &mut Observer) -> u32 {
        let children = observer.children(self.target.window());
        assert_eq!(
            children.len(),
            1,
            "one decoder owns exactly one private canvas"
        );
        children[0]
    }
}

#[test]
fn real_hevc_reuses_one_canvas_and_hard_kill_removes_remote_pixels() {
    for fitted in [false, true] {
        let server = Server::start();
        let mut observer = Observer::open(&server);
        let mut viewer = Viewer::start(&server, fitted);
        let canvas = viewer.canvas(&mut observer);
        for value in [31, 155, 227] {
            viewer.next(value, true);
            let before = viewer.owner.snapshot().unwrap();
            assert!(before.pixels().as_chunks::<4>().0.iter().any(|p| p[2] > 20));
            // This worker test covers submitted pixels and process lifetime.
            // The renderer's explicit idle maintenance is tested separately.
            assert!(viewer.owner.snapshot().unwrap().pixels() == before.pixels());
            assert_eq!(
                viewer.canvas(&mut observer),
                canvas,
                "never allocate a per-frame window"
            );
        }
        // SIGKILL runs no Rust/C destructor and sends no orderly Stop record.
        viewer.worker.crash();
        observer.wait_for_canvas_removal(viewer.target.window());
        observer.assert_black(viewer.target.window());
        observer.expose(viewer.target.window());
        observer.assert_black(viewer.target.window());
        assert_eq!(viewer.owner.presentation_target().unwrap(), viewer.target);
    }
}
#[test]
fn reference_only_decode_never_replaces_retained_presentation() {
    let server = Server::start();
    let mut observer = Observer::open(&server);
    let mut viewer = Viewer::start(&server, false);
    let first = viewer.owner.snapshot().unwrap();
    viewer.next(3, false);
    assert!(viewer.owner.snapshot().unwrap().pixels() == first.pixels());
    viewer.next(241, true);
    let next = viewer.owner.snapshot().unwrap();
    assert!(next.pixels() != first.pixels());
    assert!(viewer.owner.snapshot().unwrap().pixels() == next.pixels());
    viewer.worker.stop();
    observer.wait_for_canvas_removal(viewer.target.window());
    observer.assert_black(viewer.target.window());
}
#[test]
fn orderly_stop_clears_canvas_without_destroying_original_ui_target() {
    let server = Server::start();
    let mut observer = Observer::open(&server);
    let mut viewer = Viewer::start(&server, false);
    viewer.worker.stop();
    observer.wait_for_canvas_removal(viewer.target.window());
    observer.assert_black(viewer.target.window());
    observer.expose(viewer.target.window());
    observer.assert_black(viewer.target.window());
    assert_eq!(viewer.owner.presentation_target().unwrap(), viewer.target);
}
#[test]
fn parent_pipe_eof_releases_pixels_without_an_explicit_stop_request() {
    let server = Server::start();
    let mut observer = Observer::open(&server);
    let mut viewer = Viewer::start(&server, false);
    drop(viewer.worker.input.take());
    assert!(viewer.worker.child.wait().unwrap().success());
    observer.wait_for_canvas_removal(viewer.target.window());
    observer.assert_black(viewer.target.window());
}
#[test]
fn wrong_role_refusal_retires_the_child_and_all_retained_pixels() {
    let server = Server::start();
    let mut observer = Observer::open(&server);
    let mut viewer = Viewer::start(&server, false);
    let refusal = viewer.worker.transact(
        Kind::Capture,
        worker::capture_payload(FrameId::FIRST, 0, false),
    );
    assert_eq!(refusal.header.kind, Kind::Refused);
    assert_eq!(refusal.body(), (Error::WrongRole as u16).to_be_bytes());
    assert!(!viewer.worker.child.wait().unwrap().success());
    observer.wait_for_canvas_removal(viewer.target.window());
    observer.expose(viewer.target.window());
    observer.assert_black(viewer.target.window());
}
#[test]
fn a_canvas_resize_cannot_silently_change_the_original_view_mapping() {
    let server = Server::start();
    let mut observer = Observer::open(&server);
    let mut viewer = Viewer::start(&server, false);
    let canvas = viewer.canvas(&mut observer);
    observer.resize_away_and_back(canvas);
    // Current workers service native lifecycle events while idle. Retirement
    // must not wait for a follow-up frame, nor invent a reply identity for an
    // unsolicited event. Require EOF within the original response deadline.
    assert!(matches!(
        viewer
            .worker
            .output
            .as_ref()
            .unwrap()
            .recv_timeout(Duration::from_secs(3)),
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected)
    ));
    assert_eq!(viewer.worker.child.wait().unwrap().code(), Some(2));
    observer.wait_for_canvas_removal(viewer.target.window());
    observer.assert_black(viewer.target.window());
}

#[test]
fn hard_kill_clears_original_target_without_cooperative_stop() {
    let server = Server::start();
    let mut observer = Observer::open(&server);
    let mut viewer = Viewer::start(&server, false);
    let submitted = viewer.owner.snapshot().unwrap();
    assert!(
        submitted
            .pixels()
            .as_chunks::<4>()
            .0
            .iter()
            .any(|p| p[2] > 20)
    );
    // No child-count precondition: this exact pixel regression also executes
    // against the old renderer that painted directly into the borrowed parent.
    viewer.worker.crash();
    observer.wait_for_canvas_removal(viewer.target.window());
    observer.assert_black(viewer.target.window());
    assert_eq!(viewer.owner.presentation_target().unwrap(), viewer.target);
}
