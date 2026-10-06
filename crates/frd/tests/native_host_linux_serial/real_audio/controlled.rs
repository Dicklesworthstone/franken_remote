//! Real control and playback on the same production session. The only synthetic
//! services are the parent harness's LocalAPI, CA and ingress fixtures. Input
//! effects come from an independent X client, audio from independent PulseAudio
//! monitor recordings, never from a protocol acknowledgement or a test counter.
use super::super::real_control::{Harness, eventually, host_pointer, indicator};
use super::*;

/// Independent host observer and viewer input driver, reusing the control
/// harness's Xlib declarations and bounded command/reply operations.
pub(super) struct Controls {
    host: Harness,
    viewer: Harness,
    origin: (i32, i32),
}
impl Controls {
    pub(super) fn start(host: &str, viewer: &str) -> Self {
        Self {
            host: Harness::start(HOST, host, "READY"),
            viewer: Harness::start(VIEWER, viewer, "READY"),
            origin: (0, 0),
        }
    }
    pub(super) fn ready(&mut self, window: u64, client: &mut Child) -> Result<(), String> {
        let focused = self.viewer.ask(&format!("focus {window}"));
        if focused.len() != 3 || focused[0] != "ORIGIN" {
            return Err("the independent window manager did not focus the viewer".into());
        }
        self.origin = (focused[1].parse().unwrap(), focused[2].parse().unwrap());
        let until = Instant::now() + Duration::from_secs(30);
        while Instant::now() < until {
            if client.try_wait().unwrap().is_some() {
                return Err("controller exited before independent input observation".into());
            }
            if self.pointer_follows((410, 310), Duration::from_millis(500)) {
                return if indicator(&mut self.host).is_some() {
                    Ok(())
                } else {
                    Err("input landed without the mandatory host indicator".into())
                };
            }
        }
        Err("viewer input never reached the host".into())
    }
    fn send(&mut self, command: &str) {
        assert_eq!(self.viewer.ask(command), ["OK"]);
    }
    fn motion(&mut self, at: (i32, i32)) {
        self.send(&format!(
            "move {} {}",
            self.origin.0 + at.0,
            self.origin.1 + at.1
        ));
    }
    fn pointer_follows(&mut self, at: (i32, i32), limit: Duration) -> bool {
        let mut nudge = 0;
        eventually(limit, || {
            nudge ^= 1;
            let target = (at.0 + nudge, at.1);
            self.motion(target);
            eventually(Duration::from_millis(400), || {
                host_pointer(&mut self.host).0 == target
            })
        })
    }
    fn counts(&mut self) -> Vec<u64> {
        let counts = self.host.ask("counts");
        assert_eq!(counts[0], "COUNTS");
        counts[1..].iter().map(|s| s.parse().unwrap()).collect()
    }
    fn exercise(&mut self) -> Result<(), String> {
        if !self.pointer_follows((410, 310), Duration::from_secs(10)) {
            return Err("pointer stopped following while audio was active".into());
        }
        let before = self.counts();
        for command in ["button 1 1", "button 1 0", "key 0x61 1", "key 0x61 0"] {
            self.send(command);
        }
        if !eventually(Duration::from_secs(10), || {
            let now = self.counts();
            // Exactly one native press/release of each kind, not SendEvent.
            now.len() == 4 && now.iter().zip(&before).all(|(n, b)| *n == *b + 1)
        }) {
            return Err("independent host observer did not see the key/button transitions".into());
        }
        if indicator(&mut self.host).is_none() {
            return Err("input lease ended during the audio scenario".into());
        }
        Ok(())
    }
    pub(super) fn stopped(&mut self) -> bool {
        eventually(Duration::from_secs(5), || indicator(&mut self.host).is_none())
    }
}

const HOST: &str = r#"
sig(x.XDefaultGC, c.c_void_p, D, I)
sig(x.XSetForeground, I, D, c.c_void_p, W)
sig(x.XFillRectangle, I, D, W, c.c_void_p, I, I, U, U)
sig(x.XQueryPointer, I, D, W, c.POINTER(W), c.POINTER(W), c.POINTER(I), c.POINTER(I),
    c.POINTER(I), c.POINTER(I), c.POINTER(U))
x.XSelectInput(d, root, 1 | 2 | 4 | 8)
x.XSetInputFocus(d, root, 1, 0)
x.XSync(d, 0)
gc = x.XDefaultGC(d, 0)
state = {"last": 0.0, "tick": 0, "counts": [0, 0, 0, 0]}
def idle():
    if time.monotonic() - state["last"] >= 0.2:
        state["last"] = time.monotonic()
        state["tick"] ^= 1
        x.XSetForeground(d, gc, 0x3172b4 if state["tick"] else 0xb45a31)
        x.XFillRectangle(d, root, gc, 560, 400, 64, 64)
        x.XFlush(d)
def event(e):
    if 2 <= e.type <= 5 and e.input.send_event == 0:
        state["counts"][e.type - 2] += 1
def command(cmd):
    if cmd[0] == "counts":
        say("COUNTS", *state["counts"])
    elif cmd[0] == "pointer":
        r, ch, rx, ry, wx, wy, mask = W(), W(), I(), I(), I(), I(), U()
        x.XQueryPointer(d, root, c.byref(r), c.byref(ch), c.byref(rx), c.byref(ry),
                        c.byref(wx), c.byref(wy), c.byref(mask))
        say("PTR", rx.value, ry.value, mask.value & 0x1f00)
    elif cmd[0] == "indicator":
        found = mapped(b"FrankenRemote - Stop remote control")
        say("IND", *(found[0][:3] if found else ("none",)))
say("READY")
serve(event, command, idle)
"#;

const VIEWER: &str = r#"
def command(cmd):
    if cmd[0] == "focus":
        w, a = int(cmd[1]), A()
        assert x.XGetWindowAttributes(d, w, c.byref(a)) and a.map_state == 2
        x.XSetInputFocus(d, w, 2, 0)
        x.XSync(d, 0)
        say("ORIGIN", a.x, a.y)
        return
    if cmd[0] == "move":
        t.XTestFakeMotionEvent(d, 0, int(cmd[1]), int(cmd[2]), 0)
    elif cmd[0] == "button":
        t.XTestFakeButtonEvent(d, int(cmd[1]), int(cmd[2]), 0)
    elif cmd[0] == "key":
        t.XTestFakeKeyEvent(d, x.XKeysymToKeycode(d, int(cmd[1], 0)), int(cmd[2]), 0)
    x.XSync(d, 0)
    say("OK")
say("READY")
serve(lambda e: None, command)
"#;

fn enabled(pulse: &Pulse) -> AudioOptions {
    AudioOptions {
        server: pulse.socket.clone(),
        sink: None,
    }
}
fn completion(report: &serde_json::Value, diagnostics: &str, played: bool) {
    assert_eq!(report["outcome"], "stopped", "{diagnostics}");
    assert_eq!(report["role"], "control", "{diagnostics}");
    assert_eq!(report["control_granted"], true, "{diagnostics}");
    assert_eq!(report["audio_requested"], true, "{diagnostics}");
    assert_eq!(report["audio_active"], played, "{diagnostics}");
    assert!(
        report["input_submitted_to_os"].as_u64().unwrap_or(0) >= 4,
        "{diagnostics}"
    );
    assert_eq!(report["audibility_proven"], false, "{diagnostics}");
    assert_eq!(report["cleanup_confirmed"], true, "{diagnostics}");
}

#[test]
#[ignore = "explicit isolated namespace; real XTest, HEVC/Opus workers and private PulseAudio null sinks"]
fn controlled_playback_and_input_share_the_same_live_session() {
    let host = Pulse::start("fr_host");
    let client = Pulse::start("fr_view");
    let mut player = Player::start(&host, WARMUP_HZ);
    let (report, observed, diagnostics) = share_session(
        &host,
        &client,
        Some(enabled(&host)),
        true,
        |recorder, controls, _, _| {
            let controls = controls.ok_or("missing real input observers")?;
            recorder
                .await_tone(WARMUP_HZ, Duration::from_secs(20))
                .ok_or("no host tone reached the controlling client")?;
            controls.exercise()?;
            recorder.drain();
            player.tone(MARKER_HZ);
            recorder
                .await_tone(MARKER_HZ, Duration::from_secs(5))
                .ok_or("controlled playback did not follow the host's frequency change")?;
            let (seen, total) = recorder.count_tone(MARKER_HZ, Duration::from_secs(4));
            if total < 180 || seen * 5 < total * 4 {
                return Err(format!(
                    "controlled playback sustained only {seen}/{total} blocks"
                ));
            }
            // More than a 3 s lease later: input still lands, not just startup.
            controls.exercise()
        },
    );
    assert_eq!(observed, Ok(()), "{diagnostics}");
    completion(&report, &diagnostics, true);
    assert!(
        report["audio_frames_submitted"].as_u64().unwrap_or(0) >= 180,
        "{diagnostics}"
    );
    let recorder = Recorder::start(&client);
    let (seen, total) = recorder.count_tone(MARKER_HZ, Duration::from_secs(2));
    assert!(
        total >= 50 && seen == 0,
        "audio survived client closure: {seen}/{total}"
    );
}

#[test]
#[ignore = "explicit isolated namespace; real control and private PulseAudio null sinks"]
fn a_controller_keeps_input_when_the_host_did_not_enable_audio() {
    let host = Pulse::start("fr_host");
    let client = Pulse::start("fr_view");
    let _player = Player::start(&host, WARMUP_HZ);
    let (report, observed, diagnostics) = share_session(
        &host,
        &client,
        None,
        true,
        |recorder, controls, _, _| {
            let controls = controls.ok_or("missing real input observers")?;
            controls.exercise()?;
            let (seen, total) = recorder.count_tone(WARMUP_HZ, Duration::from_secs(4));
            if total < 100 || seen != 0 {
                return Err(format!(
                    "unenabled audio reached controller: {seen}/{total}"
                ));
            }
            controls.exercise()
        },
    );
    assert_eq!(observed, Ok(()), "{diagnostics}");
    completion(&report, &diagnostics, false);
    assert_eq!(report["audio_absence"], "host_did_not_offer", "{diagnostics}");
}

/// Pin the original decoder child of THIS fr process with a pidfd. Checking a
/// PID's start ticks before `kill` still races exit/reuse between check and signal.
/// The helper resumes the pinned process on stdin EOF, including test unwinding;
/// a supervisor-killed decoder cannot redirect that cleanup to a replacement.
struct FrozenDecoder {
    helper: Child,
    input: Option<ChildStdin>,
}
impl FrozenDecoder {
    fn stop(client: &Child) -> Result<Self, String> {
        let mut helper = Command::new("python3")
            .args([
                "-u",
                "-c",
                include_str!("freeze_decoder.py"),
                &client.id().to_string(),
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|_| "could not start pidfd decoder fault helper")?;
        let mut stdout = helper.stdout.take().expect("piped helper stdout");
        let input = helper.stdin.take();
        // Install cleanup BEFORE any fallible acknowledgement wait. EOF also
        // resumes a child stopped while this caller times out or unwinds.
        let frozen = Self { helper, input };
        let (send, receive) = mpsc::sync_channel(1);
        thread::Builder::new()
            .name("fr-decoder-fault-reply".into())
            .spawn(move || {
                // One fixed-size reply: no unbounded line or payload buffer.
                let mut reply = [0; 8];
                let result = stdout.read_exact(&mut reply).map(|()| reply);
                let _ = send.send(result);
            })
            .map_err(|_| "could not observe pidfd decoder fault helper")?;
        match receive.recv_timeout(Duration::from_secs(8)) {
            Ok(Ok(reply)) if reply == *b"STOPPED\n" => Ok(frozen),
            _ => Err("decoder stop was not independently observed".into()),
        }
    }
}
impl Drop for FrozenDecoder {
    fn drop(&mut self) {
        drop(self.input.take());
        // Join the helper, never the foreign decoder. Its finally block uses
        // the original pidfd even when the decoder exited during the test.
        let _ = self.helper.wait();
    }
}

#[test]
#[ignore = "explicit isolated namespace; scoped SIGSTOP of the controlling client's real Opus child"]
fn a_stalled_opus_child_cannot_stop_control_or_lease_renewal() {
    let host = Pulse::start("fr_host");
    let client = Pulse::start("fr_view");
    let _player = Player::start(&host, WARMUP_HZ);
    let (report, observed, diagnostics) = share_session(
        &host,
        &client,
        Some(enabled(&host)),
        true,
        |recorder, controls, client, _| {
            let controls = controls.ok_or("missing real input observers")?;
            recorder
                .await_tone(WARMUP_HZ, Duration::from_secs(20))
                .ok_or("audio never became live before the stall")?;
            let original_indicator = indicator(&mut controls.host).ok_or("no input lease")?;
            let _frozen = FrozenDecoder::stop(client)?;
            let until = Instant::now() + Duration::from_secs(4);
            while Instant::now() < until {
                controls.exercise()?;
                if indicator(&mut controls.host) != Some(original_indicator)
                    || client.try_wait().unwrap().is_some()
                {
                    return Err(
                        "decoder stall replaced or ended the original control lease".into(),
                    );
                }
                thread::sleep(Duration::from_millis(150));
            }
            Ok(())
        },
    );
    assert_eq!(observed, Ok(()), "{diagnostics}");
    completion(&report, &diagnostics, true);
    assert!(
        report["audio_output_resets"].as_u64().unwrap_or(0) > 0
            || report["audio_absence"].as_str().is_some(),
        "decoder stall had no typed audio outcome: {diagnostics}"
    );
}

#[test]
#[ignore = "explicit isolated namespace; real control revocation and independent audio observation"]
fn local_host_stop_fences_control_and_audio_together() {
    let host = Pulse::start("fr_host");
    let client = Pulse::start("fr_view");
    let mut player = Player::start(&host, WARMUP_HZ);
    let (report, observed, diagnostics) = share_session(
        &host,
        &client,
        Some(enabled(&host)),
        true,
        |recorder, controls, client, stop| {
            let controls = controls.ok_or("missing real input observers")?;
            recorder
                .await_tone(WARMUP_HZ, Duration::from_secs(20))
                .ok_or("no audio before local revoke")?;
            controls.exercise()?;
            stop.request();
            if !controls.stopped() {
                return Err("input executor survived local stop".into());
            }
            let resting = host_pointer(&mut controls.host);
            let before = controls.counts();
            controls.motion((450, 350));
            controls.send("key 0x62 1");
            controls.send("key 0x62 0");
            // This tone did not exist before revocation, so pre-stop queues
            // cannot account for it being heard after the stop.
            recorder.drain();
            player.tone(MARKER_HZ);
            let (seen, total) = recorder.count_tone(MARKER_HZ, Duration::from_secs(3));
            if seen != 0 || total < 100 {
                return Err(format!(
                    "post-revoke audio reached the viewer: {seen}/{total}"
                ));
            }
            if host_pointer(&mut controls.host) != resting || controls.counts() != before {
                return Err("post-revoke input affected the host".into());
            }
            if !eventually(Duration::from_secs(10), || {
                client.try_wait().unwrap().is_some()
            }) {
                return Err("client did not finish after host revocation".into());
            }
            Ok(())
        },
    );
    assert_eq!(observed, Ok(()), "{diagnostics}");
    // Host effects and tone absence above are the evidence; require a typed
    // client ending rather than substituting a success receipt for those checks.
    assert_eq!(report["outcome"], "revoked", "{diagnostics}");
    assert_eq!(report["error"]["code"], "host_session_ended", "{diagnostics}");
}
