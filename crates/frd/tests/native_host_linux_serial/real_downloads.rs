//! The shipped host and viewer CLIs transfer explicitly selected host files
//! into the viewer's private receive directory, under the existing control
//! lease. This reuses the upload fixture's real UDP/TLS/QUIC transport, Xvfb
//! displays, input executor and independent clipboard peers. Ingress evidence
//! and installed-tailnet metadata remain the namespace's synthetic fixtures.
//! No test here establishes live-tailnet or physical-desktop qualification.
use super::*;
use std::path::Path;

#[path = "real_downloads/progress.rs"]
mod progress;

fn assert_file(path: &Path, expected: &[u8]) {
    let actual = fs::read(path).unwrap();
    assert_eq!(actual.len(), expected.len());
    assert_eq!(sha256(&actual), sha256(expected));
}

fn assert_entries(path: &Path, expected: &[&str]) {
    let mut actual: Vec<_> = fs::read_dir(path)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    actual.sort();
    assert_eq!(actual, expected);
}

/// These are the actual CLI stdout/stderr, including the host's final events.
fn assert_private(output: &std::process::Output, host: &str, paths: &[&Path], names: &[&str]) {
    let diagnostics = format!(
        "{}\n{}\n{host}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    for path in paths {
        assert!(
            !diagnostics.contains(path.to_str().unwrap()),
            "a selected local path escaped into CLI diagnostics"
        );
    }
    for name in names {
        assert!(
            !diagnostics.contains(name),
            "a private selection name escaped into CLI diagnostics"
        );
    }
}

fn assert_download_report(report: &serde_json::Value) {
    assert_eq!(report["outcome"], "stopped", "{report}");
    assert_eq!(report["attempts"], 1, "{report}");
    assert_eq!(report["control_granted"], true, "{report}");
    assert_eq!(report["files_receive_requested"], true, "{report}");
}

const FILE: &str = "01-données-été λ 📄.bin";
const EMPTY: &str = "02-empty.txt";
const TREE: &str = "03-private-tree";
const EMPTY_TREE: &str = "04-empty-tree";
const EMPTY_BRANCHES: &str = "05-empty-branches";

fn assert_tree(root: &Path, files: &[(&str, Vec<u8>)]) {
    assert_entries(root, &["empty-branch", "notes.txt", "sub", "vacant"]);
    assert_entries(&root.join("sub"), &["empty", "nested.bin", "zero.bin"]);
    assert_entries(&root.join("empty-branch"), &["leaf"]);
    for empty in ["empty-branch/leaf", "sub/empty", "vacant"] {
        assert!(fs::symlink_metadata(root.join(empty)).unwrap().is_dir());
        assert_entries(&root.join(empty), &[]);
    }
    for (path, bytes) in files {
        assert_file(&root.join(path), bytes);
    }
}

fn assert_empty_branches(root: &Path) {
    assert_entries(root, &["one", "other"]);
    assert_entries(&root.join("one"), &["two"]);
    assert_entries(&root.join("one/two"), &["three"]);
    for empty in ["one/two/three", "other"] {
        assert!(fs::symlink_metadata(root.join(empty)).unwrap().is_dir());
        assert_entries(&root.join(empty), &[]);
    }
}

#[test]
#[ignore = "explicit isolated user/mount/network namespace; synthetic ingress; two Xvfb displays; real frd/fr CLIs, input agent and file lane"]
fn host_selections_download_atomically_while_control_and_clipboard_stay_live() {
    let source = Dir::new("download-source");
    let destination = Dir::new("download-destination");
    let bytes = random(3 * 1024 * 1024);
    let tree_files = [
        ("notes.txt", b"private host download payload".to_vec()),
        ("sub/nested.bin", random(64 * 1024 + 7)),
        ("sub/zero.bin", Vec::new()),
    ];
    fs::write(source.0.join(FILE), &bytes).unwrap();
    fs::write(source.0.join(EMPTY), []).unwrap();
    for (path, body) in &tree_files {
        let file = source.0.join(TREE).join(path);
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(file, body).unwrap();
    }
    for empty in ["empty-branch/leaf", "sub/empty", "vacant"] {
        fs::create_dir_all(source.0.join(TREE).join(empty)).unwrap();
    }
    fs::create_dir(source.0.join(EMPTY_TREE)).unwrap();
    for empty in ["one/two/three", "other"] {
        fs::create_dir_all(source.0.join(EMPTY_BRANCHES).join(empty)).unwrap();
    }
    let names = [FILE, EMPTY, TREE, EMPTY_TREE, EMPTY_BRANCHES];
    let selected: Vec<_> = names.iter().map(|name| source.0.join(name)).collect();
    let mut s = Controlled::start_downloads(true, true, &selected, Some(&destination.0));
    let mut host_app = Peer::start(&s.host.display);
    let mut viewer_app = Peer::start(&s.viewer.display);
    let published = eventually(Duration::from_secs(90), || {
        // Any final name we can observe must already contain the whole object.
        if destination.0.join(FILE).exists() {
            assert_file(&destination.0.join(FILE), &bytes);
        }
        if destination.0.join(EMPTY).exists() {
            assert_file(&destination.0.join(EMPTY), &[]);
        }
        if destination.0.join(TREE).exists() {
            assert_tree(&destination.0.join(TREE), &tree_files);
        }
        if destination.0.join(EMPTY_TREE).exists() {
            assert_entries(&destination.0.join(EMPTY_TREE), &[]);
        }
        if destination.0.join(EMPTY_BRANCHES).exists() {
            assert_empty_branches(&destination.0.join(EMPTY_BRANCHES));
        }
        destination.entries() == names
    });
    assert!(
        published,
        "downloads did not publish: {:?}; {}; host: {}",
        destination.entries(),
        client_state(&mut s.client),
        s.daemon.dump()
    );
    assert!(s.pointer_follows((222, 111), Duration::from_secs(10)));
    let to_host = unique("download-viewer-copy");
    viewer_app.copy(&to_host);
    if let Err(last) = pasted(&mut host_app, &to_host, Duration::from_secs(15)) {
        panic!(
            "host never pasted the viewer's copy (last {last:?}); host: {}",
            s.daemon.dump()
        );
    }
    let to_viewer = unique("download-host-copy");
    host_app.copy(&to_viewer);
    if let Err(last) = pasted(&mut viewer_app, &to_viewer, Duration::from_secs(15)) {
        panic!(
            "viewer never pasted the host's copy (last {last:?}); host: {}",
            s.daemon.dump()
        );
    }
    assert!(s.pointer_follows((180, 180), Duration::from_secs(10)));
    s.close_viewer();
    let output = wait_for(s.client, Duration::from_secs(30));
    let host = s.daemon.finish_once();
    let report = completion(&output, &host);
    assert!(output.status.success(), "{report}; {host}");
    assert_download_report(&report);
    let tree_bytes: usize = tree_files.iter().map(|(_, bytes)| bytes.len()).sum();
    assert_eq!(
        report["files_received"],
        serde_json::json!([
            {"index": 0, "bytes": bytes.len(), "durable": true},
            {"index": 1, "bytes": 0, "durable": true},
            {"index": 2, "bytes": tree_bytes, "durable": true},
            {"index": 3, "bytes": 0, "durable": true},
            {"index": 4, "bytes": 0, "durable": true}
        ]),
        "{report}"
    );
    assert_eq!(
        report["files_receive_refused"],
        serde_json::json!([]),
        "{report}"
    );
    assert!(report["files_receive_absence"].is_null(), "{report}");
    assert_eq!(report["files_receive_cleanup"], "finished", "{report}");
    assert_eq!(report["clipboard_active"], true, "{report}");
    assert_eq!(destination.entries(), names);
    assert_file(&destination.0.join(FILE), &bytes);
    assert_file(&destination.0.join(EMPTY), &[]);
    assert_tree(&destination.0.join(TREE), &tree_files);
    assert_entries(&destination.0.join(EMPTY_TREE), &[]);
    assert_empty_branches(&destination.0.join(EMPTY_BRANCHES));
    assert_private(
        &output,
        &host,
        &[&source.0, &destination.0],
        &[
            FILE,
            EMPTY,
            TREE,
            EMPTY_TREE,
            EMPTY_BRANCHES,
            &to_host,
            &to_viewer,
        ],
    );
}

#[test]
#[ignore = "explicit isolated user/mount/network namespace; synthetic ingress; two Xvfb displays; real frd/fr CLIs, input agent and file lane"]
fn a_download_conflict_preserves_existing_content_and_the_live_controller() {
    let source = Dir::new("download-conflict-source");
    let destination = Dir::new("download-conflict-destination");
    let fresh = random(32 * 1024);
    let original = random(4096);
    let replacement = random(64 * 1024);
    let names = [
        "first-private.bin",
        "private-notes.txt",
        "never-offered.bin",
    ];
    fs::write(source.0.join(names[0]), &fresh).unwrap();
    fs::write(source.0.join(names[1]), &replacement).unwrap();
    fs::write(source.0.join(names[2]), random(4096)).unwrap();
    fs::write(destination.0.join(names[1]), &original).unwrap();
    let selected: Vec<_> = names.iter().map(|name| source.0.join(name)).collect();
    let mut s = Controlled::start_downloads(false, false, &selected, Some(&destination.0));
    assert!(
        eventually(Duration::from_secs(60), || destination
            .0
            .join(names[0])
            .exists()),
        "first download did not publish: {}; host: {}",
        client_state(&mut s.client),
        s.daemon.dump()
    );
    // As in the upload conflict fixture, let the bounded refusal cross the lane.
    thread::sleep(Duration::from_secs(3));
    assert_file(&destination.0.join(names[0]), &fresh);
    assert_file(&destination.0.join(names[1]), &original);
    assert!(s.pointer_follows((150, 150), Duration::from_secs(10)));
    s.close_viewer();
    let output = wait_for(s.client, Duration::from_secs(30));
    let host = s.daemon.finish_once();
    let report = completion(&output, &host);
    assert_download_report(&report);
    assert_eq!(
        report["files_received"],
        serde_json::json!([{"index": 0, "bytes": fresh.len(), "durable": true}]),
        "{report}"
    );
    assert_eq!(
        report["files_receive_refused"],
        serde_json::json!([{"index": 1, "reason": "receive_conflict"}]),
        "{report}"
    );
    assert!(report["files_receive_absence"].is_null(), "{report}");
    assert_eq!(report["files_receive_cleanup"], "finished", "{report}");
    assert_eq!(destination.entries(), [names[0], names[1]]);
    assert_file(&destination.0.join(names[0]), &fresh);
    assert_file(&destination.0.join(names[1]), &original);
    assert!(!destination.0.join(names[2]).exists());
    assert_private(&output, &host, &[&source.0, &destination.0], &names);
}

#[test]
#[ignore = "explicit isolated user/mount/network namespace; synthetic ingress; two Xvfb displays; real frd/fr CLIs, input agent and file lane"]
fn local_keep_both_download_policy_preserves_files_and_whole_trees() {
    let source = Dir::new("download-keep-both-source");
    let destination = Dir::new("download-keep-both-destination");
    let original = b"local original remains";
    let incoming = random(32 * 1024);
    let names = [
        "private-document.txt",
        "private-project",
        "last-selection.bin",
    ];
    fs::write(source.0.join(names[0]), &incoming).unwrap();
    fs::create_dir(source.0.join(names[1])).unwrap();
    fs::write(source.0.join(names[1]).join("remote-only"), &incoming).unwrap();
    fs::create_dir(source.0.join(names[1]).join("empty-folder")).unwrap();
    fs::write(source.0.join(names[2]), []).unwrap();
    fs::write(destination.0.join(names[0]), original).unwrap();
    fs::create_dir(destination.0.join(names[1])).unwrap();
    fs::write(destination.0.join(names[1]).join("local-only"), original).unwrap();
    let selected: Vec<_> = names.iter().map(|name| source.0.join(name)).collect();
    let mut s = Controlled::start_downloads_with_options(
        false,
        false,
        &selected,
        Some(&destination.0),
        &["--receive-conflict", "keep-both"],
    );
    assert!(
        eventually(Duration::from_secs(60), || destination
            .0
            .join(names[2])
            .exists()),
        "keep-both batch did not finish: {}; host: {}",
        client_state(&mut s.client),
        s.daemon.dump()
    );
    assert_file(&destination.0.join(names[0]), original);
    assert_entries(&destination.0.join(names[1]), &["local-only"]);
    assert_file(&destination.0.join(names[1]).join("local-only"), original);
    let entries = destination.entries();
    assert_eq!(entries.len(), 5);
    let conflicts: Vec<_> = entries
        .iter()
        .filter(|name| name.contains(".fr-conflict-"))
        .collect();
    assert_eq!(conflicts.len(), 2);
    for name in conflicts {
        let path = destination.0.join(name);
        if path.is_dir() {
            assert_entries(&path, &["empty-folder", "remote-only"]);
            assert_file(&path.join("remote-only"), &incoming);
            assert_entries(&path.join("empty-folder"), &[]);
        } else {
            assert_file(&path, &incoming);
        }
    }
    assert!(s.pointer_follows((170, 170), Duration::from_secs(10)));
    s.close_viewer();
    let output = wait_for(s.client, Duration::from_secs(30));
    let host = s.daemon.finish_once();
    let report = completion(&output, &host);
    assert_download_report(&report);
    assert_eq!(
        report["files_received"],
        serde_json::json!([
            {"index": 0, "bytes": incoming.len(), "durable": true},
            {"index": 1, "bytes": incoming.len(), "durable": true},
            {"index": 2, "bytes": 0, "durable": true},
        ]),
        "{report}"
    );
    assert_eq!(
        report["files_receive_refused"],
        serde_json::json!([]),
        "{report}"
    );
    assert_eq!(report["files_receive_cleanup"], "finished", "{report}");
    assert_private(&output, &host, &[&source.0, &destination.0], &names);
    assert_private(
        &output,
        &host,
        &[],
        &entries.iter().map(String::as_str).collect::<Vec<_>>(),
    );
}

#[test]
#[ignore = "explicit isolated user/mount/network namespace; synthetic ingress; two Xvfb displays; real frd/fr CLIs, input agent and file lane"]
fn local_download_limits_bound_each_object_and_cumulative_session_admission() {
    // The same shipped options must reach both the pre-staging file bound and
    // the non-refundable aggregate session bound. A later refusal keeps the
    // first publication and the controller alive, and never starts item three.
    for (second_bytes, file_limit, session_limit) in [(7, "6", "20"), (4, "4", "7")] {
        let source = Dir::new("download-limits-source");
        let destination = Dir::new("download-limits-destination");
        let names = ["first-private.bin", "over-budget.bin", "never-started.bin"];
        fs::write(source.0.join(names[0]), b"firs").unwrap();
        fs::write(source.0.join(names[1]), random(second_bytes)).unwrap();
        fs::write(source.0.join(names[2]), []).unwrap();
        let selected: Vec<_> = names.iter().map(|name| source.0.join(name)).collect();
        let mut s = Controlled::start_downloads_with_options(
            false,
            false,
            &selected,
            Some(&destination.0),
            &[
                "--receive-max-file-bytes",
                file_limit,
                "--receive-max-session-bytes",
                session_limit,
            ],
        );
        assert!(
            eventually(Duration::from_secs(60), || destination
                .0
                .join(names[0])
                .exists()),
            "first bounded download missing: {}; host: {}",
            client_state(&mut s.client),
            s.daemon.dump()
        );
        thread::sleep(Duration::from_secs(3));
        assert!(s.pointer_follows((160, 160), Duration::from_secs(10)));
        s.close_viewer();
        let output = wait_for(s.client, Duration::from_secs(30));
        let host = s.daemon.finish_once();
        let report = completion(&output, &host);
        assert_download_report(&report);
        assert_eq!(
            report["files_received"],
            serde_json::json!([{"index": 0, "bytes": 4, "durable": true}]),
            "{report}"
        );
        assert_eq!(
            report["files_receive_refused"],
            serde_json::json!([{"index": 1, "reason": "receive_limit"}]),
            "{report}"
        );
        assert_eq!(report["files_receive_cleanup"], "finished", "{report}");
        assert_entries(&destination.0, &[names[0]]);
        assert_file(&destination.0.join(names[0]), b"firs");
        assert_private(&output, &host, &[&source.0, &destination.0], &names);
    }
}

#[test]
#[ignore = "explicit isolated user/mount/network namespace; synthetic ingress; two Xvfb displays; real frd/fr CLIs and input agent"]
fn no_host_selection_reports_download_absence_and_preserves_control() {
    let destination = Dir::new("download-unavailable");
    let mut s = Controlled::start_downloads(false, false, &[], Some(&destination.0));
    assert!(s.pointer_follows((150, 150), Duration::from_secs(10)));
    s.close_viewer();
    let output = wait_for(s.client, Duration::from_secs(30));
    let host = s.daemon.finish_once();
    let report = completion(&output, &host);
    assert_download_report(&report);
    assert_eq!(report["files_received"], serde_json::json!([]), "{report}");
    assert_eq!(
        report["files_receive_refused"],
        serde_json::json!([]),
        "{report}"
    );
    assert_eq!(
        report["files_receive_absence"], "host_download_unavailable",
        "{report}"
    );
    assert_eq!(report["files_receive_cleanup"], "not_started", "{report}");
    assert_eq!(destination.entries(), [] as [String; 0]);
    assert_private(&output, &host, &[&destination.0], &[]);
}

/// The viewer's real receive disk worker holds unnamed inodes in its own
/// process, unlike the upload fixture's in-process host worker.
fn staged_sizes(directory: &Dir, pid: u32) -> Vec<u64> {
    let prefix = format!("{}/#", fs::canonicalize(&directory.0).unwrap().display());
    fs::read_dir(format!("/proc/{pid}/fd"))
        .into_iter()
        .flatten()
        .filter_map(|fd| {
            let fd = fd.ok()?.path();
            let target = fs::read_link(&fd).ok()?;
            let target = target.to_str()?;
            if target.starts_with(&prefix) && target.ends_with(" (deleted)") {
                fs::metadata(&fd).ok().map(|metadata| metadata.len())
            } else {
                None
            }
        })
        .collect()
}

/// A failed assertion must not strand a frozen client in the namespace.
struct ResumeOnDrop(u32);
impl Drop for ResumeOnDrop {
    fn drop(&mut self) {
        let _ = std::process::Command::new("kill")
            .args(["-CONT", &self.0.to_string()])
            .status();
    }
}

#[test]
#[ignore = "explicit isolated user/mount/network namespace; synthetic ingress; two Xvfb displays; real frd/fr CLIs, input agent and file lane"]
fn an_expired_download_lease_cannot_publish_a_partially_received_file() {
    let source = Dir::new("download-expiry-source");
    let destination = Dir::new("download-expiry-destination");
    let name = "private-unfinished.bin";
    let total = 32 * 1024 * 1024;
    fs::write(source.0.join(name), random(total)).unwrap();
    let mut s =
        Controlled::start_downloads(false, false, &[source.0.join(name)], Some(&destination.0));
    let until = Instant::now() + Duration::from_secs(90);
    let staged = loop {
        assert_eq!(
            destination.entries(),
            [] as [String; 0],
            "a partial final name appeared"
        );
        if let Some(size) = staged_sizes(&destination, s.client.id())
            .into_iter()
            .find(|&size| size > 0 && size < total as u64)
        {
            break size;
        }
        assert!(
            Instant::now() < until,
            "no private receive in progress: {}; host: {}",
            client_state(&mut s.client),
            s.daemon.dump()
        );
        thread::sleep(Duration::from_millis(10));
    };
    signal(&s.client, "-STOP");
    let resume = ResumeOnDrop(s.client.id());
    println!("download e2e: froze the viewer with {staged} of {total} bytes privately staged");
    let fenced = eventually(Duration::from_secs(20), || {
        assert_eq!(
            destination.entries(),
            [] as [String; 0],
            "an expired download was published"
        );
        indicator(&mut s.observer).is_none()
    });
    assert!(
        fenced,
        "frozen viewer retained input authority: {}",
        s.daemon.dump()
    );
    // The host's real input lease is terminal. Resuming the same process must
    // neither reacquire control nor publish the already-admitted private inode.
    drop(resume);
    let cleaned = eventually(Duration::from_secs(15), || {
        assert_eq!(
            destination.entries(),
            [] as [String; 0],
            "resuming resurrected a download"
        );
        staged_sizes(&destination, s.client.id()).is_empty()
    });
    assert!(cleaned, "the cancelled receive retained its staging inode");
    assert_eq!(indicator(&mut s.observer), None);
    if s.client.try_wait().unwrap().is_none() {
        signal(&s.client, "-INT");
    }
    let output = wait_for(s.client, Duration::from_secs(30));
    let host = s.daemon.finish_once();
    let report = completion(&output, &host);
    assert_eq!(report["files_receive_requested"], true, "{report}");
    assert_eq!(report["files_received"], serde_json::json!([]), "{report}");
    assert_eq!(destination.entries(), [] as [String; 0]);
    assert_private(&output, &host, &[&source.0, &destination.0], &[name]);
}
