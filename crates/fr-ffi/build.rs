use std::{env, path::PathBuf, process::Command};

fn main() {
    println!("cargo:rerun-if-changed=src/ffmpeg_bridge.c");
    println!("cargo:rerun-if-changed=src/ffmpeg_bridge.h");
    println!("cargo:rerun-if-env-changed=FR_FFI_INCLUDE_DIR");
    println!("cargo:rerun-if-env-changed=FR_FFI_LIBRARY_DIR");

    let packages = ["libavcodec", "libavutil", "libswscale"];
    let include_env = env::var_os("FR_FFI_INCLUDE_DIR");
    let library_env = env::var_os("FR_FFI_LIBRARY_DIR");

    let (cflags, has_ffmpeg) = if let (Some(inc), Some(lib)) = (&include_env, &library_env) {
        let inc_path = PathBuf::from(inc);
        let lib_path = PathBuf::from(lib);
        println!("cargo:rustc-link-search=native={}", lib_path.display());
        for pkg in &["avcodec", "avutil", "swscale"] {
            println!("cargo:rustc-link-lib={pkg}");
        }
        (vec![format!("-I{}", inc_path.display())], true)
    } else {
        let probe = Command::new("pkg-config")
            .arg("--cflags")
            .args(packages)
            .output();

        match probe {
            Ok(output) if output.status.success() => {
                let flags_str = String::from_utf8_lossy(&output.stdout);
                let flags = flags_str
                    .split_whitespace()
                    .map(str::to_owned)
                    .collect::<Vec<_>>();

                // Link flags
                let libs = Command::new("pkg-config")
                    .arg("--libs")
                    .args(packages)
                    .output()
                    .expect("pkg-config libs failed");
                let libs_str = String::from_utf8_lossy(&libs.stdout);
                for token in libs_str.split_whitespace() {
                    if let Some(path) = token.strip_prefix("-L") {
                        println!("cargo:rustc-link-search=native={path}");
                    } else if let Some(lib) = token.strip_prefix("-l") {
                        println!("cargo:rustc-link-lib={lib}");
                    }
                }
                (flags, true)
            }
            _ => {
                // Fallback stub mode for targets without dev packages
                (vec!["-DFR_FFI_STUB_ONLY".to_string()], false)
            }
        }
    };

    let mut build = cc::Build::new();
    build
        .file("src/ffmpeg_bridge.c")
        .include("src")
        .std("c11")
        .opt_level(2)
        .warnings(true);

    for flag in &cflags {
        build.flag(flag);
    }

    if !has_ffmpeg {
        build.define("FR_FFI_STUB_ONLY", None);
    }

    build.compile("fr_ffmpeg_bridge");
}
