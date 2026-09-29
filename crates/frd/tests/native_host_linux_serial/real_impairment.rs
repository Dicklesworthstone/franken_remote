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
struct Netem;
impl Netem {
    /// `rate_kbit` 0 leaves the rate unlimited.
    fn apply(delay_ms: u32, loss_percent: f32, rate_kbit: u32) -> Self {
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

/// One view-only profile: a fresh `frd run` (no input agent) + shipped
/// `fr connect --view-only` on a clean link, then impaired while the host
/// desktop changes colour; each change must appear in the viewer window.
/// Shown changes, their times, how the row ended, and the held session's
/// media counters. `busy` rapid intermediate host changes (never near a
/// measured colour) precede each measured change, so loss actually hits video.
fn view_row(
    delay_ms: u32,
    loss_percent: f32,
    rate_kbit: u32,
    busy: i32,
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
    let netem = Netem::apply(delay_ms, loss_percent, rate_kbit);
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
        let (steps, mut times, ended, repaired) = view_row(delay_ms, loss_percent, rate_kbit, 0);
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
    // (repair alone disabled still held 3/3: recovery covers it). 5% loss is a
    // printed limit that must hold, show a change late, or end with a named
    // cause.
    for (delay_ms, loss_percent, required) in [(0, 0.0, true), (20, 1.0, true), (20, 5.0, false)] {
        let (steps, mut times, ended, media) = view_row(delay_ms, loss_percent, 0, 10);
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
