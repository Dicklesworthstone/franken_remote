//! Executable test fixtures that a parallel test process can exec safely.
//!
//! A test that writes an image itself holds a writable descriptor to it while
//! writing. Another test thread's fork in that window inherits the descriptor
//! until its own exec, and an exec of the image meanwhile fails with ETXTBSY:
//! production correctly refuses with `SpawnFailed(ExecutableFileBusy)`, and an
//! unrelated test fails (fr-rc-gates-cargo-test-6d8). A child process writes
//! the file instead, so this process never opens it for writing; the mode is
//! set by path afterwards. Included by the frd library's tests and by each
//! integration-test binary that writes worker images (frd's, and fr-native's
//! `viewer_window_x11`).
use std::{
    io::Write,
    os::unix::fs::PermissionsExt,
    path::Path,
    process::{Command, Stdio},
};

/// Create or replace `path` with `contents` through `sh -c 'cat > path'`, then
/// make it owner-only executable (0700, as every fixture image was).
pub(crate) fn write(path: &Path, contents: impl AsRef<[u8]>) {
    let mut child = Command::new("sh")
        .args(["-c", "cat > \"$1\"", "sh"])
        .arg(path)
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    // Dropping stdin at the end of this statement closes the pipe: EOF.
    child
        .stdin
        .take()
        .unwrap()
        .write_all(contents.as_ref())
        .unwrap();
    assert!(
        child.wait().unwrap().success(),
        "fixture image {}",
        path.display()
    );
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
}
