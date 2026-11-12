use std::{env, path::PathBuf, process::Command};
fn main() {
    build_pipewire_microphone();
    build_opus_process();
    build_macos_opus();
    build_macos_audio();
    build_keyboard();
    build_clipboard();
    build_indicator();
    build_viewer_input();
    build_viewer_window();
    build_host_picker();
    build_macos_viewer();
    build_macos_clipboard();
    build_macos_capture();
    build_macos_input();
    build_macos_session_monitor();
    build_macos_observation_indicator();
    link_clang_runtime();
    println!("cargo:rerun-if-changed=src/bridge.c");
    println!("cargo:rerun-if-changed=src/x11_image.c");
    println!("cargo:rerun-if-changed=src/x11_image.h");
    println!("cargo:rerun-if-changed=src/decoder_sandbox.c");
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
    let packages = ["libavcodec", "libavutil", "libswscale", "x11", "xcb"];
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
    // Narrow public ABI for checked MIT-SHM on the original Xlib connection.
    println!("cargo:rustc-link-lib=dylib:+verbatim=libX11-xcb.so.1");
    link_system_libraries(&packages, library);
}

fn build_pipewire_microphone() {
    println!("cargo:rerun-if-changed=src/pipewire_mic.c");
    println!("cargo:rerun-if-changed=src/pipewire_mic.h");
    println!("cargo:rerun-if-env-changed=CC");
    if env::var_os("CARGO_FEATURE_LINUX_PIPEWIRE_MIC").is_none()
        || env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("linux")
    {
        return;
    }
    assert_eq!(
        env::var("HOST").unwrap(),
        env::var("TARGET").unwrap(),
        "PipeWire cross-build requires an explicit qualified sysroot"
    );
    let flags = Command::new("pkg-config")
        .args(["--cflags", "libpipewire-0.3"])
        .output()
        .expect("pkg-config is required for the PipeWire microphone");
    assert!(
        flags.status.success(),
        "install libpipewire-0.3-dev: {}",
        String::from_utf8_lossy(&flags.stderr)
    );
    let out = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    assert!(
        Command::new(env::var_os("CC").unwrap_or_else(|| "cc".into()))
            .args(["-std=c11", "-O2", "-fPIC", "-Wall", "-Wextra", "-Werror"])
            .args(String::from_utf8(flags.stdout).unwrap().split_whitespace())
            .args(["-c", "src/pipewire_mic.c", "-o"])
            .arg(out.join("pipewire_mic.o"))
            .status()
            .expect("native C compiler")
            .success(),
        "PipeWire microphone bridge compilation failed"
    );
    assert!(
        Command::new("ar")
            .arg("crs")
            .arg(out.join("libfrpipewiremic.a"))
            .arg(out.join("pipewire_mic.o"))
            .status()
            .unwrap()
            .success()
    );
    println!("cargo:rustc-link-search=native={}", out.display());
    println!("cargo:rustc-link-lib=static=frpipewiremic");
    link_system_libraries(&["libpipewire-0.3"], None);
}

/// Link FFmpeg/X11 either from the explicit SDK directory or from pkg-config.
fn link_system_libraries(packages: &[&str], library: Option<std::ffi::OsString>) {
    let libs = if let Some(path) = library {
        format!(
            "-L{} -lavcodec -lavutil -lswscale -lX11 -lxcb",
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
    println!("cargo:rerun-if-changed=src/input_gate.c");
    println!("cargo:rerun-if-changed=src/input_gate.h");
    // The input executor (linux-input) shows the remote-control indicator.
    if (env::var_os("CARGO_FEATURE_LINUX_SESSION_UI").is_none()
        && env::var_os("CARGO_FEATURE_LINUX_INPUT").is_none())
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
    for (source, object) in [
        ("src/sharing_indicator.c", "indicator.o"),
        ("src/input_gate.c", "input_gate.o"),
    ] {
        assert!(
            Command::new(env::var_os("CC").unwrap_or_else(|| "cc".into()))
                .args([
                    "-std=c11", "-O2", "-fPIC", "-Wall", "-Wextra", "-Werror", "-c", source, "-o"
                ])
                .arg(out.join(object))
                .status()
                .expect("native C compiler")
                .success(),
            "install XCB development headers (libxcb1-dev)"
        );
    }
    assert!(
        Command::new("ar")
            .arg("crs")
            .arg(out.join("libfrindicator.a"))
            .arg(out.join("indicator.o"))
            .arg(out.join("input_gate.o"))
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
    println!("cargo:rerun-if-changed=src/viewer_cursor.c");
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
    // The local pointer image owner (remote cursor while controlling): its
    // own connection, RENDER ARGB cursors only.
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
                "src/viewer_cursor.c",
                "-o"
            ])
            .arg(out.join("viewercursor.o"))
            .status()
            .expect("native C compiler")
            .success(),
        "install XCB RENDER development headers (libxcb-render0-dev)"
    );
    assert!(
        Command::new("ar")
            .arg("crs")
            .arg(out.join("libfrviewerinput.a"))
            .arg(out.join("viewerinput.o"))
            .arg(out.join("viewercursor.o"))
            .status()
            .unwrap()
            .success()
    );
    println!("cargo:rustc-link-search=native={}", out.display());
    println!("cargo:rustc-link-lib=static=frviewerinput");
    println!("cargo:rustc-link-lib=xcb-render");
    println!("cargo:rustc-link-lib=xcb");
}

fn build_viewer_window() {
    println!("cargo:rerun-if-changed=src/viewer_window.c");
    println!("cargo:rerun-if-changed=src/viewer_drop.c");
    println!("cargo:rerun-if-changed=src/viewer_switch.c");
    println!("cargo:rerun-if-changed=src/file_progress.c");
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

fn build_host_picker() {
    println!("cargo:rerun-if-changed=src/host_picker.c");
    if (env::var_os("CARGO_FEATURE_LINUX_MEDIA").is_none()
        && env::var_os("CARGO_FEATURE_LINUX_DESKTOP").is_none())
        || env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("linux")
    {
        return;
    }
    assert_eq!(
        env::var("HOST").unwrap(),
        env::var("TARGET").unwrap(),
        "host picker cross-builds require a qualified native sysroot"
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
                "src/host_picker.c",
                "-o"
            ])
            .arg(out.join("hostpicker.o"))
            .status()
            .expect("native C compiler")
            .success(),
        "install XCB development headers (libxcb1-dev)"
    );
    assert!(
        Command::new("ar")
            .arg("crs")
            .arg(out.join("libfrhostpicker.a"))
            .arg(out.join("hostpicker.o"))
            .status()
            .unwrap()
            .success()
    );
    println!("cargo:rustc-link-search=native={}", out.display());
    println!("cargo:rustc-link-lib=static=frhostpicker");
    println!("cargo:rustc-link-lib=xcb");
}

fn build_opus_process() {
    println!("cargo:rerun-if-changed=src/opus/process/sandbox.c");
    if env::var_os("CARGO_FEATURE_OPUS_PROCESS").is_none()
        || env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("linux")
    {
        return;
    }
    assert_eq!(
        env::var("HOST").unwrap(),
        env::var("TARGET").unwrap(),
        "Opus process cross-builds need a qualified native sysroot"
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
                "src/opus/process/sandbox.c",
                "-o"
            ])
            .arg(out.join("opus_sandbox.o"))
            .status()
            .unwrap()
            .success()
    );
    assert!(
        Command::new("ar")
            .arg("crs")
            .arg(out.join("libfropussandbox.a"))
            .arg(out.join("opus_sandbox.o"))
            .status()
            .unwrap()
            .success()
    );
    println!("cargo:rustc-link-search=native={}", out.display());
    println!("cargo:rustc-link-lib=static=fropussandbox");
}

/// The Mac decoder keeps its own native process and statically linked codec.
/// The caller supplies an audited SDK archive; this build downloads nothing and
/// creates no new runtime library-search path.
fn build_macos_opus() {
    println!("cargo:rerun-if-changed=src/opus/process/macos.c");
    println!("cargo:rerun-if-env-changed=FR_NATIVE_OPUS_LIBRARY_DIR");
    println!("cargo:rerun-if-env-changed=MACOSX_DEPLOYMENT_TARGET");
    if env::var_os("CARGO_FEATURE_OPUS").is_none()
        || env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos")
    {
        return;
    }
    assert_eq!(
        env::var("HOST").unwrap(),
        env::var("TARGET").unwrap(),
        "macOS Opus cross-builds require an explicit qualified SDK"
    );
    let library = PathBuf::from(
        env::var_os("FR_NATIVE_OPUS_LIBRARY_DIR")
            .expect("macOS Opus requires FR_NATIVE_OPUS_LIBRARY_DIR with the trusted libopus.a"),
    );
    assert!(
        library.is_absolute() && library.join("libopus.a").is_file(),
        "FR_NATIVE_OPUS_LIBRARY_DIR must be absolute and contain the trusted libopus.a"
    );
    println!(
        "cargo:rerun-if-changed={}",
        library.join("libopus.a").display()
    );
    println!("cargo:rustc-link-search=native={}", library.display());
    if env::var_os("CARGO_FEATURE_OPUS_PROCESS").is_none() {
        return;
    }
    let out = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    let minimum = env::var("MACOSX_DEPLOYMENT_TARGET").unwrap_or_else(|_| "11.0".into());
    assert!(
        Command::new(env::var_os("CC").unwrap_or_else(|| "cc".into()))
            .args(["-std=c11", "-O2", "-fPIC", "-Wall", "-Wextra", "-Werror"])
            .arg(format!("-mmacosx-version-min={minimum}"))
            .args(["-c", "src/opus/process/macos.c", "-o"])
            .arg(out.join("opus_macos.o"))
            .status()
            .expect("native C compiler")
            .success(),
        "macOS Opus child boundary compilation failed"
    );
    assert!(
        Command::new("ar")
            .arg("crs")
            .arg(out.join("libfropusmacos.a"))
            .arg(out.join("opus_macos.o"))
            .status()
            .unwrap()
            .success()
    );
    println!("cargo:rustc-link-search=native={}", out.display());
    println!("cargo:rustc-link-lib=static=fropusmacos");
}

/// Native output only. Opus decoding stays in its separately supervised child.
fn build_macos_audio() {
    println!("cargo:rerun-if-changed=src/macos_audio.m");
    println!("cargo:rerun-if-changed=src/macos_audio_clock.h");
    if env::var_os("CARGO_FEATURE_MACOS_AUDIO").is_none()
        || env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos")
    {
        return;
    }
    assert_eq!(
        env::var("HOST").unwrap(),
        env::var("TARGET").unwrap(),
        "macOS audio cross-builds require an explicit qualified SDK"
    );
    println!("cargo:rerun-if-env-changed=MACOSX_DEPLOYMENT_TARGET");
    let minimum = env::var("MACOSX_DEPLOYMENT_TARGET").unwrap_or_else(|_| "11.0".into());
    let out = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    assert!(
        Command::new(env::var_os("CC").unwrap_or_else(|| "cc".into()))
            .args([
                "-x",
                "objective-c",
                "-fobjc-arc",
                "-O2",
                "-fPIC",
                "-Wall",
                "-Wextra",
                "-Werror"
            ])
            .arg(format!("-mmacosx-version-min={minimum}"))
            .args(["-c", "src/macos_audio.m", "-o"])
            .arg(out.join("macosaudio.o"))
            .status()
            .expect("native Objective-C compiler")
            .success(),
        "macOS audio output boundary compilation failed"
    );
    assert!(
        Command::new("ar")
            .arg("crs")
            .arg(out.join("libfrmacosaudio.a"))
            .arg(out.join("macosaudio.o"))
            .status()
            .unwrap()
            .success()
    );
    println!("cargo:rustc-link-search=native={}", out.display());
    println!("cargo:rustc-link-lib=static=frmacosaudio");
    for framework in ["AudioToolbox", "CoreAudio", "CoreFoundation"] {
        println!("cargo:rustc-link-lib=framework={framework}");
    }
}

/// macOS viewer glue (fr-5a4w): `AppKit` window, `VideoToolbox` decode and `Metal`
/// presentation. System frameworks only; nothing is downloaded or bundled.
fn build_macos_viewer() {
    println!("cargo:rerun-if-changed=src/macos_viewer.m");
    println!("cargo:rerun-if-changed=src/macos_picker.m");
    println!("cargo:rerun-if-changed=src/macos_file_picker.m");
    if env::var_os("CARGO_FEATURE_MACOS_DESKTOP").is_none()
        || env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos")
    {
        return;
    }
    assert_eq!(
        env::var("HOST").unwrap(),
        env::var("TARGET").unwrap(),
        "macOS viewer cross-builds require an explicit SDK"
    );
    println!("cargo:rerun-if-env-changed=MACOSX_DEPLOYMENT_TARGET");
    let minimum = env::var("MACOSX_DEPLOYMENT_TARGET").unwrap_or_else(|_| "11.0".into());
    let out = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    for (source, object) in [
        ("src/macos_viewer.m", "macosviewer.o"),
        ("src/macos_picker.m", "macospicker.o"),
        ("src/macos_file_picker.m", "macosfilepicker.o"),
    ] {
        assert!(
            Command::new(env::var_os("CC").unwrap_or_else(|| "cc".into()))
                .args([
                    "-x",
                    "objective-c",
                    "-fobjc-arc",
                    "-O2",
                    "-fPIC",
                    "-Wall",
                    "-Wextra",
                    "-Werror",
                ])
                .arg(format!("-mmacosx-version-min={minimum}"))
                .args(["-c", source, "-o"])
                .arg(out.join(object))
                .status()
                .expect("native Objective-C compiler")
                .success(),
            "install the Xcode command line tools (xcode-select --install)"
        );
    }
    assert!(
        Command::new("ar")
            .arg("crs")
            .arg(out.join("libfrmacosviewer.a"))
            .arg(out.join("macosviewer.o"))
            .arg(out.join("macospicker.o"))
            .arg(out.join("macosfilepicker.o"))
            .status()
            .unwrap()
            .success()
    );
    println!("cargo:rustc-link-search=native={}", out.display());
    println!("cargo:rustc-link-lib=static=frmacosviewer");
    for framework in [
        "AppKit",
        "CoreFoundation",
        "CoreGraphics",
        "CoreMedia",
        "CoreVideo",
        "Foundation",
        "Metal",
        "QuartzCore",
        "VideoToolbox",
    ] {
        println!("cargo:rustc-link-lib=framework={framework}");
    }
    println!("cargo:rustc-link-lib=objc");
}

/// Shared host/client pasteboard boundary, independent of the viewer/codec.
/// Only system `AppKit`/`Foundation` APIs; no clipboard access at build or link.
fn build_macos_clipboard() {
    println!("cargo:rerun-if-changed=src/macos_clipboard.m");
    if env::var_os("CARGO_FEATURE_MACOS_CLIPBOARD").is_none()
        || env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos")
    {
        return;
    }
    assert_eq!(
        env::var("HOST").unwrap(),
        env::var("TARGET").unwrap(),
        "macOS clipboard cross-builds require an explicit SDK"
    );
    println!("cargo:rerun-if-env-changed=MACOSX_DEPLOYMENT_TARGET");
    let minimum = env::var("MACOSX_DEPLOYMENT_TARGET").unwrap_or_else(|_| "11.0".into());
    let out = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    assert!(
        Command::new(env::var_os("CC").unwrap_or_else(|| "cc".into()))
            .args([
                "-x",
                "objective-c",
                "-fobjc-arc",
                "-O2",
                "-fPIC",
                "-Wall",
                "-Wextra",
                "-Werror",
            ])
            .arg(format!("-mmacosx-version-min={minimum}"))
            .args(["-c", "src/macos_clipboard.m", "-o"])
            .arg(out.join("macosclipboard.o"))
            .status()
            .expect("native Objective-C compiler")
            .success(),
        "install the Xcode command line tools (xcode-select --install)"
    );
    assert!(
        Command::new("ar")
            .arg("crs")
            .arg(out.join("libfrmacosclipboard.a"))
            .arg(out.join("macosclipboard.o"))
            .status()
            .unwrap()
            .success()
    );
    println!("cargo:rustc-link-search=native={}", out.display());
    println!("cargo:rustc-link-lib=static=frmacosclipboard");
    for framework in ["AppKit", "Foundation"] {
        println!("cargo:rustc-link-lib=framework={framework}");
    }
    println!("cargo:rustc-link-lib=objc");
}

/// The macOS glue's `@available` checks call `__isPlatformVersionAtLeast`.
/// Rust's std defines it only weakly and no Rust code calls it, so LTO (the
/// release profile) drops it and the link fails with an undefined symbol; std
/// documents linking Clang's own runtime instead, which defines it strongly.
fn link_clang_runtime() {
    let glue = env::var_os("CARGO_FEATURE_MACOS_CLIPBOARD").is_some()
        || env::var_os("CARGO_FEATURE_MACOS_DESKTOP").is_some()
        || env::var_os("CARGO_FEATURE_MACOS_MEDIA").is_some()
        || env::var_os("CARGO_FEATURE_MACOS_INPUT").is_some()
        || env::var_os("CARGO_FEATURE_MACOS_SESSION_MONITOR").is_some()
        || env::var_os("CARGO_FEATURE_MACOS_OBSERVATION_INDICATOR").is_some();
    if !glue || env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
        return;
    }
    println!("cargo:rerun-if-env-changed=CC");
    let output = Command::new(env::var_os("CC").unwrap_or_else(|| "cc".into()))
        .arg("-print-resource-dir")
        .output()
        .expect("native Objective-C compiler");
    assert!(output.status.success(), "cc -print-resource-dir failed");
    let resource = String::from_utf8(output.stdout).expect("UTF-8 resource dir");
    let runtime = PathBuf::from(resource.trim()).join("lib/darwin");
    assert!(
        runtime.join("libclang_rt.osx.a").is_file(),
        "Clang's macOS runtime is missing from {}; install the Xcode command line tools",
        runtime.display()
    );
    println!("cargo:rustc-link-search=native={}", runtime.display());
    println!("cargo:rustc-link-lib=static:-bundle=clang_rt.osx");
}

/// macOS host capture glue (fr-p1-host-macos-vif): `ScreenCaptureKit` into
/// `VideoToolbox` HEVC. The binary may target macOS 13; capture explicitly
/// requires macOS 14 for the independent `SCScreenshotManager` verification.
fn build_macos_capture() {
    println!("cargo:rerun-if-changed=src/macos_capture.m");
    println!("cargo:rerun-if-changed=src/macos_capture_policy.h");
    println!("cargo:rerun-if-changed=src/macos_display.h");
    if env::var_os("CARGO_FEATURE_MACOS_MEDIA").is_none()
        || env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos")
    {
        return;
    }
    assert_eq!(
        env::var("HOST").unwrap(),
        env::var("TARGET").unwrap(),
        "macOS capture cross-builds require an explicit SDK"
    );
    println!("cargo:rerun-if-env-changed=MACOSX_DEPLOYMENT_TARGET");
    let requested = env::var("MACOSX_DEPLOYMENT_TARGET").unwrap_or_default();
    let major = requested
        .split('.')
        .next()
        .and_then(|m| m.parse::<u32>().ok())
        .unwrap_or(0);
    let minimum = if major >= 13 {
        requested
    } else {
        "13.0".into()
    };
    let out = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    assert!(
        Command::new(env::var_os("CC").unwrap_or_else(|| "cc".into()))
            .args([
                "-x",
                "objective-c",
                "-fobjc-arc",
                "-O2",
                "-fPIC",
                "-Wall",
                "-Wextra",
                "-Werror",
            ])
            .arg(format!("-mmacosx-version-min={minimum}"))
            .args(["-c", "src/macos_capture.m", "-o"])
            .arg(out.join("macoscapture.o"))
            .status()
            .expect("native Objective-C compiler")
            .success(),
        "install the Xcode command line tools (xcode-select --install)"
    );
    assert!(
        Command::new("ar")
            .arg("crs")
            .arg(out.join("libfrmacoscapture.a"))
            .arg(out.join("macoscapture.o"))
            .status()
            .unwrap()
            .success()
    );
    println!("cargo:rustc-link-search=native={}", out.display());
    println!("cargo:rustc-link-lib=static=frmacoscapture");
    for framework in [
        "CoreFoundation",
        "CoreGraphics",
        "CoreMedia",
        "CoreVideo",
        "Foundation",
        "ScreenCaptureKit",
        "VideoToolbox",
    ] {
        println!("cargo:rustc-link-lib=framework={framework}");
    }
    println!("cargo:rustc-link-lib=objc");
}

/// macOS host input glue (fr-macos-host-control-b14s): Quartz event posting
/// and the `AppKit` control indicator, linked only into `fr-input-agent`'s
/// macOS role.
fn build_macos_input() {
    println!("cargo:rerun-if-changed=src/macos_input.m");
    println!("cargo:rerun-if-changed=src/macos_display.h");
    if env::var_os("CARGO_FEATURE_MACOS_INPUT").is_none()
        || env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos")
    {
        return;
    }
    assert_eq!(
        env::var("HOST").unwrap(),
        env::var("TARGET").unwrap(),
        "macOS input cross-builds require an explicit SDK"
    );
    println!("cargo:rerun-if-env-changed=MACOSX_DEPLOYMENT_TARGET");
    let minimum = env::var("MACOSX_DEPLOYMENT_TARGET").unwrap_or_else(|_| "11.0".into());
    let out = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    assert!(
        Command::new(env::var_os("CC").unwrap_or_else(|| "cc".into()))
            .args([
                "-x",
                "objective-c",
                "-fobjc-arc",
                "-O2",
                "-fPIC",
                "-Wall",
                "-Wextra",
                "-Werror",
            ])
            .arg(format!("-mmacosx-version-min={minimum}"))
            .args(["-c", "src/macos_input.m", "-o"])
            .arg(out.join("macosinput.o"))
            .status()
            .expect("native Objective-C compiler")
            .success(),
        "install the Xcode command line tools (xcode-select --install)"
    );
    assert!(
        Command::new("ar")
            .arg("crs")
            .arg(out.join("libfrmacosinput.a"))
            .arg(out.join("macosinput.o"))
            .status()
            .unwrap()
            .success()
    );
    println!("cargo:rustc-link-search=native={}", out.display());
    println!("cargo:rustc-link-lib=static=frmacosinput");
    for framework in ["AppKit", "CoreFoundation", "CoreGraphics", "Foundation"] {
        println!("cargo:rustc-link-lib=framework={framework}");
    }
    println!("cargo:rustc-link-lib=objc");
}

/// Independent visible Stop sharing surface; system frameworks only, no input
/// injection, capture or codec bindings in this native adapter.
fn build_macos_observation_indicator() {
    println!("cargo:rerun-if-changed=src/macos_observation.m");
    if env::var_os("CARGO_FEATURE_MACOS_OBSERVATION_INDICATOR").is_none()
        || env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos")
    {
        return;
    }
    assert_eq!(
        env::var("HOST").unwrap(),
        env::var("TARGET").unwrap(),
        "macOS observation indicator cross-builds require an explicit SDK"
    );
    println!("cargo:rerun-if-env-changed=MACOSX_DEPLOYMENT_TARGET");
    let minimum = env::var("MACOSX_DEPLOYMENT_TARGET").unwrap_or_else(|_| "11.0".into());
    let out = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    assert!(
        Command::new(env::var_os("CC").unwrap_or_else(|| "cc".into()))
            .args([
                "-x",
                "objective-c",
                "-fobjc-arc",
                "-O2",
                "-fPIC",
                "-Wall",
                "-Wextra",
                "-Werror",
            ])
            .arg(format!("-mmacosx-version-min={minimum}"))
            .args(["-c", "src/macos_observation.m", "-o"])
            .arg(out.join("macosobservation.o"))
            .status()
            .expect("native Objective-C compiler")
            .success(),
        "install the Xcode command line tools (xcode-select --install)"
    );
    assert!(
        Command::new("ar")
            .arg("crs")
            .arg(out.join("libfrmacosobservation.a"))
            .arg(out.join("macosobservation.o"))
            .status()
            .unwrap()
            .success()
    );
    println!("cargo:rustc-link-search=native={}", out.display());
    println!("cargo:rustc-link-lib=static=frmacosobservation");
    for framework in ["AppKit", "CoreFoundation", "CoreGraphics", "Foundation"] {
        println!("cargo:rustc-link-lib=framework={framework}");
    }
    println!("cargo:rustc-link-lib=objc");
}

/// A read-only main-thread console-session monitor. System frameworks only;
/// it is never linked into the safe broker and does not depend on media/input.
fn build_macos_session_monitor() {
    println!("cargo:rerun-if-changed=src/macos_session.m");
    if env::var_os("CARGO_FEATURE_MACOS_SESSION_MONITOR").is_none()
        || env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos")
    {
        return;
    }
    assert_eq!(
        env::var("HOST").unwrap(),
        env::var("TARGET").unwrap(),
        "macOS session monitor cross-builds require an explicit SDK"
    );
    println!("cargo:rerun-if-env-changed=MACOSX_DEPLOYMENT_TARGET");
    let minimum = env::var("MACOSX_DEPLOYMENT_TARGET").unwrap_or_else(|_| "11.0".into());
    let out = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    assert!(
        Command::new(env::var_os("CC").unwrap_or_else(|| "cc".into()))
            .args([
                "-x",
                "objective-c",
                "-fobjc-arc",
                "-O2",
                "-fPIC",
                "-Wall",
                "-Wextra",
                "-Werror",
            ])
            .arg(format!("-mmacosx-version-min={minimum}"))
            .args(["-c", "src/macos_session.m", "-o"])
            .arg(out.join("macossession.o"))
            .status()
            .expect("native Objective-C compiler")
            .success(),
        "install the Xcode command line tools (xcode-select --install)"
    );
    assert!(
        Command::new("ar")
            .arg("crs")
            .arg(out.join("libfrmacossession.a"))
            .arg(out.join("macossession.o"))
            .status()
            .unwrap()
            .success()
    );
    println!("cargo:rustc-link-search=native={}", out.display());
    println!("cargo:rustc-link-lib=static=frmacossession");
    for framework in [
        "AppKit",
        "CoreFoundation",
        "CoreGraphics",
        "Foundation",
        "Security",
    ] {
        println!("cargo:rustc-link-lib=framework={framework}");
    }
    println!("cargo:rustc-link-lib=objc");
}
