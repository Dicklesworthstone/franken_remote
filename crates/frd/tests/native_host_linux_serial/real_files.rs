//! The shipped `fr connect --control --send PATH` against the production
//! `frd run --input-agent --files DIR`: explicitly selected regular files go
//! from the viewer to the host over the controller's one-use file lane on the
//! real UDP/TLS/QUIC session (ATP full-object profile, the host's bounded disk
//! worker, a private staging file and a no-replace atomic rename), attached
//! only under the controller's input lease. Effects are observed directly on
//! the host's drop directory; nothing here reads `FrankenRemote` state for them.
//!
//! Fixtures, stated: the tailnet `LocalAPI`, CA and ingress firewall are the
//! existing namespace fixtures; the viewer harness plays the window manager
//! and the user as in `real_control`. A namespace run is not a live tailnet,
//! a desktop file manager or a drag-and-drop UI.
use super::real_control::{Controlled, eventually, indicator, signal};
use super::real_media::close_window;
use super::shipped_client::wait_for;
use super::*;
use frd::native_files::{Directory, Limits};
use std::{thread, time::Instant};

/// A private directory on the namespace's own /run tmpfs (root-owned there).
struct Dir(PathBuf);
impl Dir {
    fn new(label: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let dir = PathBuf::from(format!(
            "/run/fr-files-{label}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
        Self(dir)
    }
    fn entries(&self) -> Vec<String> {
        let mut names: Vec<_> = fs::read_dir(&self.0)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }
    /// The operator's validated, descriptor-pinned drop directory.
    fn directory(&self) -> Directory {
        Directory::open(&self.0, Limits::default()).unwrap()
    }
}
impl Drop for Dir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn random(len: usize) -> Vec<u8> {
    let mut bytes = vec![0; len];
    getrandom::fill(&mut bytes).unwrap();
    bytes
}
fn sha256(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    ring::digest::digest(&ring::digest::SHA256, bytes)
        .as_ref()
        .iter()
        .fold(String::new(), |mut hex, b| {
            let _ = write!(hex, "{b:02x}");
            hex
        })
}
/// The shipped client's JSON completion.
fn completion(output: &std::process::Output, host: &str) -> serde_json::Value {
    let stdout = String::from_utf8_lossy(&output.stdout);
    serde_json::from_str(stdout.trim()).unwrap_or_else(|_| {
        panic!(
            "fr: {stdout} {}; host: {host}",
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

#[test]
#[ignore = "explicit isolated user/mount/network namespace; synthetic ingress; two Xvfb displays; real input agent and file lane"]
fn fr_connect_send_publishes_identical_bytes_into_frd_run_files() {
    let drop_dir = Dir::new("drop");
    let source = Dir::new("source");
    // Non-ASCII, portable: the host name is exactly the final path component.
    let name = "données-été λ 📄.bin";
    let bytes = random(3 * 1024 * 1024);
    fs::write(source.0.join(name), &bytes).unwrap();
    let mut s = Controlled::start_full(
        false,
        false,
        Some(drop_dir.directory()),
        &[source.0.join(name)],
    );
    let published = eventually(Duration::from_secs(60), || drop_dir.entries() == [name]);
    assert!(
        published,
        "drop directory {:?}; host: {}",
        drop_dir.entries(),
        s.daemon.dump()
    );
    let received = fs::read(drop_dir.0.join(name)).unwrap();
    assert_eq!(received.len(), bytes.len());
    assert_eq!(sha256(&received), sha256(&bytes));
    println!(
        "files e2e: host holds exactly {:?}: {} bytes, sha256 {}",
        drop_dir.entries(),
        received.len(),
        sha256(&received)
    );
    // Control keeps working alongside the file lane.
    assert!(
        s.pointer_follows((222, 111), Duration::from_secs(10)),
        "control after the file: {}",
        s.daemon.dump()
    );
    // Let the host's publication proof reach the client before closing.
    thread::sleep(Duration::from_secs(2));
    close_window(&s.viewer.display, s.window.0);
    let output = wait_for(s.client, Duration::from_secs(30));
    let report = completion(&output, &s.daemon.dump());
    println!("fr completion: {report}");
    assert_eq!(report["outcome"], "stopped", "{report}");
    assert_eq!(report["control_granted"], true, "{report}");
    assert_eq!(report["files_requested"], 1, "{report}");
    assert_eq!(
        report["files_sent"],
        serde_json::json!([{"index": 0, "bytes": bytes.len(), "durable": true}]),
        "{report}"
    );
    assert_eq!(report["files_refused"], serde_json::json!([]), "{report}");
    assert_eq!(report["files_absence"], serde_json::Value::Null, "{report}");
    let text = report.to_string();
    for secret in ["données", "λ", "📄", source.0.to_str().unwrap()] {
        assert!(!text.contains(secret), "completion leaked a name: {text}");
    }
    // The published file survives the session; nothing else was left behind.
    assert_eq!(drop_dir.entries(), [name]);
    assert_eq!(
        sha256(&fs::read(drop_dir.0.join(name)).unwrap()),
        sha256(&bytes)
    );
    s.daemon.finish();
}

#[test]
#[ignore = "explicit isolated user/mount/network namespace; synthetic ingress; two Xvfb displays; real input agent and file lane"]
fn fr_connect_send_never_overwrites_a_name_the_host_already_holds() {
    let drop_dir = Dir::new("drop");
    let source = Dir::new("source");
    let original = b"original host notes: never overwritten\n".to_vec();
    fs::write(drop_dir.0.join("notes.txt"), &original).unwrap();
    let fresh = random(64 * 1024);
    fs::write(source.0.join("fresh.bin"), &fresh).unwrap();
    let replacement = b"client notes that must not land on the host\n".to_vec();
    fs::write(source.0.join("notes.txt"), &replacement).unwrap();
    let s = Controlled::start_full(
        false,
        false,
        Some(drop_dir.directory()),
        &[source.0.join("fresh.bin"), source.0.join("notes.txt")],
    );
    let published = eventually(Duration::from_secs(60), || {
        drop_dir.entries() == ["fresh.bin", "notes.txt"]
            && fs::read(drop_dir.0.join("fresh.bin")).is_ok_and(|b| b == fresh)
    });
    assert!(
        published,
        "drop directory {:?}; host: {}",
        drop_dir.entries(),
        s.daemon.dump()
    );
    // The second file is staged, verified, then refused at the no-replace
    // rename; give its refusal time to reach the client.
    thread::sleep(Duration::from_secs(3));
    assert_eq!(fs::read(drop_dir.0.join("notes.txt")).unwrap(), original);
    close_window(&s.viewer.display, s.window.0);
    let output = wait_for(s.client, Duration::from_secs(30));
    let report = completion(&output, &s.daemon.dump());
    println!("fr completion: {report}");
    assert_eq!(report["outcome"], "stopped", "{report}");
    assert_eq!(report["files_requested"], 2, "{report}");
    assert_eq!(
        report["files_sent"],
        serde_json::json!([{"index": 0, "bytes": fresh.len(), "durable": true}]),
        "{report}"
    );
    assert_eq!(
        report["files_refused"],
        serde_json::json!([{"index": 1, "bytes": replacement.len(), "reason": "host_conflict"}]),
        "{report}"
    );
    assert_eq!(report["files_absence"], serde_json::Value::Null, "{report}");
    // Exactly the two names, the original bytes intact, no staging residue.
    assert_eq!(drop_dir.entries(), ["fresh.bin", "notes.txt"]);
    assert_eq!(fs::read(drop_dir.0.join("notes.txt")).unwrap(), original);
    println!(
        "files e2e: conflict kept the original ({} bytes, sha256 {})",
        original.len(),
        sha256(&original)
    );
    s.daemon.finish();
}

#[test]
#[ignore = "explicit isolated user/mount/network namespace; synthetic ingress; two Xvfb displays; real input agent and file lane"]
fn a_frozen_controller_leaves_no_partial_file_under_its_final_name() {
    let drop_dir = Dir::new("drop");
    let source = Dir::new("source");
    let name = "large-transfer.bin";
    let total = 32 * 1024 * 1024;
    fs::write(source.0.join(name), random(total)).unwrap();
    let mut s = Controlled::start_full(
        false,
        false,
        Some(drop_dir.directory()),
        &[source.0.join(name)],
    );
    // Mid-transfer: a private staging file holds some, not all, bytes.
    let until = Instant::now() + Duration::from_secs(90);
    let staged = loop {
        assert!(
            !drop_dir.0.join(name).exists(),
            "published before the freeze"
        );
        let partial = fs::read_dir(&drop_dir.0).unwrap().find_map(|entry| {
            let entry = entry.unwrap();
            let len = entry.metadata().unwrap().len();
            (entry.file_name().to_string_lossy().starts_with(".fr-part-")
                && len > 0
                && len < total as u64)
                .then_some(len)
        });
        if let Some(len) = partial {
            break len;
        }
        assert!(
            Instant::now() < until,
            "no staged transfer: {:?}; host: {}",
            drop_dir.entries(),
            s.daemon.dump()
        );
        thread::sleep(Duration::from_millis(10));
    };
    // Freeze the whole client past its 3 s lease.
    signal(&s.client, "-STOP");
    println!("files e2e: froze the client with {staged} of {total} bytes staged");
    let fenced = eventually(Duration::from_secs(20), || {
        assert!(
            !drop_dir.0.join(name).exists(),
            "a fenced transfer was published"
        );
        indicator(&mut s.observer).is_none() && drop_dir.entries().is_empty()
    });
    assert!(
        fenced,
        "lease/transfer survived a frozen client: {:?}; host: {}",
        drop_dir.entries(),
        s.daemon.dump()
    );
    // Resuming cannot resurrect the lease or the transfer.
    signal(&s.client, "-CONT");
    thread::sleep(Duration::from_secs(3));
    assert!(drop_dir.entries().is_empty(), "{:?}", drop_dir.entries());
    assert_eq!(indicator(&mut s.observer), None, "control was reacquired");
    if s.client.try_wait().unwrap().is_none() {
        signal(&s.client, "-INT");
    }
    let output = wait_for(s.client, Duration::from_secs(30));
    println!(
        "fr after lease loss: {:?} {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(drop_dir.entries().is_empty(), "{:?}", drop_dir.entries());
    s.daemon.finish();
}

#[test]
#[ignore = "explicit isolated user/mount/network namespace; synthetic ingress; two Xvfb displays; real input agent"]
fn a_host_without_files_keeps_control_and_the_client_reports_typed_absence() {
    // `frd run --input-agent` WITHOUT --files; the client asks to send one.
    let source = Dir::new("source");
    let bytes = random(4096);
    fs::write(source.0.join("report.txt"), &bytes).unwrap();
    let mut s = Controlled::start_full(false, false, None, &[source.0.join("report.txt")]);
    assert!(
        s.pointer_follows((150, 150), Duration::from_secs(10)),
        "control without files: {}",
        s.daemon.dump()
    );
    close_window(&s.viewer.display, s.window.0);
    let output = wait_for(s.client, Duration::from_secs(30));
    let report = completion(&output, &s.daemon.dump());
    println!("fr completion: {report}");
    assert_eq!(report["outcome"], "stopped", "{report}");
    assert_eq!(report["control_granted"], true, "{report}");
    assert_eq!(report["files_requested"], 1, "{report}");
    assert_eq!(report["files_sent"], serde_json::json!([]), "{report}");
    assert_eq!(
        report["files_refused"],
        serde_json::json!([{"index": 0, "bytes": bytes.len(), "reason": "not_sent"}]),
        "{report}"
    );
    assert_eq!(
        report["files_absence"], "host_files_unavailable",
        "{report}"
    );
    s.daemon.finish();
}
