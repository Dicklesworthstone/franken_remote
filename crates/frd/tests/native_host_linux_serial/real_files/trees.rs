//! Native uploads preserve directory structure without copying OS metadata.
//! Real host runtime, shipped client, ATP and disk effects; the surrounding
//! admission, Xvfb and input fixtures do not establish live-tailnet evidence.
use super::*;
use std::path::Path;

const NAMES: [&str; 3] = ["01-private-tree", "02-empty-branches", "03-empty-root"];

fn entries(path: &Path, expected: &[&str]) {
    let mut names: Vec<_> = fs::read_dir(path)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    assert_eq!(names, expected);
}

fn check_tree(root: &Path, bytes: &[u8]) {
    entries(root, &["payload.bin", "sub", "vacant"]);
    entries(&root.join("sub"), &["deep", "zero"]);
    entries(&root.join("sub/deep"), &["empty"]);
    for path in ["sub/deep/empty", "vacant"] {
        let metadata = fs::symlink_metadata(root.join(path)).unwrap();
        assert!(metadata.is_dir());
        assert_eq!(metadata.permissions().mode() & 0o777, 0o700);
        entries(&root.join(path), &[]);
    }
    assert_eq!(
        sha256(&fs::read(root.join("payload.bin")).unwrap()),
        sha256(bytes)
    );
    assert_eq!(fs::read(root.join("sub/zero")).unwrap(), [] as [u8; 0]);
}

fn check_empty_branches(root: &Path) {
    entries(root, &["a", "other"]);
    entries(&root.join("a"), &["b"]);
    for path in ["a/b", "other"] {
        assert!(fs::symlink_metadata(root.join(path)).unwrap().is_dir());
        entries(&root.join(path), &[]);
    }
}

#[test]
#[ignore = "explicit isolated user/mount/network namespace; synthetic ingress; two Xvfb displays; real client, host runtime, input agent and file lane"]
fn native_upload_preserves_nested_empty_directories_and_the_original_controller() {
    let source = Dir::new("tree-source");
    let destination = Dir::new("tree-destination");
    let tree = source.0.join(NAMES[0]);
    let bytes = random(128 * 1024 + 7);
    for path in ["sub/deep/empty", "vacant"] {
        fs::create_dir_all(tree.join(path)).unwrap();
    }
    fs::write(tree.join("payload.bin"), &bytes).unwrap();
    fs::write(tree.join("sub/zero"), []).unwrap();
    for path in ["a/b", "other"] {
        fs::create_dir_all(source.0.join(NAMES[1]).join(path)).unwrap();
    }
    fs::create_dir(source.0.join(NAMES[2])).unwrap();
    let selected: Vec<_> = NAMES.iter().map(|name| source.0.join(name)).collect();
    let mut s = Controlled::start_full(false, false, Some(destination.directory()), &selected);
    let original_indicator = indicator(&mut s.observer).unwrap();
    assert!(
        eventually(Duration::from_secs(90), || {
            // Each observed final root must already hold its whole selection.
            if destination.0.join(NAMES[0]).exists() {
                check_tree(&destination.0.join(NAMES[0]), &bytes);
            }
            if destination.0.join(NAMES[1]).exists() {
                check_empty_branches(&destination.0.join(NAMES[1]));
            }
            if destination.0.join(NAMES[2]).exists() {
                entries(&destination.0.join(NAMES[2]), &[]);
            }
            destination.entries() == NAMES
        }),
        "tree upload did not publish: {}; host: {}",
        client_state(&mut s.client),
        s.daemon.dump()
    );
    assert!(s.pointer_follows((220, 180), Duration::from_secs(10)));
    assert_eq!(indicator(&mut s.observer), Some(original_indicator));
    // Give the original publication proofs time to cross before closing input.
    thread::sleep(Duration::from_secs(2));
    s.close_viewer();
    let output = wait_for(s.client, Duration::from_secs(30));
    let host = s.daemon.dump();
    let report = completion(&output, &host);
    assert!(output.status.success(), "{report}; {host}");
    assert_eq!(report["outcome"], "stopped", "{report}");
    assert_eq!(report["control_granted"], true, "{report}");
    assert_eq!(report["files_requested"], 3, "{report}");
    assert_eq!(report["files_refused"], serde_json::json!([]), "{report}");
    assert!(report["files_absence"].is_null(), "{report}");
    assert_eq!(
        report["files_sent"],
        serde_json::json!([
            {"index": 0, "bytes": bytes.len(), "durable": true},
            {"index": 1, "bytes": 0, "durable": true},
            {"index": 2, "bytes": 0, "durable": true}
        ]),
        "{report}"
    );
    let diagnostics = format!(
        "{}\n{}\n{host}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    for secret in NAMES
        .iter()
        .copied()
        .chain([source.0.to_str().unwrap(), destination.0.to_str().unwrap()])
    {
        assert!(
            !diagnostics.contains(secret),
            "a private selection escaped into CLI diagnostics"
        );
    }
    assert_eq!(destination.entries(), NAMES);
    check_tree(&destination.0.join(NAMES[0]), &bytes);
    check_empty_branches(&destination.0.join(NAMES[1]));
    entries(&destination.0.join(NAMES[2]), &[]);
    s.daemon.finish();
}
