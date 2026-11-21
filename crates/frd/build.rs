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
use std::io::{self, Read};
use std::path::Path;

const MAX_BROWSER_JS_BYTES: usize = 256 * 1024;
const MAX_BROWSER_WASM_BYTES: usize = 4 * 1024 * 1024;
const WASM_V1_HEADER: &[u8; 8] = b"\0asm\x01\0\0\0";

fn main() -> io::Result<()> {
    println!("cargo::rustc-check-cfg=cfg(native_stack)");
    println!("cargo::rustc-check-cfg=cfg(host_stack)");
    println!("cargo::rustc-check-cfg=cfg(browser_wasm_bundle)");
    println!("cargo::rerun-if-env-changed=FR_WEB_ASSET_DIR");
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
    embed_browser_bundle()
}

// The release recipe generates this pair with the pinned Rust/wasm-bindgen
// toolchain before building frd. A default build keeps the shell available but
// explicitly refuses package requests; build.rs never downloads or builds it.
fn embed_browser_bundle() -> io::Result<()> {
    let Some(asset_dir) = std::env::var_os("FR_WEB_ASSET_DIR") else {
        return Ok(());
    };
    if asset_dir.is_empty() {
        return Err(invalid_bundle(
            "FR_WEB_ASSET_DIR must name a bundle directory",
        ));
    }
    let asset_dir = Path::new(&asset_dir);
    let javascript_path = asset_dir.join("fr_web.js");
    let wasm_path = asset_dir.join("fr_web_bg.wasm");
    println!("cargo::rerun-if-changed={}", javascript_path.display());
    println!("cargo::rerun-if-changed={}", wasm_path.display());

    // Read and validate both inputs before publishing either output or the cfg.
    let javascript = read_asset(&javascript_path, MAX_BROWSER_JS_BYTES)?;
    let wasm = read_asset(&wasm_path, MAX_BROWSER_WASM_BYTES)?;
    validate_bundle(&javascript, &wasm)?;
    let out_dir = std::env::var_os("OUT_DIR")
        .ok_or_else(|| invalid_bundle("OUT_DIR is required to embed the browser bundle"))?;
    let out_dir = Path::new(&out_dir);
    std::fs::write(out_dir.join("fr_web.js"), javascript)?;
    std::fs::write(out_dir.join("fr_web_bg.wasm"), wasm)?;
    println!("cargo::rustc-cfg=browser_wasm_bundle");
    Ok(())
}

fn invalid_bundle(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

fn read_asset(path: &Path, maximum: usize) -> io::Result<Vec<u8>> {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let file = std::fs::File::open(path).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("FR_WEB_ASSET_DIR requires the complete fr_web.js / fr_web_bg.wasm pair: {name}: {error}"),
        )
    })?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() == 0 || metadata.len() > maximum as u64 {
        return Err(invalid_bundle(format!(
            "{name} must be a nonempty regular file no larger than {maximum} bytes"
        )));
    }
    // Also bound the read itself: an input can grow after its metadata is read.
    read_bounded_asset(file, maximum, &name)
}

fn read_bounded_asset(reader: impl Read, maximum: usize, name: &str) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reader.take(maximum as u64 + 1).read_to_end(&mut bytes)?;
    if bytes.is_empty() || bytes.len() > maximum {
        return Err(invalid_bundle(format!(
            "{name} must contain between 1 and {maximum} bytes"
        )));
    }
    Ok(bytes)
}

fn validate_bundle(javascript: &[u8], wasm: &[u8]) -> io::Result<()> {
    if javascript.is_empty() || javascript.len() > MAX_BROWSER_JS_BYTES {
        return Err(invalid_bundle(
            "fr_web.js must contain between 1 and 262144 bytes",
        ));
    }
    if std::str::from_utf8(javascript).is_err() {
        return Err(invalid_bundle("fr_web.js must be UTF-8 JavaScript"));
    }
    if wasm.len() > MAX_BROWSER_WASM_BYTES || !wasm.starts_with(WASM_V1_HEADER) {
        return Err(invalid_bundle(
            "fr_web_bg.wasm must be a version-1 WebAssembly binary no larger than 4194304 bytes",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_utf8_javascript_and_version_one_wasm() {
        validate_bundle(b"export const fixture = 1;", WASM_V1_HEADER).unwrap();
    }

    #[test]
    fn rejects_empty_or_non_utf8_javascript() {
        for javascript in [&[][..], &[0xff][..]] {
            assert_eq!(
                validate_bundle(javascript, WASM_V1_HEADER)
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::InvalidData
            );
        }
    }

    #[test]
    fn rejects_truncated_wrong_magic_and_wrong_version_wasm() {
        for wasm in [
            &[][..],
            &WASM_V1_HEADER[..7],
            &b"not wasm"[..],
            &b"\0asm\x02\0\0\0"[..],
        ] {
            assert_eq!(
                validate_bundle(b"export {};", wasm).unwrap_err().kind(),
                io::ErrorKind::InvalidData
            );
        }
    }

    #[test]
    fn byte_limits_include_the_exact_boundary() {
        let javascript = vec![b' '; MAX_BROWSER_JS_BYTES];
        let mut wasm = vec![0; MAX_BROWSER_WASM_BYTES];
        wasm[..WASM_V1_HEADER.len()].copy_from_slice(WASM_V1_HEADER);
        validate_bundle(&javascript, &wasm).unwrap();
    }

    #[test]
    fn rejects_either_asset_one_byte_over_its_limit() {
        let javascript = vec![b' '; MAX_BROWSER_JS_BYTES + 1];
        assert!(validate_bundle(&javascript, WASM_V1_HEADER).is_err());
        let mut wasm = vec![0; MAX_BROWSER_WASM_BYTES + 1];
        wasm[..WASM_V1_HEADER.len()].copy_from_slice(WASM_V1_HEADER);
        assert!(validate_bundle(b"export {};", &wasm).is_err());
    }

    #[test]
    fn bounded_reader_rejects_empty_and_over_limit_inputs() {
        assert!(read_bounded_asset(&[][..], 8, "fixture").is_err());
        assert_eq!(
            read_bounded_asset(&b"12345678"[..], 8, "fixture").unwrap(),
            b"12345678"
        );
        let mut oversized = &b"123456789-unread-tail"[..];
        assert!(read_bounded_asset(&mut oversized, 8, "fixture").is_err());
        assert_eq!(oversized, b"-unread-tail");
    }
}
