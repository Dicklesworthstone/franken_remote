//! `cfg(native_stack)`: frd's native session stack (installed tailnet, QUIC
//! transport, session runtime, workers) is compiled. That is Linux, or macOS
//! with the `macos-viewer` feature (fr-5a4w). One name keeps each gate short;
//! host-only code stays `target_os = "linux"`.
fn main() {
    println!("cargo::rustc-check-cfg=cfg(native_stack)");
    println!("cargo::rerun-if-changed=build.rs");
    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let viewer = std::env::var_os("CARGO_FEATURE_MACOS_VIEWER").is_some();
    if os == "linux" || (os == "macos" && viewer) {
        println!("cargo::rustc-cfg=native_stack");
    }
}
