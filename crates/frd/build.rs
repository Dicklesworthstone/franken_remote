//! `cfg(native_stack)`: frd's native session stack (installed tailnet, QUIC
//! transport, session runtime, workers) is compiled. That is Linux, or macOS
//! with the `macos-viewer` feature (fr-5a4w). One name keeps each gate short;
//! host-only code stays `target_os = "linux"`.
//!
//! iOS always compiles the viewer stack (`native_stack`, for fr-mobile-ffi)
//! and never the host half.
//!
//! `cfg(host_stack)`: the host half (`frd run`, its policy and lifecycle) is
//! compiled. That is Linux, or macOS with `macos-host` (fr-p1-host-macos-vif).
fn main() {
    println!("cargo::rustc-check-cfg=cfg(native_stack)");
    println!("cargo::rustc-check-cfg=cfg(host_stack)");
    println!("cargo::rerun-if-changed=build.rs");
    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let viewer = std::env::var_os("CARGO_FEATURE_MACOS_VIEWER").is_some();
    let host = std::env::var_os("CARGO_FEATURE_MACOS_HOST").is_some();
    let windows = std::env::var_os("CARGO_FEATURE_WINDOWS_VIEWER").is_some();
    if os == "linux"
        || os == "ios"
        || (os == "windows" && windows)
        || (os == "macos" && (viewer || host))
    {
        println!("cargo::rustc-cfg=native_stack");
    }
    let windows_host = std::env::var_os("CARGO_FEATURE_WINDOWS_HOST").is_some();
    if os == "linux" || (os == "macos" && host) || (os == "windows" && windows_host) {
        println!("cargo::rustc-cfg=host_stack");
    }
}
