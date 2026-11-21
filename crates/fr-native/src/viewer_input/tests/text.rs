//! Production batch/timestamp handling. The Target supplies only the existing
//! native-loop clock seam; no authority or host text-delivery claim is made.
use super::*;

fn target() -> Recording {
    Recording {
        start: Instant::now(),
        probe: Arc::new(Probe::default()),
        caps: capabilities().with(Capability::Text),
    }
}

#[test]
fn native_send_retains_the_x_event_age_before_dequeue() {
    let mut timeline = Timeline::new(1000, ClientInstant(1_000_000));
    timeline.barrier(1030).unwrap();
    let events = [Raw {
        kind: text::SEND,
        time: 1010,
        ..Raw::default()
    }];
    let captured = captured_batch(
        &events,
        &mut timeline,
        1030,
        ClientInstant(1_030_000),
        &target(),
        None,
        &mut Decoder::new(&[[0; 4]; 256], capabilities()),
    )
    .unwrap()
    .unwrap();
    // Includes the existing conservative millisecond quantization. The Send
    // gets neither the dequeue timestamp nor a fresh native lifetime.
    assert_eq!(captured.at[0], Some(ClientInstant(1_009_000)));
}

#[test]
fn expired_send_is_refused_without_retimestamping_or_hiding_later_input() {
    let mut timeline = Timeline::new(1000, ClientInstant(1_000_000));
    timeline.barrier(1100).unwrap();
    let events = [
        Raw {
            kind: text::SEND,
            time: 1000,
            ..Raw::default()
        },
        Raw {
            kind: 5,
            time: 1099,
            ..Raw::default()
        },
    ];
    let captured = captured_batch(
        &events,
        &mut timeline,
        1100,
        ClientInstant(1_100_000),
        &target(),
        None,
        &mut Decoder::new(&[[0; 4]; 256], capabilities()),
    )
    .unwrap()
    .unwrap();
    assert_eq!(captured.at[0], None);
    assert_eq!(captured.at[1], Some(ClientInstant(1_098_000)));
}

#[test]
fn editor_keys_and_cancellation_never_enter_the_remote_key_decoder() {
    let (_, layout) = viewport();
    let mut decoder = Decoder::new(&[[0; 4]; 256], capabilities());
    for kind in [14, 15, text::RETIRED, 17, 18] {
        assert!(
            decoder
                .event(
                    Raw {
                        kind,
                        detail: 38,
                        ..Raw::default()
                    },
                    &layout,
                )
                .unwrap()
                .is_none()
        );
    }
    assert!(!decoder.has_held());
}

/// The same explicit Target fixture as the physical input tests, extended only
/// to record a validated native text handoff. Source authority, host admission
/// and OS delivery are deliberately outside this X11 bridge test.
struct TextTarget {
    recording: Recording,
    context: text::Context<u64>,
}
impl Target for TextTarget {
    fn clock(&self) -> Result<ClientInstant, StopReason> {
        self.recording.clock()
    }
    fn capabilities(&self) -> Capabilities {
        self.recording.capabilities()
    }
    fn push(&mut self, event: Event, sampled: ClientInstant) -> Result<(), StopReason> {
        self.recording.push(event, sampled)
    }
    fn ready(&self) {
        self.recording.ready();
    }
    fn stop(&self, reason: StopReason) {
        self.recording.stop(reason);
    }
    fn text_offer(&mut self, held: bool) -> Result<text::Offer, StopReason> {
        self.context.offer(Some(1), !held)
    }
    fn text_retire(&mut self, serial: u64) {
        let _ = self.context.take(serial);
    }
    fn text_send(
        &mut self,
        commit: &text::Commit,
        sampled: Option<ClientInstant>,
        held: bool,
    ) -> Result<bool, StopReason> {
        if self.context.take(commit.serial).is_none() {
            return Ok(false);
        }
        let Some((value, sampled, _until)) = commit.validate(sampled, self.clock()?, held) else {
            return Ok(false);
        };
        self.push(Event::Text(CommittedText::new(value).unwrap()), sampled)?;
        Ok(true)
    }
}

struct TextCapture {
    probe: Arc<Probe>,
    task: Option<JoinHandle<Result<(), StopReason>>>,
}
impl TextCapture {
    fn start(display: CString, window: Window) -> Self {
        let recording = target();
        let probe = recording.probe.clone();
        let (_, layout) = viewport();
        let task = thread::spawn(move || {
            run(
                &display,
                window,
                &layout,
                &mut TextTarget {
                    recording,
                    context: text::Context::default(),
                },
            )
        });
        let capture = Self {
            probe,
            task: Some(task),
        };
        wait(|| capture.probe.ready.load(Ordering::Acquire) || capture.finished());
        assert!(
            !capture.finished(),
            "native text capture stopped during setup"
        );
        capture
    }
    fn finished(&self) -> bool {
        self.task.as_ref().is_none_or(JoinHandle::is_finished)
    }
}
impl Drop for TextCapture {
    fn drop(&mut self) {
        self.probe.stopped.store(true, Ordering::Release);
        self.probe.pause.store(false, Ordering::Release);
        if let Some(task) = self.task.take() {
            let _ = task.join();
        }
    }
}

#[test]
fn native_send_crosses_the_rust_bridge_once_without_exporting_editor_keys() {
    let _serial = SERIAL.lock().unwrap();
    let Some((mut peer, display)) = Peer::new() else {
        return;
    };
    let capture = TextCapture::start(display, peer.window);
    let title = peer.query("title");
    assert!(
        title.contains("F7: Send Text"),
        "native editor unavailable: {title}"
    );
    // The existing independent peer's default XKB layout: F7 opens locally,
    // A produces a draft, then the original Ctrl+Return gesture requests Send.
    for command in [
        "key 73 1", "key 73 0", "key 38 1", "key 38 0", "key 37 1", "key 36 1", "key 36 0",
        "key 37 0",
    ] {
        peer.command(command);
    }
    wait(|| !capture.probe.events.lock().unwrap().is_empty() || capture.finished());
    assert!(!capture.finished());
    {
        let events = capture.probe.events.lock().unwrap();
        assert_eq!(events.len(), 1, "editor keys must remain local");
        let Event::Text(value) = &events[0].0 else {
            panic!("native Send did not reach the text boundary");
        };
        assert_eq!(value.as_str(), "a");
    }
    // The editor's held Ctrl/Return releases remain local after closing. A
    // later deliberate physical key still has its ordinary press/release pair.
    peer.command("key 38 1");
    peer.command("key 38 0");
    wait(|| capture.probe.events.lock().unwrap().len() >= 3 || capture.finished());
    assert!(!capture.finished());
    let events = capture.probe.events.lock().unwrap();
    assert_eq!(events.len(), 3);
    for (event, transition) in events[1..]
        .iter()
        .zip([KeyTransition::Press, KeyTransition::Release])
    {
        assert!(matches!(event.0, Event::Key { key, transition: actual }
            if key.usage() == 4 && actual == transition));
    }
}
