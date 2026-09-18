#![cfg(all(target_os = "linux", feature = "linux-viewer-input"))]
//! Real cold-server regressions for the shipping XCB keyboard boundary. This
//! does not substitute for the Rust source/authority or remote-session tests.
use std::process::Command;

#[test]
fn cold_keyboard_and_map_verification_use_real_xservers() {
    if Command::new("Xvfb").arg("-help").output().is_err() {
        assert!(
            std::env::var_os("FR_NATIVE_INPUT_CAPTURE_REQUIRED").is_none(),
            "required native keyboard-map tests need Xvfb"
        );
        eprintln!("BLOCKED: Xvfb unavailable; native keyboard-map capture is not qualified");
        return;
    }
    let output = Command::new("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/viewer_input/keyboard_map.py"
        ))
        .output()
        .expect("run the independent native keyboard-map tests");
    assert!(
        output.status.success(),
        "native keyboard-map failure:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    print!("{}", String::from_utf8_lossy(&output.stdout));
}
