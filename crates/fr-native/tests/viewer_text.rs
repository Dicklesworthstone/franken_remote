#![cfg(all(target_os = "linux", feature = "linux-viewer-input"))]
//! Actual local Unicode producer tests; remote authority remains independently
//! covered by the shared Source tests and full native session qualification.
use std::process::Command;

#[test]
fn original_view_text_composer_uses_real_xservers() {
    if Command::new("Xvfb").arg("-help").output().is_err() {
        assert!(
            std::env::var_os("FR_NATIVE_INPUT_CAPTURE_REQUIRED").is_none(),
            "required native text tests need Xvfb"
        );
        eprintln!("BLOCKED: Xvfb unavailable; native text capture is not qualified");
        return;
    }
    let output = Command::new("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/viewer_input/text.py"
        ))
        .output()
        .expect("run the independent native text tests");
    assert!(
        output.status.success(),
        "native text failure:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    print!("{}", String::from_utf8_lossy(&output.stdout));
}
