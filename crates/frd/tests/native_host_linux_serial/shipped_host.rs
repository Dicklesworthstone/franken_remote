//! Shipped `frd run` and management commands, not a library-only composition.
//! Real binaries, TLS/QUIC/UDP, Store and worker retirement. `LocalAPI`, capture
//! payloads, sysfs and firewall tools are fixtures in disposable namespaces.
use super::*;
use frd::host_policy::{Approval, options::RunOptions};
use serde_json::Value;
use std::{
    io::{BufRead, BufReader},
    path::Path,
    process::{Child, Command, Stdio},
    thread::{self, JoinHandle},
    time::Instant,
};

fn binary() -> PathBuf {
    std::env::current_exe()
        .unwrap()
        .ancestors()
        .map(|dir| dir.join("frd"))
        .find(|frd| frd.is_file())
        .expect("build the shipped daemon first: cargo build -p frd --bin frd --locked")
}

struct Daemon {
    child: Child,
    events: Arc<Mutex<Vec<Value>>>,
    output: Option<JoinHandle<()>>,
}
impl Daemon {
    fn start(
        api: &fixture::Api,
        tools: &Tools,
        worker: &Path,
        config: &Path,
        overrides: bool,
    ) -> Self {
        // Only this child sees the mounted tools. No production bypass switch,
        // alternate trust rule, real firewall mutation or system-wide mount.
        let mut command = Command::new("unshare");
        command.args([
            "--mount",
            "--",
            "/bin/sh",
            "-ec",
            "mount --make-rprivate /; mount --bind \"$1\" /usr/sbin; shift; exec \"$@\"",
            "sh",
        ]);
        command
            .arg(&tools.0)
            .arg(binary())
            .args(["run", "--software-explicit", "--json"])
            .arg("--config")
            .arg(config)
            .arg("--socket")
            .arg(&api.path)
            .arg("--trust-roots")
            .arg(fixture::pki().join("ca.pem"))
            .arg("--worker")
            .arg(worker)
            .args(["--display", ":0", "--interface", "fr-fixture", "--port"])
            .arg(address().port().to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        if overrides {
            command.args(["--approval", "none", "--sharing", "own-user"]);
        }
        let mut child = command.spawn().unwrap();
        let events = Arc::new(Mutex::new(Vec::new()));
        let sink = events.clone();
        let stdout = child.stdout.take().unwrap();
        let output = thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let line = line.unwrap();
                assert!(line.len() < 8192, "bounded diagnostic record");
                let value = serde_json::from_str(&line).unwrap();
                let mut events = sink.lock().unwrap();
                assert!(events.len() < 128, "bounded event log");
                events.push(value);
            }
        });
        Self {
            child,
            events,
            output: Some(output),
        }
    }
    fn count(&self, event: &str) -> usize {
        self.events
            .lock()
            .unwrap()
            .iter()
            .filter(|v| v["event"] == event)
            .count()
    }
    fn stop(&mut self) {
        if self.child.try_wait().unwrap().is_none() {
            let pid = rustix::process::Pid::from_child(&self.child);
            rustix::process::kill_process(pid, rustix::process::Signal::TERM).unwrap();
        }
    }
    fn finish(&mut self) -> std::process::ExitStatus {
        let until = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                self.output.take().unwrap().join().unwrap();
                return status;
            }
            assert!(
                Instant::now() < until,
                "daemon failed to stop: {:?}",
                self.events
            );
            thread::sleep(Duration::from_millis(5));
        }
    }
}
impl Drop for Daemon {
    fn drop(&mut self) {
        // Test failures cannot leave the tested daemon or its pipe reader alive.
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(output) = self.output.take() {
            let _ = output.join();
        }
    }
}

#[derive(Clone, Copy)]
enum Update {
    Approval,
    Overridden,
    Corrupt,
}

#[test]
#[ignore = "explicit isolated user/mount/network namespace; synthetic ingress"]
fn shipped_frd_applies_saved_approval_changes_to_an_active_share() {
    update_active(Update::Approval);
}
#[test]
#[ignore = "explicit isolated user/mount/network namespace; synthetic ingress"]
fn shipped_frd_overrides_preserve_values_but_never_preserve_old_authority() {
    update_active(Update::Overridden);
}
#[test]
#[ignore = "explicit isolated user/mount/network namespace; synthetic ingress"]
fn shipped_frd_stops_on_malformed_policy_instead_of_using_old_defaults() {
    update_active(Update::Corrupt);
}

fn update_active(update: Update) {
    let api = fixture::Api::new();
    let tools = Tools::new();
    let path = tools.0.join("host-policy.json");
    let store = Store::new(&path).unwrap();
    store.update(Change::Approval(Approval::None)).unwrap();
    let (worker, trace) = persistent_desktop::source_script("normal");
    let mut host = Daemon::start(
        &api,
        &tools,
        &worker,
        &path,
        matches!(update, Update::Overridden),
    );
    let runtime = network::runtime();
    let cx = runtime.request_cx_with_budget(Budget::INFINITE);
    let mut policy_observed = false;
    let result = runtime.block_on(async {
        timeout(cx.now(), Duration::from_secs(12), async {
            until(&cx, || {
                host.count("listening") == 1 || host.count("stopped") > 0
            })
            .await;
            assert_eq!(host.count("listening"), 1, "{:?}", host.events);
            assert!(!trace.exists(), "no capture before admission");
            let native = fixture::client(&cx, address()).await;
            let viewer = Viewer::new(
                cx.clone(),
                native,
                persistent_desktop::offer(),
                fr_transport::quic::Policy::default(),
                Duration::from_secs(3),
            )
            .unwrap();
            let mut viewer =
                Box::pin(crate::desktop::client::Client::start(cx.clone(), viewer)).await;
            viewer.ready().await;
            assert_eq!(viewer.frames.first(), Some(&0));
            // Obtain a fresh observation renewal before withholding responses.
            let warm = network::clock(&cx) + 1_100_000;
            while network::clock(&cx) < warm {
                viewer.turn().await;
            }
            let pid = fs::read_to_string(&trace).unwrap();
            let started = Instant::now();
            let publication = if matches!(update, Update::Corrupt) {
                fs::write(&path, b"{invalid policy").unwrap();
                None
            } else {
                // Run the actual local management command against the same file.
                let output = Command::new(binary())
                    .args(["approval", "set", "local", "--config"])
                    .arg(&path)
                    .arg("--json")
                    .output()
                    .unwrap();
                assert!(output.status.success(), "{output:?}");
                Some(serde_json::from_slice::<Value>(&output.stdout).unwrap())
            };
            policy_observed = wait_for_policy(&host, &mut viewer.viewer, &cx).await;
            if !policy_observed {
                return; // Always stop/reap the actual daemon before reporting failure.
            }
            assert!(started.elapsed() < Duration::from_secs(2));
            assert!(
                !PathBuf::from(format!("/proc/{pid}")).exists(),
                "old capture reaped"
            );
            if matches!(update, Update::Overridden) {
                assert_eq!(host.count("listening"), 2);
                assert!(host.child.try_wait().unwrap().is_none());
                assert_eq!(
                    fs::read_to_string(&trace).unwrap(),
                    pid,
                    "new capture stays lazy"
                );
                assert_eq!(store.load().unwrap().approval_mode, Approval::Local);
            }
            if let Some(publication) = publication {
                assert_eq!(publication["live_application"], "unconfirmed");
                assert!(publication["applied_to_running_daemon"].is_null());
                assert!(publication["restart_required"].is_null());
            }
            drop(viewer);
        })
        .await
    });
    host.stop();
    let status = host.finish();
    result.unwrap();
    assert!(
        policy_observed,
        "saved policy was ignored by frd run: {:?}",
        host.events
    );
    check_stopped(&host, status, update, &tools);
}

fn check_stopped(host: &Daemon, status: std::process::ExitStatus, update: Update, tools: &Tools) {
    assert_eq!(
        host.count("share_ended"),
        1 + usize::from(matches!(update, Update::Overridden))
    );
    let events = host.events.lock().unwrap();
    assert!(
        !events.iter().any(|v| v["event"] == "cleanup_failed"),
        "{events:?}"
    );
    match update {
        Update::Overridden => assert!(status.success(), "{events:?}"),
        Update::Approval | Update::Corrupt => {
            assert_eq!(status.code(), Some(1), "{events:?}");
            assert_eq!(
                events.last().unwrap()["code"],
                if matches!(update, Update::Approval) {
                    "local_approval_unavailable"
                } else {
                    "host_policy_unavailable"
                }
            );
        }
    }
    let shares = if matches!(update, Update::Overridden) {
        2
    } else {
        1
    };
    assert_eq!(tools.state()["created"], shares);
    assert_eq!(tools.state()["deleted"], shares);
    assert!(
        UdpSocket::bind(address()).is_ok(),
        "original socket retired"
    );
}

/// Service real transport acknowledgements but deliberately do not dispatch or
/// answer any observation-renewal challenge. Otherwise an unacknowledged media
/// record can expire before the authority lease, falsely resembling a policy stop.
async fn wait_for_policy(host: &Daemon, viewer: &mut ViewerSession, cx: &Cx) -> bool {
    timeout(cx.now(), Duration::from_millis(1500), async {
        let mut connected = true;
        while host.count("stopped") == 0 && host.count("listening") < 2 {
            if connected {
                connected = match viewer.io() {
                    Ok((q, _)) => q.drive(cx, Duration::from_millis(5), || true).await.is_ok(),
                    Err(_) => false,
                };
            } else {
                sleep(cx.now(), Duration::from_millis(5)).await;
            }
        }
    })
    .await
    .is_ok()
}

#[test]
fn resolution_preserves_the_exact_watched_path_without_turning_saved_values_into_overrides() {
    let path = fixture::pki().join("resolved-startup-policy");
    let store = Store::new(&path).unwrap();
    store
        .update(Change::Sharing(host_policy::Sharing::Tailnet))
        .unwrap();
    let args = vec!["--config".into(), path.to_str().unwrap().into()];
    let options = RunOptions::parse(&args).unwrap();
    let effective = options.resolve().unwrap();
    assert_eq!(effective.policy_path, path);
    assert_eq!(effective.sharing, host_policy::Sharing::Tailnet);
    assert_eq!(options.sharing, None);
    assert_eq!(options.approval, None);
}
