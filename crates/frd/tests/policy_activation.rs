#![cfg(target_os = "linux")]

//! Actual CLI save/read receipts, not an acknowledgement from a running host.
use std::{fs, os::unix::fs::DirBuilderExt, process::Command};

#[test]
fn policy_save_and_read_report_unknown_running_activation() {
    let path = std::env::temp_dir().join(format!(
        "fr-policy-activation-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
    let file = path.join("policy.json");
    for (args, approval, sharing, revision, changed) in [
        (vec!["approval", "get"], "unattended", "own-user", 0, false),
        (
            vec!["approval", "set", "local"],
            "local",
            "own-user",
            1,
            true,
        ),
        (vec!["approval", "get"], "local", "own-user", 1, false),
        (
            vec!["approval", "set", "local"],
            "local",
            "own-user",
            1,
            false,
        ),
        (
            vec!["sharing", "set", "tailnet"],
            "local",
            "tailnet",
            2,
            true,
        ),
        (vec!["sharing", "get"], "local", "tailnet", 2, false),
    ] {
        let writing = args.get(1) == Some(&"set");
        let output = Command::new(env!("CARGO_BIN_EXE_frd"))
            .args(&args)
            .arg("--config")
            .arg(&file)
            .arg("--json")
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        let receipt: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(receipt["schema_version"], "fr.policy.v1");
        assert_eq!(receipt["applies_to"], "matching_live_daemon_or_next_start");
        // Missing fields must not masquerade as explicit unknown values.
        for field in ["applied_to_running_daemon", "restart_required"] {
            assert_eq!(receipt.get(field), Some(&serde_json::Value::Null));
        }
        assert_eq!(receipt["live_application"], "unconfirmed");
        assert_eq!(receipt["explicit_startup_flags_override"], true);
        assert_eq!(receipt["approval_mode"], approval);
        assert_eq!(receipt["sharing_scope"], sharing);
        assert_eq!(receipt["revision"], revision);
        assert_eq!(receipt["updated"], changed);
        assert_eq!(receipt["outcome"], "success");
        assert_eq!(
            receipt.get("durable"),
            Some(&if writing {
                serde_json::Value::Bool(true)
            } else {
                serde_json::Value::Null
            })
        );
    }
    let output = Command::new(env!("CARGO_BIN_EXE_frd"))
        .args(["approval", "get", "--config"])
        .arg(&file)
        .output()
        .unwrap();
    assert!(output.status.success());
    let human = String::from_utf8(output.stdout).unwrap();
    assert!(human.contains("live application is unconfirmed"));
    assert!(!human.contains("No running session was changed"));
}
