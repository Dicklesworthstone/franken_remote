//! Compiles the `VideoToolbox` decode glue (`src/vt_decode.c`) for iOS devices,
//! the iOS simulator and the macOS host (whose tests decode the same way).
//! System frameworks only; the SDK comes from `xcrun`, never a download.
#[path = "build_support/roots.rs"]
mod roots;

use std::{env, path::PathBuf, process::Command};

/// The app's deployment floor (mobile/ios project: iOS 17).
const IOS_MINIMUM: &str = "17.0";
/// macOS host tests: the oldest macOS this workspace's viewer builds for.
const MACOS_MINIMUM: &str = "14.0";

fn main() {
    println!("cargo::rerun-if-changed=build.rs");
    println!("cargo::rerun-if-changed=build_support/roots.rs");
    println!("cargo::rerun-if-env-changed={}", roots::LOCAL_ROOTS_ENV);
    println!("cargo::rerun-if-changed=src/vt_decode.c");
    println!("cargo::rerun-if-changed=../fr-native/src/apple_decode_output.h");
    println!("cargo::rerun-if-changed=src/abi_check.c");
    println!("cargo::rerun-if-changed=include/fr_mobile.h");
    println!("cargo::rerun-if-env-changed=IPHONEOS_DEPLOYMENT_TARGET");
    let os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let abi = env::var("CARGO_CFG_TARGET_ABI").unwrap_or_default();
    let arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    let (sdk, target) = match (os.as_str(), abi.as_str()) {
        ("ios", "sim") => (
            "iphonesimulator",
            format!("{}-apple-ios{IOS_MINIMUM}-simulator", clang_arch(&arch)),
        ),
        ("ios", _) => (
            "iphoneos",
            format!("{}-apple-ios{IOS_MINIMUM}", clang_arch(&arch)),
        ),
        ("macos", _) => (
            "macosx",
            format!("{}-apple-macos{MACOS_MINIMUM}", clang_arch(&arch)),
        ),
        // Windows: exactly the same pinned roots, either explicitly supplied
        // locally or exported from a build Mac. No VideoToolbox glue.
        ("windows", _) => {
            windows_roots(&PathBuf::from(env::var_os("OUT_DIR").unwrap()));
            return;
        }
        _ => return,
    };
    let out = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    export_roots(&out);
    // The hand-written header must describe the Rust ABI exactly: compile its
    // layout assertions (and every declaration) for this target.
    let status = Command::new("xcrun")
        .args(["--sdk", sdk, "clang", "-target", &target])
        .args(["-x", "c", "-std=c11", "-fsyntax-only", "-Iinclude"])
        .args(["-Wall", "-Wextra", "-Wnullability-completeness", "-Werror"])
        .arg("src/abi_check.c")
        .status()
        .expect("xcrun clang (install Xcode and its command line tools)");
    assert!(
        status.success(),
        "include/fr_mobile.h does not match the Rust ABI"
    );
    let object = out.join("vt_decode.o");
    // `xcrun --sdk` picks both the SDK and its clang. The Werror build with an
    // explicit target and deployment floor fails loudly on any deprecation.
    let status = Command::new("xcrun")
        .args(["--sdk", sdk, "clang", "-target", &target])
        .args([
            "-x", "c", "-std=c11", "-O2", "-fPIC", "-Wall", "-Wextra", "-Werror",
        ])
        .args(["-c", "src/vt_decode.c", "-o"])
        .arg(&object)
        .status()
        .expect("xcrun clang (install Xcode and its command line tools)");
    assert!(status.success(), "src/vt_decode.c failed to compile");
    let status = Command::new("xcrun")
        .args(["--sdk", sdk, "ar", "crs"])
        .arg(out.join("libfrvtdecode.a"))
        .arg(&object)
        .status()
        .expect("xcrun ar");
    assert!(status.success(), "archiving vt_decode.o failed");
    println!("cargo::rustc-link-search=native={}", out.display());
    println!("cargo::rustc-link-lib=static=frvtdecode");
    for framework in [
        "CoreFoundation",
        "CoreMedia",
        "CoreVideo",
        "IOSurface",
        "VideoToolbox",
    ] {
        println!("cargo::rustc-link-lib=framework={framework}");
    }
}

/// macOS's own trust anchors (part of the OS, not user-editable).
const SYSTEM_ROOTS: &str = "/System/Library/Keychains/SystemRootCertificates.keychain";

fn windows_roots(out: &std::path::Path) {
    if let Some(directory) = env::var_os(roots::LOCAL_ROOTS_ENV) {
        let directory = PathBuf::from(directory);
        for root in roots::ISRG_ROOTS {
            println!(
                "cargo::rerun-if-changed={}",
                directory.join(root.file).display()
            );
        }
        let validated = roots::local_roots(&directory)
            .unwrap_or_else(|error| panic!("{}: {error}", roots::LOCAL_ROOTS_ENV));
        for (root, pem) in roots::ISRG_ROOTS.into_iter().zip(validated) {
            std::fs::write(out.join(root.file), pem).expect("write OUT_DIR pinned root");
        }
    } else if cfg!(target_os = "macos") {
        export_roots(out);
    } else {
        panic!(
            "Windows builds off macOS require {}: an absolute local directory containing isrg-root-x1.pem and isrg-root-x2.pem with the existing pinned DER fingerprints",
            roots::LOCAL_ROOTS_ENV,
        );
    }
}

/// Copy the two ISRG roots out of the build Mac's system trust store into
/// `OUT_DIR` for `src/roots.rs` to embed. The repository keeps no
/// certificate files (.gitignore); a store without exactly one certificate
/// with each pinned fingerprint fails the build.
fn export_roots(out: &std::path::Path) {
    println!("cargo::rerun-if-changed={SYSTEM_ROOTS}");
    for root in roots::ISRG_ROOTS {
        let roots::Root {
            name,
            fingerprint,
            file,
        } = root;
        let output = Command::new("/usr/bin/security")
            .args([
                "find-certificate",
                "-a",
                "-Z",
                "-p",
                "-c",
                name,
                SYSTEM_ROOTS,
            ])
            .output()
            .expect("/usr/bin/security (macOS)");
        assert!(output.status.success(), "{name} is not in {SYSTEM_ROOTS}");
        let text = String::from_utf8(output.stdout).expect("security prints ASCII");
        // Each certificate prints as "SHA-256 hash: <hex>", "SHA-1 hash:
        // <hex>", then its PEM block.
        let mut matching = Vec::new();
        let mut current: Option<&str> = None;
        let mut lines = text.lines();
        while let Some(line) = lines.next() {
            if let Some(hash) = line.strip_prefix("SHA-256 hash: ") {
                current = Some(hash.trim());
            } else if line == "-----BEGIN CERTIFICATE-----" {
                let mut pem = String::from(line);
                pem.push('\n');
                for body in lines.by_ref() {
                    pem.push_str(body);
                    pem.push('\n');
                    if body == "-----END CERTIFICATE-----" {
                        break;
                    }
                }
                if current.take() == Some(fingerprint) {
                    matching.push(pem);
                }
            }
        }
        assert!(
            matching.len() == 1,
            "{SYSTEM_ROOTS} has {} certificates named {name} with SHA-256 {fingerprint}",
            matching.len()
        );
        let pem = roots::validate_pem(matching[0].as_bytes(), root)
            .unwrap_or_else(|error| panic!("system {name}: {error}"));
        std::fs::write(out.join(file), pem).expect("write OUT_DIR root");
    }
}

fn clang_arch(arch: &str) -> &str {
    match arch {
        "aarch64" => "arm64",
        other => other,
    }
}
