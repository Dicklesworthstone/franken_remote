use std::{env, path::PathBuf, process::Command};
fn main() {
    println!("cargo:rerun-if-changed=src/bridge.c");
    if env::var_os("CARGO_FEATURE_LINUX_MEDIA").is_none()
        || env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("linux")
    {
        return;
    }
    assert_eq!(
        env::var("HOST").unwrap(),
        env::var("TARGET").unwrap(),
        "native media cross-build requires an explicit qualified sysroot"
    );
    let packages = ["libavcodec", "libavutil", "libswscale", "x11"];
    println!("cargo:rerun-if-env-changed=FR_NATIVE_INCLUDE_DIR");
    println!("cargo:rerun-if-env-changed=FR_NATIVE_LIBRARY_DIR");
    let include = env::var_os("FR_NATIVE_INCLUDE_DIR");
    let library = env::var_os("FR_NATIVE_LIBRARY_DIR");
    assert_eq!(
        include.is_some(),
        library.is_some(),
        "supply both explicit SDK directories"
    );
    let cflags = if let Some(path) = &include {
        vec![format!("-I{}", PathBuf::from(path).display())]
    } else {
        let flags = Command::new("pkg-config")
            .arg("--cflags")
            .args(packages)
            .output()
            .expect("pkg-config is required");
        assert!(
            flags.status.success(),
            "install the FFmpeg and X11 development packages: {}",
            String::from_utf8_lossy(&flags.stderr)
        );
        String::from_utf8(flags.stdout)
            .unwrap()
            .split_whitespace()
            .map(str::to_owned)
            .collect()
    };
    let out = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    let mut cc = Command::new(env::var_os("CC").unwrap_or_else(|| "cc".into()));
    cc.args([
        "-std=c11",
        "-O2",
        "-fPIC",
        "-Wall",
        "-Wextra",
        "-Werror=implicit-function-declaration",
    ]);
    cc.args(cflags);
    assert!(
        cc.arg("-c")
            .arg("src/bridge.c")
            .arg("-o")
            .arg(out.join("bridge.o"))
            .status()
            .expect("C compiler")
            .success(),
        "native bridge compilation failed"
    );
    assert!(
        Command::new("ar")
            .arg("crs")
            .arg(out.join("libfrnative.a"))
            .arg(out.join("bridge.o"))
            .status()
            .unwrap()
            .success()
    );
    println!("cargo:rustc-link-search=native={}", out.display());
    println!("cargo:rustc-link-lib=static=frnative");
    let libs = if let Some(path) = library {
        format!(
            "-L{} -lavcodec -lavutil -lswscale -lX11",
            PathBuf::from(path).display()
        )
    } else {
        let output = Command::new("pkg-config")
            .arg("--libs")
            .args(packages)
            .output()
            .unwrap();
        assert!(output.status.success());
        String::from_utf8(output.stdout).unwrap()
    };
    for flag in libs.split_whitespace() {
        if let Some(lib) = flag.strip_prefix("-l") {
            println!("cargo:rustc-link-lib={lib}");
        } else if let Some(path) = flag.strip_prefix("-L") {
            println!("cargo:rustc-link-search=native={path}");
        }
    }
}
