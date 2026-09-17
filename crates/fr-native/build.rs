use std::{env, path::PathBuf, process::Command};
fn main() {
    build_keyboard();
    build_clipboard();
    build_indicator();
    build_viewer_input();
    build_viewer_window();
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

fn build_keyboard() {
    println!("cargo:rerun-if-changed=src/keyboard.c");
    println!("cargo:rerun-if-env-changed=CC");
    if env::var_os("CARGO_FEATURE_LINUX_INPUT").is_none()
        || env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("linux")
    {
        return;
    }
    assert_eq!(
        env::var("HOST").unwrap(),
        env::var("TARGET").unwrap(),
        "XKB cross-builds require a qualified native sysroot"
    );
    let out = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    assert!(
        Command::new(env::var_os("CC").unwrap_or_else(|| "cc".into()))
            .args([
                "-std=c11",
                "-O2",
                "-fPIC",
                "-Wall",
                "-Wextra",
                "-Werror",
                "-c",
                "src/keyboard.c",
                "-o"
            ])
            .arg(out.join("keyboard.o"))
            .status()
            .expect("native C compiler")
            .success(),
        "install X11 development headers (libx11-dev)"
    );
    assert!(
        Command::new("ar")
            .arg("crs")
            .arg(out.join("libfrkeyboard.a"))
            .arg(out.join("keyboard.o"))
            .status()
            .unwrap()
            .success()
    );
    println!("cargo:rustc-link-search=native={}", out.display());
    println!("cargo:rustc-link-lib=static=frkeyboard");
}

fn build_clipboard() {
    println!("cargo:rerun-if-changed=src/clipboard_bridge.c");
    println!("cargo:rerun-if-env-changed=CC");
    if env::var_os("CARGO_FEATURE_LINUX_CLIPBOARD").is_none()
        || env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("linux")
    {
        return;
    }
    assert_eq!(
        env::var("HOST").unwrap(),
        env::var("TARGET").unwrap(),
        "clipboard cross-builds require a qualified native sysroot"
    );
    let out = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    assert!(
        Command::new(env::var_os("CC").unwrap_or_else(|| "cc".into()))
            .args([
                "-std=c11",
                "-O2",
                "-fPIC",
                "-Wall",
                "-Wextra",
                "-Werror",
                "-c",
                "src/clipboard_bridge.c",
                "-o"
            ])
            .arg(out.join("clipboard.o"))
            .status()
            .expect("native C compiler")
            .success(),
        "install XCB development headers (libxcb1-dev)"
    );
    assert!(
        Command::new("ar")
            .arg("crs")
            .arg(out.join("libfrclipboard.a"))
            .arg(out.join("clipboard.o"))
            .status()
            .unwrap()
            .success()
    );
    println!("cargo:rustc-link-search=native={}", out.display());
    println!("cargo:rustc-link-lib=static=frclipboard");
    println!("cargo:rustc-link-lib=xcb");
}

fn build_indicator() {
    println!("cargo:rerun-if-changed=src/sharing_indicator.c");
    if env::var_os("CARGO_FEATURE_LINUX_SESSION_UI").is_none()
        || env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("linux")
    {
        return;
    }
    assert_eq!(
        env::var("HOST").unwrap(),
        env::var("TARGET").unwrap(),
        "session UI cross-builds require a qualified native sysroot"
    );
    let out = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    assert!(
        Command::new(env::var_os("CC").unwrap_or_else(|| "cc".into()))
            .args([
                "-std=c11",
                "-O2",
                "-fPIC",
                "-Wall",
                "-Wextra",
                "-Werror",
                "-c",
                "src/sharing_indicator.c",
                "-o"
            ])
            .arg(out.join("indicator.o"))
            .status()
            .expect("native C compiler")
            .success(),
        "install XCB development headers (libxcb1-dev)"
    );
    assert!(
        Command::new("ar")
            .arg("crs")
            .arg(out.join("libfrindicator.a"))
            .arg(out.join("indicator.o"))
            .status()
            .unwrap()
            .success()
    );
    println!("cargo:rustc-link-search=native={}", out.display());
    println!("cargo:rustc-link-lib=static=frindicator");
    println!("cargo:rustc-link-lib=xcb");
}

fn build_viewer_input() {
    println!("cargo:rerun-if-changed=src/viewer_input.c");
    if env::var_os("CARGO_FEATURE_LINUX_VIEWER_INPUT").is_none()
        || env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("linux")
    {
        return;
    }
    assert_eq!(
        env::var("HOST").unwrap(),
        env::var("TARGET").unwrap(),
        "viewer input cross-builds require a qualified native sysroot"
    );
    let out = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    assert!(
        Command::new(env::var_os("CC").unwrap_or_else(|| "cc".into()))
            .args([
                "-std=c11",
                "-O2",
                "-fPIC",
                "-Wall",
                "-Wextra",
                "-Werror",
                "-c",
                "src/viewer_input.c",
                "-o"
            ])
            .arg(out.join("viewerinput.o"))
            .status()
            .expect("native C compiler")
            .success(),
        "install XCB and XKB protocol headers (libxcb1-dev, libx11-dev)"
    );
    assert!(
        Command::new("ar")
            .arg("crs")
            .arg(out.join("libfrviewerinput.a"))
            .arg(out.join("viewerinput.o"))
            .status()
            .unwrap()
            .success()
    );
    println!("cargo:rustc-link-search=native={}", out.display());
    println!("cargo:rustc-link-lib=static=frviewerinput");
    println!("cargo:rustc-link-lib=xcb");
}

fn build_viewer_window() {
    println!("cargo:rerun-if-changed=src/viewer_window.c");
    if env::var_os("CARGO_FEATURE_LINUX_VIEWER_WINDOW").is_none()
        || env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("linux")
    {
        return;
    }
    assert_eq!(
        env::var("HOST").unwrap(),
        env::var("TARGET").unwrap(),
        "viewer window cross-builds require a qualified native sysroot"
    );
    let out = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    assert!(
        Command::new(env::var_os("CC").unwrap_or_else(|| "cc".into()))
            .args([
                "-std=c11",
                "-O2",
                "-fPIC",
                "-Wall",
                "-Wextra",
                "-Werror",
                "-c",
                "src/viewer_window.c",
                "-o"
            ])
            .arg(out.join("viewerwindow.o"))
            .status()
            .expect("native C compiler")
            .success(),
        "install XCB development headers (libxcb1-dev)"
    );
    assert!(
        Command::new("ar")
            .arg("crs")
            .arg(out.join("libfrviewerwindow.a"))
            .arg(out.join("viewerwindow.o"))
            .status()
            .unwrap()
            .success()
    );
    println!("cargo:rustc-link-search=native={}", out.display());
    println!("cargo:rustc-link-lib=static=frviewerwindow");
    println!("cargo:rustc-link-lib=xcb");
}
