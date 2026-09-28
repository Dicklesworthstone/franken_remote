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
    fn apply(delay_ms: u32, loss_percent: f32) -> Self {
        let delay = format!("{delay_ms}ms");
        let loss = format!("{loss_percent}%");
        let status = Command::new("tc")
            .args(["qdisc", "replace", "dev", "lo", "root", "netem", "delay"])
            .args([delay.as_str(), "loss", loss.as_str()])
            .status()
            .unwrap();
        assert!(status.success(), "tc netem {delay} {loss}");
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
    /// Viewer-motion-to-host-pointer times for the steps that completed.
    motion: Vec<Duration>,
    steps: usize,
    /// None: control still held at the end. Some: the client's completion.
    ended: Option<String>,
}

const STEPS: usize = 8;

fn row(delay_ms: u32, loss_percent: f32) -> Row {
    let mut s = Controlled::start_with(false, false);
    let netem = Netem::apply(delay_ms, loss_percent);
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
    if ended.is_some() || s.client.try_wait().unwrap().is_some() {
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
    } else {
        super::real_media::close_window(&s.viewer.display, s.window.0);
        let _ = wait_for(s.client, Duration::from_secs(30));
        s.daemon.finish();
    }
    Row {
        delay_ms,
        loss_percent,
        motion,
        steps,
        ended,
    }
}

/// Profiles control must survive: a clean link and a 40 ms RTT.
const MUST_HOLD: [(u32, f32); 2] = [(0, 0.0), (20, 0.0)];
/// Measured limits, printed with their scope and asserted only to hold or end
/// with the client's own outcome record: 0.5% loss at a 10 ms RTT, 60 and 100 ms
/// RTT, and 1% loss at a 40 ms RTT. On 2026-09-28 at load ~100-200 control ended
/// in some or all runs of each (see `PRESENTATION_FRESHNESS.md`).
const LIMITS: [(u32, f32); 4] = [(5, 0.5), (30, 0.0), (50, 0.0), (20, 1.0)];

fn summary(r: &Row) -> String {
    let mut sorted = r.motion.clone();
    sorted.sort();
    format!(
        "delay {} ms (RTT {} ms) loss {}%: steps {}/{STEPS}, max motion->host {:?} (harness polls ~50 ms), ended {:?}",
        r.delay_ms,
        2 * r.delay_ms,
        r.loss_percent,
        r.steps,
        sorted.last(),
        r.ended
    )
}

#[test]
#[ignore = "explicit isolated user/mount/network namespace; synthetic ingress; two Xvfb displays; real input agent; tc netem on the namespace loopback"]
fn controlled_session_under_namespace_delay_and_loss() {
    for (delay_ms, loss_percent) in MUST_HOLD {
        let r = row(delay_ms, loss_percent);
        println!("IMPAIRMENT {}", summary(&r));
        assert!(
            r.ended.is_none() && r.steps == STEPS,
            "control must hold: {}",
            summary(&r)
        );
    }
    for (delay_ms, loss_percent) in LIMITS {
        let r = row(delay_ms, loss_percent);
        println!("IMPAIRMENT LIMIT {}", summary(&r));
        // Holding or ending with the shipped client's own outcome record; never a
        // hang or a silent success after control was lost.
        if let Some(ended) = &r.ended {
            assert!(
                ended.contains("\"outcome\""),
                "untyped end: {}",
                summary(&r)
            );
        }
    }
}
