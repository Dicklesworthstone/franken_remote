use std::{env, path::PathBuf, process::Command};

fn main() {
    let source = "../fr-native/src/tailnet_macos.c";
    println!("cargo:rerun-if-changed={source}");
    println!("cargo:rerun-if-env-changed=CC");
    println!("cargo:rerun-if-env-changed=MACOSX_DEPLOYMENT_TARGET");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
        return;
    }
    assert_eq!(
        env::var("HOST").unwrap(),
        env::var("TARGET").unwrap(),
        "signed Tailscale bridge requires a native Apple SDK"
    );
    let out = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    // Match rustc's deployment target, rather than the build host's OS version.
    let target = env::var("MACOSX_DEPLOYMENT_TARGET").unwrap_or_else(|_| "11.0".into());
    assert!(
        Command::new(env::var_os("CC").unwrap_or_else(|| "cc".into()))
            .arg(format!("-mmacosx-version-min={target}"))
            .args([
                "-std=c11", "-O2", "-fPIC", "-Wall", "-Wextra", "-Werror", "-c", source, "-o"
            ])
            .arg(out.join("tailnet_macos.o"))
            .status()
            .expect("native Apple C compiler")
            .success(),
        "signed Tailscale bridge compilation failed"
    );
    assert!(
        Command::new("ar")
            .arg("crs")
            .arg(out.join("libfrtailnetmacos.a"))
            .arg(out.join("tailnet_macos.o"))
            .status()
            .expect("native archiver")
            .success()
    );
    println!("cargo:rustc-link-search=native={}", out.display());
    println!("cargo:rustc-link-lib=static=frtailnetmacos");
    println!("cargo:rustc-link-lib=framework=CoreFoundation");
    println!("cargo:rustc-link-lib=framework=Security");
}
