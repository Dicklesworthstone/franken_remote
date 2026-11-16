//! Exercise the exact shared C callback on portable builders. Native APIs are
//! explicit doubles; these tests establish output custody, not codec support.
#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::{env, fs, path::Path, process::Command, time::SystemTime};

#[test]
fn original_apple_picture_custody_survives_adversarial_callbacks() {
    let unique = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .expect("test clock")
        .as_nanos();
    let temporary = env::temp_dir().join(format!(
        "fr-apple-decode-output-{}-{unique}",
        std::process::id()
    ));
    fs::create_dir(&temporary).expect("create isolated C test output");
    let executable = temporary.join("apple-decode-output");
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/apple_decode_output.c");
    let compiled = Command::new("cc")
        .args(["-std=c11", "-Wall", "-Wextra", "-Werror", "-pthread"])
        .arg(&source)
        .arg("-o")
        .arg(&executable)
        .output()
        .expect("portable C compiler");
    assert!(
        compiled.status.success(),
        "C boundary regression did not compile:\n{}",
        String::from_utf8_lossy(&compiled.stderr)
    );
    let ran = Command::new(executable)
        .output()
        .expect("run C boundary regression");
    assert!(
        ran.status.success(),
        "C boundary regression failed:\n{}\n{}",
        String::from_utf8_lossy(&ran.stdout),
        String::from_utf8_lossy(&ran.stderr)
    );
}
