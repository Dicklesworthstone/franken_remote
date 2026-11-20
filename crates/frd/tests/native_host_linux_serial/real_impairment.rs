//! Real kernel impairment under a real controlled session: `tc netem` on the
//! namespace loopback, which carries exactly the host<->client QUIC traffic (both
//! X servers use Unix sockets, the fixture `LocalAPI` a Unix socket). Each row
//! starts a fresh `frd run --input-agent` + shipped `fr connect --control`
//! session on a clean link, then impairs it and keeps working: the host desktop
//! changes and viewer motion must reach the host.
//!
//! Scope, stated: namespace impairment on one machine - symmetric delay on both
//! directions of loopback and independent (iid) loss - not WAN, Wi-Fi, DERP or
//! Tailscale qualification, and the timings are this loaded host's, not claims.
use super::real_control::Controlled;
use super::real_media::set_root;
use super::shipped_client::wait_for;
use super::*;
use std::{process::Command, thread, time::Instant};

/// One loopback netem profile for the lifetime of the value.
pub(super) struct Netem;
impl Netem {
    /// `rate_kbit` 0 leaves the rate unlimited.
    pub(super) fn apply(delay_ms: u32, loss_percent: f32, rate_kbit: u32) -> Self {
        let delay = format!("{delay_ms}ms");
        let loss = format!("{loss_percent}%");
        let rate = format!("{rate_kbit}kbit");
        let mut tc = Command::new("tc");
        tc.args(["qdisc", "replace", "dev", "lo", "root", "netem", "delay"])
            .args([delay.as_str(), "loss", loss.as_str()]);
        if rate_kbit != 0 {
            tc.args(["rate", rate.as_str()]);
        }
        let status = tc.status().unwrap();
        assert!(status.success(), "tc netem {delay} {loss} {rate}");
        Self
    }
}
impl Netem {
    /// `delay_ms` on both directions, and `rate_kbit` on the host's datagrams
    /// to the client only: a narrow downlink, as on hotel Wi-Fi, without the
    /// client's acknowledgements and input waiting behind the host's video in
    /// loopback's one queue.
    pub(super) fn narrow_downlink(delay_ms: u32, rate_kbit: u32) -> Self {
        let delay = format!("{delay_ms}ms");
        let rate = format!("{rate_kbit}kbit");
        let port = address().port().to_string();
        let tc = |args: &[&str]| {
            let status = Command::new("tc").args(args).status().unwrap();
            assert!(status.success(), "tc {args:?}");
        };
        let all = ["0"; 16];
        tc(&[
            &[
                "qdisc", "replace", "dev", "lo", "root", "handle", "1:", "prio",
            ][..],
            &["bands", "2", "priomap"],
            &all,
        ]
        .concat());
        tc(&[
            "qdisc", "add", "dev", "lo", "parent", "1:1", "handle", "10:", "netem", "delay", &delay,
        ]);
        tc(&[
            "qdisc", "add", "dev", "lo", "parent", "1:2", "handle", "20:", "netem", "delay",
            &delay, "rate", &rate,
        ]);
        tc(&[
            "filter", "add", "dev", "lo", "parent", "1:", "protocol", "ip", "prio", "1", "u32",
            "match", "ip", "protocol", "17", "0xff", "match", "ip", "sport", &port, "0xffff",
            "flowid", "1:2",
        ]);
        Self
    }
}
impl Drop for Netem {
    fn drop(&mut self) {
        let _ = Command::new("tc")
            .args(["qdisc", "del", "dev", "lo", "root"])
            .status();
    }
}

#[derive(Debug)]
struct Row {
    delay_ms: u32,
    loss_percent: f32,
    rate_kbit: u32,
    /// Viewer-motion-to-host-pointer times for the steps that completed.
    motion: Vec<Duration>,
    steps: usize,
    /// None: control still held at the end. Some: the client's completion.
    ended: Option<String>,
    /// The held session's content-free `last_attempt_media` counters.
    media: Option<String>,
    /// Clean-link session starts that failed before any impairment.
    setup_retries: u32,
}

/// A fresh controlled session on a CLEAN link, before any impairment is
/// applied. Session startup is asserted strictly by the other control tests;
/// here a failed start (seen at load ~200: startup budgets, or a view that
/// aged out before control) is retried at most twice and counted, so the
/// matrix measures impairment rather than startup. A third failure fails the
/// test.
fn start_row() -> (Controlled, u32) {
    let mut retries = 0;
    loop {
        match std::panic::catch_unwind(|| Controlled::start_with(false, false)) {
            Ok(session) => return (session, retries),
            Err(_) if retries < 2 => {
                retries += 1;
                eprintln!("IMPAIRMENT setup failed on a clean link; retry {retries}");
            }
            Err(panic) => std::panic::resume_unwind(panic),
        }
    }
}

/// `fr`'s stopped completion from `attempts` through the `last_attempt_media`
/// object (decoded pictures, repair requests, recovered streams, presented-age
/// p50/p95 bucket bounds) or its `null`.
fn media(completion: &str) -> Option<String> {
    let start = completion.find("\"attempts\":")?;
    let rest = &completion[start..];
    let media = rest.find("\"last_attempt_media\":")? + "\"last_attempt_media\":".len();
    let end = if rest[media..].starts_with("null") {
        media + "null".len()
    } else {
        media + rest[media..].find('}')? + 1
    };
    Some(rest[..end].to_owned())
}

/// The decoded-picture count in [`media`]'s counters.
fn decoded_pictures(media: &str) -> Option<u64> {
    let start = media.find("\"decoded\":")? + "\"decoded\":".len();
    media[start..]
        .split(|c: char| !c.is_ascii_digit())
        .next()?
        .parse()
        .ok()
}

const STEPS: usize = 8;

fn row(delay_ms: u32, loss_percent: f32, rate_kbit: u32) -> Row {
    let (mut s, setup_retries) = start_row();
    let netem = Netem::apply(delay_ms, loss_percent, rate_kbit);
    let mut motion = Vec::new();
    let mut ended = None;
    for step in 0..STEPS {
        // A real screen change on the host, then viewer motion to a new point.
        let shade = i32::try_from(step * 25).unwrap();
        set_root(&s.host.display, (0x30 + shade, 0x60, 0x90 - shade));
        let target = (120 + 40 * i32::try_from(step).unwrap(), 150);
        let sent = Instant::now();
        let followed = s.pointer_follows(target, Duration::from_secs(8));
        if let Some(status) = s.client.try_wait().unwrap() {
            ended = Some(format!("exited {status}"));
            break;
        }
        if !followed {
            ended = Some("motion stopped reaching the host".into());
            break;
        }
        motion.push(sent.elapsed());
        thread::sleep(Duration::from_millis(400));
    }
    drop(netem);
    let steps = motion.len();
    let repaired = if ended.is_some() || s.client.try_wait().unwrap().is_some() {
        let output = wait_for(s.client, Duration::from_secs(30));
        let completion = format!(
            "{} | stderr {}",
            String::from_utf8_lossy(&output.stdout).trim(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
        let host = s.daemon.dump();
        let peer: String = host
            .split("PeerFinished")
            .nth(1)
            .map(|rest| rest.chars().take(300).collect())
            .unwrap_or_default();
        ended = Some(format!(
            "{}; completion {completion}; host peer outcome {peer}",
            ended.unwrap_or_default()
        ));
        s.daemon.finish();
        None
    } else {
        super::real_media::close_window(&s.viewer.display, s.window.0);
        let output = wait_for(s.client, Duration::from_secs(30));
        s.daemon.finish();
        media(&String::from_utf8_lossy(&output.stdout))
    };
    Row {
        delay_ms,
        loss_percent,
        rate_kbit,
        motion,
        steps,
        ended,
        media: repaired,
        setup_retries,
    }
}

/// How long [`narrow_episode`] keeps the link narrow in an episode.
const EPISODE: Duration = Duration::from_secs(8);
/// And on a sustained narrow link, long enough for the rate to settle.
const SUSTAINED: Duration = Duration::from_secs(20);

/// One controlled session across a narrow-link episode with incompressible
/// content on the host.
struct Episode {
    rate_kbit: u32,
    /// Viewer motion that reached the host while the link was narrow.
    during: usize,
    /// The client was still running when the link widened.
    alive: bool,
    /// Of [`STEPS`] viewer motions after the link widened, those that reached
    /// the host before the first that did not.
    resumed: usize,
    /// From the link widening to the first motion that reached the host.
    first_motion: Option<Duration>,
    /// `fr`'s completion when it ended on its own, or how the steps stopped.
    ended: Option<String>,
    /// The completion's content-free `last_attempt_media` counters.
    media: Option<String>,
}

/// A fresh controlled session with [`NOISE`] on the host: the link narrows to
/// `rate_kbit` (host to client, [`Netem::narrow_downlink`]) for `length`
/// while the viewer keeps moving, as a person would, then widens again and the
/// viewer's motion must reach the host.
fn narrow_episode(rate_kbit: u32, length: Duration) -> Episode {
    let (mut s, _) = start_row();
    // Clear of the 480x148 control indicator at the host's top left: covering
    // any of it is a local revoke.
    let mut painter = Command::new("python3")
        .args(["-c", NOISE, &s.host.display, "336", "270"])
        .spawn()
        .unwrap();
    // The encoder is at its steady busy rate before the link narrows.
    thread::sleep(Duration::from_secs(2));
    let target = |step: usize| (120 + 40 * i32::try_from(step % 8).unwrap(), 150);
    let netem = Netem::narrow_downlink(3, rate_kbit);
    let narrowed = Instant::now();
    let mut step = 0;
    let mut during = 0;
    while narrowed.elapsed() < length && s.client.try_wait().unwrap().is_none() {
        if s.pointer_follows(target(step), length.saturating_sub(narrowed.elapsed())) {
            during += 1;
        }
        step += 1;
    }
    // Which band carried what: the host's datagrams must be in the narrow one.
    let qdisc = Command::new("tc")
        .args(["-s", "qdisc", "show", "dev", "lo"])
        .output()
        .unwrap();
    println!(
        "EPISODE QDISC {}",
        String::from_utf8_lossy(&qdisc.stdout).replace('\n', " | ")
    );
    drop(netem);
    let widened = Instant::now();
    let alive = s.client.try_wait().unwrap().is_none();
    let mut resumed = 0;
    let mut first_motion = None;
    let mut ended = None;
    for _ in 0..STEPS {
        step += 1;
        let followed = s.pointer_follows(target(step), Duration::from_secs(8));
        if let Some(status) = s.client.try_wait().unwrap() {
            ended = Some(format!("exited {status}"));
            break;
        }
        if !followed {
            ended = Some("motion stopped reaching the host".into());
            break;
        }
        first_motion.get_or_insert_with(|| widened.elapsed());
        resumed += 1;
        thread::sleep(Duration::from_millis(400));
    }
    let _ = painter.kill();
    let _ = painter.wait();
    if s.client.try_wait().unwrap().is_none() {
        super::real_media::close_window(&s.viewer.display, s.window.0);
    }
    let output = wait_for(s.client, Duration::from_secs(30));
    let host = s.daemon.dump();
    s.daemon.finish();
    let completion = String::from_utf8_lossy(&output.stdout);
    let counters = media(&completion);
    if ended.is_some() {
        let fields: Vec<&str> = [
            "\"code\":",
            "\"input_suspended_ms\":",
            "\"repair_requests\":",
        ]
        .iter()
        .filter_map(|key| completion.find(key).map(|at| &completion[at..]))
        .map(|rest| rest.split([',', '}']).next().unwrap_or_default())
        .collect();
        let peer: String = host
            .split("PeerFinished")
            .nth(1)
            .map(|rest| rest.chars().take(300).collect())
            .unwrap_or_default();
        ended = ended.map(|why| format!("{why}; {}; host {peer}", fields.join(" ")));
    }
    Episode {
        rate_kbit,
        during,
        alive,
        resumed,
        first_motion,
        ended,
        media: counters,
    }
}

/// Profiles control must survive: a clean link and a 40 ms RTT.
const MUST_HOLD: [(u32, f32, u32); 2] = [(0, 0.0, 0), (20, 0.0, 0)];

/// The impairment matrix (one-way delay ms, loss %, rate kbit/s with 0
/// unlimited): about 5, 40 and 120 ms RTT x 0, 1 and 5% loss x unlimited and
/// 5 Mbit/s. Cells not required to hold are printed measured limits that must
/// hold or end with the client's own named outcome (`PRESENTATION_FRESHNESS.md`).
fn matrix() -> Vec<(u32, f32, u32)> {
    let mut cells = Vec::new();
    for delay_ms in [3, 20, 60] {
        for loss_percent in [0.0, 1.0, 5.0] {
            for rate_kbit in [0, 5000] {
                cells.push((delay_ms, loss_percent, rate_kbit));
            }
        }
    }
    cells
}

fn summary(r: &Row) -> String {
    let mut sorted = r.motion.clone();
    sorted.sort();
    format!(
        "delay {} ms (RTT {} ms) loss {}% rate {} kbit/s (0 unlimited): steps {}/{STEPS}, max motion->host {:?} (harness polls ~50 ms), media {:?}, setup retries {}, ended {:?}",
        r.delay_ms,
        2 * r.delay_ms,
        r.loss_percent,
        r.rate_kbit,
        r.steps,
        sorted.last(),
        r.media,
        r.setup_retries,
        r.ended
    )
}

#[test]
#[ignore = "explicit isolated user/mount/network namespace; synthetic ingress; two Xvfb displays; real input agent; tc netem on the namespace loopback"]
fn controlled_session_under_namespace_delay_and_loss() {
    for (delay_ms, loss_percent, rate_kbit) in MUST_HOLD {
        let r = row(delay_ms, loss_percent, rate_kbit);
        println!("IMPAIRMENT {}", summary(&r));
        assert!(
            r.ended.is_none() && r.steps == STEPS,
            "control must hold: {}",
            summary(&r)
        );
    }
    for (delay_ms, loss_percent, rate_kbit) in matrix()
        .into_iter()
        .filter(|cell| !MUST_HOLD.contains(cell))
    {
        let r = row(delay_ms, loss_percent, rate_kbit);
        println!("IMPAIRMENT LIMIT {}", summary(&r));
        // Holding or ending with the shipped client's own outcome record naming
        // why: never a hang, a silent success after control was lost, or the
        // generic failure code.
        if let Some(ended) = &r.ended {
            assert!(
                ended.contains("\"outcome\"") && !ended.contains("native_session_failed"),
                "untyped end: {}",
                summary(&r)
            );
        }
    }
}

/// Paints random noise into a 300x200 window on the host root at about 30 Hz
/// until killed: content an encoder cannot compress. Its top-left corner is at
/// the optional `X Y` arguments, else the root's. The measured root pixel (the
/// viewer window's centre) stays outside it.
const NOISE: &str = r#"
import ctypes as c, os, sys, time
x = c.CDLL("libX11.so.6")
P, U = c.c_void_p, c.c_ulong
x.XOpenDisplay.restype = P
x.XDefaultRootWindow.restype = U; x.XDefaultRootWindow.argtypes = [P]
x.XCreateSimpleWindow.restype = U
x.XCreateSimpleWindow.argtypes = [P, U, c.c_int, c.c_int, c.c_uint, c.c_uint, c.c_uint, U, U]
x.XMapWindow.argtypes = [P, U]
x.XDefaultVisual.restype = P; x.XDefaultVisual.argtypes = [P, c.c_int]
x.XDefaultGC.restype = P; x.XDefaultGC.argtypes = [P, c.c_int]
x.XCreateImage.restype = P
x.XCreateImage.argtypes = [P, P, c.c_uint, c.c_int, c.c_int, P, c.c_uint, c.c_uint, c.c_int, c.c_int]
x.XPutImage.argtypes = [P, U, P, P, c.c_int, c.c_int, c.c_int, c.c_int, c.c_uint, c.c_uint]
x.XFlush.argtypes = [P]
d = x.XOpenDisplay(sys.argv[1].encode())
assert d
W, H = 300, 200
X, Y = (int(sys.argv[2]), int(sys.argv[3])) if len(sys.argv) > 3 else (0, 0)
win = x.XCreateSimpleWindow(d, x.XDefaultRootWindow(d), X, Y, W, H, 0, 0, 0)
x.XMapWindow(d, win)
buf = c.create_string_buffer(W * H * 4)
img = x.XCreateImage(d, x.XDefaultVisual(d, 0), 24, 2, 0, c.cast(buf, P), W, H, 32, 0)
while True:
    c.memmove(buf, os.urandom(W * H * 4), W * H * 4)
    x.XPutImage(d, win, x.XDefaultGC(d, 0), img, 0, 0, 0, 0, W, H)
    x.XFlush(d)
    time.sleep(1 / 30)
"#;

/// One view-only profile: a fresh `frd run` (no input agent) + shipped
/// `fr connect --view-only` on a clean link, then impaired while the host
/// desktop changes colour; each change must appear in the viewer window.
/// Shown changes, their times, how the row ended, and the held session's
/// media counters. `busy` rapid intermediate host changes (never near a
/// measured colour) precede each measured change, so loss actually hits video.
/// `noise` runs [`NOISE`] on the host from 2 s before the link is impaired
/// until the measured changes are done. `hold` keeps the impaired link that
/// long before the first measured change.
fn view_row(
    delay_ms: u32,
    loss_percent: f32,
    rate_kbit: u32,
    busy: i32,
    noise: bool,
    hold: Duration,
) -> (usize, Vec<Duration>, Option<String>, Option<String>) {
    use super::real_control::Daemon;
    use super::real_media::{
        Xvfb, close_window, connect, near, sibling, warp_pointer, window_pixels,
    };
    use super::shipped_client::ClientApi;
    let (fr, worker) = (sibling("fr"), sibling("fr-media-worker"));
    let host = Xvfb::start("640x480x24");
    let viewer = Xvfb::start("800x600x24");
    warp_pointer(&host.display, 16, 16);
    set_root(&host.display, super::real_media::COLOUR);
    let daemon = Daemon::start_monitored(&host.display, &worker, None, false, None, None);
    let client_api = ClientApi::new();
    let roots = fixture::pki().join("ca.pem");
    let mut client = connect(&fr, &client_api.path, &roots, &viewer.display, &worker);
    let shows = |client: &mut std::process::Child, colour, limit: Duration| {
        let until = Instant::now() + limit;
        while Instant::now() < until {
            if client.try_wait().unwrap().is_some() {
                return None;
            }
            if let Some((window, _)) = window_pixels(&viewer.display)
                .into_iter()
                .find(|(_, pixel)| near(*pixel, colour))
            {
                return Some(window);
            }
            thread::sleep(Duration::from_millis(20));
        }
        None
    };
    let first = shows(
        &mut client,
        super::real_media::COLOUR,
        Duration::from_secs(45),
    )
    .expect("first picture on the clean link");
    let mut painter = noise.then(|| {
        Command::new("python3")
            .args(["-c", NOISE, &host.display])
            .spawn()
            .unwrap()
    });
    if painter.is_some() {
        // The encoder is at its steady busy rate before the link narrows.
        thread::sleep(Duration::from_secs(2));
    }
    let netem = Netem::apply(delay_ms, loss_percent, rate_kbit);
    thread::sleep(hold);
    let mut times = Vec::new();
    let mut ended = None;
    for step in 0..6 {
        for burst in 0..busy {
            set_root(&host.display, (0x80, 0x10 + 8 * burst, 0xc0));
            thread::sleep(Duration::from_millis(30));
        }
        let shade = step * 30;
        let colour = (0x20 + shade, 0xa0 - shade, 0x40);
        set_root(&host.display, colour);
        let sent = Instant::now();
        if shows(&mut client, colour, Duration::from_secs(10)).is_none() {
            ended = Some(if client.try_wait().unwrap().is_some() {
                "client exited".to_owned()
            } else {
                "change not shown within 10 s".to_owned()
            });
            break;
        }
        times.push(sent.elapsed());
    }
    if let Some(painter) = &mut painter {
        let _ = painter.kill();
        let _ = painter.wait();
    }
    drop(netem);
    if client.try_wait().unwrap().is_none() {
        close_window(&viewer.display, first);
    }
    let output = wait_for(client, Duration::from_secs(30));
    let repaired = media(&String::from_utf8_lossy(&output.stdout));
    if ended.is_some() {
        let host = daemon.dump();
        let viewer: String = host
            .split("ShareEnded")
            .nth(1)
            .map(|rest| rest.chars().take(200).collect())
            .unwrap_or_default();
        ended = Some(format!(
            "{}; completion {} | stderr {}; host share {viewer}",
            ended.unwrap_or_default(),
            String::from_utf8_lossy(&output.stdout).trim(),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    daemon.finish();
    (times.len(), times, ended, repaired)
}

#[test]
#[ignore = "explicit isolated user/mount/network namespace; synthetic ingress; two Xvfb displays; tc netem on the namespace loopback"]
fn view_only_session_under_namespace_delay_and_loss() {
    // Must hold: every change shown on a clean link, at a 40 ms RTT, and at a
    // 40 ms RTT with 1% loss. Measured limits (printed; asserted only to hold,
    // show a change late, or end with the client's outcome record): the rest of
    // the matrix and 200 ms RTT. (delay ms one way, loss %, rate kbit/s with 0
    // unlimited.)
    let must: [(u32, f32, u32); 3] = [(0, 0.0, 0), (20, 0.0, 0), (20, 1.0, 0)];
    // The matrix cells not required above, plus 200 ms RTT.
    let limits: Vec<_> = matrix()
        .into_iter()
        .filter(|cell| !must.contains(cell))
        .chain([(100, 0.0, 0)])
        .collect();
    for (delay_ms, loss_percent, rate_kbit, required) in must
        .iter()
        .map(|&(d, l, r)| (d, l, r, true))
        .chain(limits.iter().map(|&(d, l, r)| (d, l, r, false)))
    {
        let (steps, mut times, ended, repaired) =
            view_row(delay_ms, loss_percent, rate_kbit, 0, false, Duration::ZERO);
        times.sort();
        let rate = if rate_kbit == 0 {
            "unlimited".to_owned()
        } else {
            format!("{rate_kbit} kbit/s")
        };
        let line = format!(
            "delay {delay_ms} ms (RTT {} ms) loss {loss_percent}% rate {rate}: changes shown {steps}/6, median {:?}, max {:?}, media {repaired:?}, ended {ended:?}",
            2 * delay_ms,
            times.get(times.len() / 2),
            times.last()
        );
        if required {
            println!("VIEW IMPAIRMENT {line}");
            assert!(steps == 6 && ended.is_none(), "view must hold: {line}");
        } else {
            println!("VIEW IMPAIRMENT LIMIT {line}");
            if let Some(ended) = &ended {
                assert!(
                    (ended.contains("\"outcome\"") && !ended.contains("native_session_failed"))
                        || ended.contains("not shown"),
                    "untyped end: {line}"
                );
            }
        }
    }
}

#[test]
#[ignore = "explicit isolated user/mount/network namespace; synthetic ingress; two Xvfb displays; tc netem on the namespace loopback"]
fn view_only_busy_screen_under_namespace_loss() {
    // Ten rapid host changes before each of the six measured ones, so about 60
    // pictures cross the impaired link and loss actually hits video: repair
    // and recovery must work for every measured change to appear. The clean
    // row and 40 ms RTT with 1% loss must hold. The latter held 10/10 with
    // reference recovery offered (2abb861); planted negatives on 2026-09-28:
    // without recovery it froze 2/3, without repair AND recovery it ended 2/3
    // (repair alone disabled still held 3/3: recovery covers it). 40 ms with
    // 5% loss needs several reference recoveries in one session: with 7
    // lifetime channel pairs (one recovery for a view-only session) it showed
    // 3, 3 and 2 of 6 and ended video_reference_lost; with 31 (59a5037) it
    // held 13/13 on ts1, so it is required too. The printed-limit branch below
    // stays for rows that are not.
    for (delay_ms, loss_percent, required) in [(0, 0.0, true), (20, 1.0, true), (20, 5.0, true)] {
        let (steps, mut times, ended, media) =
            view_row(delay_ms, loss_percent, 0, 10, false, Duration::ZERO);
        times.sort();
        let line = format!(
            "delay {delay_ms} ms (RTT {} ms) loss {loss_percent}% busy: changes shown {steps}/6, median {:?}, max {:?}, media {media:?}, ended {ended:?}",
            2 * delay_ms,
            times.get(times.len() / 2),
            times.last()
        );
        println!("VIEW BUSY IMPAIRMENT {line}");
        if required {
            assert!(steps == 6 && ended.is_none(), "busy view must hold: {line}");
        } else if let Some(ended) = &ended {
            assert!(
                (ended.contains("\"outcome\"") && !ended.contains("native_session_failed"))
                    || ended.contains("not shown"),
                "untyped end: {line}"
            );
        }
    }
}

#[test]
#[ignore = "explicit isolated user/mount/network namespace; synthetic ingress; two Xvfb displays; tc netem on the namespace loopback"]
fn view_only_incompressible_content_over_a_narrow_link() {
    // A host window of random noise keeps the encoder at its 2 Mbit/s target
    // (the test host's) while six measured root changes cross. Before the
    // encoder had a VBV, libx265 emitted single pictures of up to 77 KB there,
    // about 300 ms of link time at 2 Mbit/s, and the session ended within
    // seconds even on a 3 or 4 Mbit/s link (0/6 and 1/6 shown). 3 Mbit/s must
    // now hold; 2 Mbit/s (no headroom) is a printed limit that must hold, show
    // a change late, or end with a named cause.
    //
    // Links at half and 0.4 times the target (a hotel's, fr-7al9) must hold
    // too. Before capture waited for a queued path (path_queue.rs), no such
    // row held on trj (0 of 26, 1-4 of 6 changes shown): QUIC lost nothing,
    // so 280 KB of pictures queued at the bottleneck and the viewer's records
    // expired behind them. With it, 48 of 48 held at load 11-37. One large
    // change picture still queues for about 0.7 s at 1 Mbit/s.
    for (rate_kbit, required) in [(3000, true), (2000, false), (1000, true), (800, true)] {
        let (steps, mut times, ended, media) = view_row(3, 0.0, rate_kbit, 0, true, Duration::ZERO);
        times.sort();
        let line = format!(
            "rate {rate_kbit} kbit/s, noise window: changes shown {steps}/6, median {:?}, max {:?}, media {media:?}, ended {ended:?}",
            times.get(times.len() / 2),
            times.last()
        );
        println!("VIEW NOISE IMPAIRMENT {line}");
        if required {
            assert!(
                steps == 6 && ended.is_none(),
                "noisy view must hold: {line}"
            );
        } else if let Some(ended) = &ended {
            assert!(
                (ended.contains("\"outcome\"") && !ended.contains("native_session_failed"))
                    || ended.contains("not shown"),
                "untyped end: {line}"
            );
        }
    }
}

#[test]
#[ignore = "explicit isolated user/mount/network namespace; synthetic ingress; two Xvfb displays; tc netem on the namespace loopback"]
fn view_only_session_whose_route_lengthens_keeps_its_pictures() {
    // A busy view (the noise window) whose round trip grows from the clean
    // link's to 120 ms, or to 300 ms, as on a switch from a direct path to a
    // relay, then holds there for 8 s. A longer route is not a queue, so
    // capture must not wait for it (path_queue.rs). Found only by its
    // baseline window, the 300 ms route was a queue for 5-10 s: 73-79
    // pictures decoded against 160 at 120 ms (trj); with the drain probe
    // 156-157, and 163-171 with no queue gate at all.
    let mut decoded = Vec::new();
    for delay_ms in [60, 150] {
        let (steps, mut times, ended, media) =
            view_row(delay_ms, 0.0, 0, 0, true, Duration::from_secs(8));
        times.sort();
        let pictures = media.as_deref().and_then(decoded_pictures);
        let line = format!(
            "RTT {} ms, noise window, 8 s hold: changes shown {steps}/6, median {:?}, decoded {pictures:?}, ended {ended:?}",
            2 * delay_ms,
            times.get(times.len() / 2)
        );
        println!("VIEW ROUTE {line}");
        assert!(
            steps == 6 && ended.is_none(),
            "a longer route must hold: {line}"
        );
        decoded.push(pictures.expect("the held session reports its decoded pictures"));
    }
    assert!(
        decoded[1] * 5 >= decoded[0] * 4,
        "a 300 ms route must keep 4/5 of the 120 ms route's pictures: {decoded:?}"
    );
}

/// [`view_only_incompressible_content_over_a_narrow_link`] with the link held
/// narrow for [`SUSTAINED`] before the six changes, still narrow while they
/// cross: what a long narrow link costs a view-only session's pictures and
/// change latency (fr-7al9).
#[test]
#[ignore = "measurement: about 2 min of namespace sessions; run by hand"]
fn view_only_session_across_a_sustained_narrow_link() {
    for rate_kbit in [1500, 1000] {
        let (steps, mut times, ended, media) = view_row(3, 0.0, rate_kbit, 0, true, SUSTAINED);
        times.sort();
        println!(
            "VIEW SUSTAINED rate {rate_kbit} kbit/s for {SUSTAINED:?}: changes shown {steps}/6, median {:?}, max {:?}, decoded {:?}, media {media:?}, ended {ended:?}",
            times.get(times.len() / 2),
            times.last(),
            media.as_deref().and_then(decoded_pictures)
        );
    }
}

/// Measures [`narrow_episode`] at three rates, below and above the hosts'
/// 2 Mbit/s target: a narrow link must cost the session's pictures, not the
/// session (fr-7al9).
#[test]
#[ignore = "measurement: about 1.5 min of namespace sessions; run by hand"]
fn controlled_session_across_a_narrow_link_episode() {
    for rate_kbit in [3000, 1000, 800] {
        let e = narrow_episode(rate_kbit, EPISODE);
        println!(
            "EPISODE rate {} kbit/s for {:?}: during {}, alive at widening {}, resumed {}/{STEPS}, first motion after {:?}, ended {:?}",
            e.rate_kbit, EPISODE, e.during, e.alive, e.resumed, e.first_motion, e.ended
        );
    }
}

/// The same with the link narrow for [`SUSTAINED`], at rates the capture gate
/// alone carries (fr-7al9): how many pictures the viewer decoded, and whether
/// its motion kept reaching the host, measure what the narrow link cost.
#[test]
#[ignore = "measurement: about 1.5 min of namespace sessions; run by hand"]
fn controlled_session_across_a_sustained_narrow_link() {
    for rate_kbit in [1500, 1000] {
        let e = narrow_episode(rate_kbit, SUSTAINED);
        println!(
            "SUSTAINED rate {} kbit/s for {:?}: during {}, alive at widening {}, resumed {}/{STEPS}, first motion after {:?}, media {:?}, ended {:?}",
            e.rate_kbit, SUSTAINED, e.during, e.alive, e.resumed, e.first_motion, e.media, e.ended
        );
    }
}
