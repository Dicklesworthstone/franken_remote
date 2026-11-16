//! Windows executables get an icon, an application manifest (Common
//! Controls 6, per-monitor DPI awareness v2, `asInvoker`) and version
//! information. Everything is generated here from source: no file is
//! downloaded and no binary asset is checked in. Other targets build nothing.
//!
//! The resource compiler is `$RC` when set, else the target's mingw
//! `windres` (GNU targets) or `rc.exe`/`llvm-rc` (MSVC targets). Without one
//! the executables still build and run, but with the system's default icon
//! and without Windows 10+ visual styles, so a warning is printed.
use std::{
    env,
    path::{Path, PathBuf},
    process::Command,
};

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=RC");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let out = PathBuf::from(env::var_os("OUT_DIR").expect("cargo sets OUT_DIR"));
    let version = version(&env::var("CARGO_PKG_VERSION").unwrap_or_default());
    std::fs::write(out.join("app.ico"), icon()).expect("write the generated icon");
    std::fs::write(out.join("app.manifest"), manifest(version)).expect("write the manifest");
    let rc = out.join("app.rc");
    std::fs::write(&rc, script(&out, version)).expect("write the resource script");
    let gnu = env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("gnu");
    let Some(compiler) = compiler(gnu) else {
        println!(
            "cargo:warning=no Windows resource compiler found (set RC, or install mingw-w64 windres / the Windows SDK rc.exe); building without icon, manifest and version information"
        );
        return;
    };
    let object = if gnu {
        out.join("app_resources.o")
    } else {
        out.join("app_resources.res")
    };
    let mut command = Command::new(&compiler);
    let windres = compiler
        .file_name()
        .is_some_and(|name| name.to_string_lossy().contains("windres"));
    if windres {
        command
            .arg("--input-format=rc")
            .arg(if gnu {
                "--output-format=coff"
            } else {
                "--output-format=res"
            })
            .arg("-i")
            .arg(&rc)
            .arg("-o")
            .arg(&object);
    } else {
        // rc.exe and llvm-rc share the classic flags.
        command.arg("/nologo").arg("/fo").arg(&object).arg(&rc);
    }
    let status = command
        .status()
        .unwrap_or_else(|error| panic!("run {}: {error}", compiler.display()));
    assert!(
        status.success(),
        "{} failed to compile the generated resources ({status})",
        compiler.display()
    );
    println!("cargo:rustc-link-arg-bins={}", object.display());
}

fn compiler(gnu: bool) -> Option<PathBuf> {
    if let Some(explicit) = env::var_os("RC").filter(|value| !value.is_empty()) {
        return Some(PathBuf::from(explicit));
    }
    let arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    let candidates: Vec<String> = if gnu {
        let prefix = match arch.as_str() {
            "x86" => "i686-w64-mingw32",
            "aarch64" => "aarch64-w64-mingw32",
            _ => "x86_64-w64-mingw32",
        };
        vec![format!("{prefix}-windres"), "windres".to_owned()]
    } else {
        vec!["rc".to_owned(), "llvm-rc".to_owned()]
    };
    candidates.into_iter().find_map(|name| find(&name))
}

fn find(name: &str) -> Option<PathBuf> {
    let paths = env::var_os("PATH")?;
    env::split_paths(&paths).find_map(|dir| {
        [name.to_owned(), format!("{name}.exe")]
            .into_iter()
            .map(|file| dir.join(file))
            .find(|path| path.is_file())
    })
}

fn version(text: &str) -> [u16; 3] {
    let mut parts = text
        .split(['.', '-', '+'])
        .map(|part| part.parse::<u16>().unwrap_or(0));
    [
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
    ]
}

fn rc_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/").replace('"', "")
}

fn script(out: &Path, [major, minor, patch]: [u16; 3]) -> String {
    let dotted = format!("{major}.{minor}.{patch}");
    format!(
        r#"#pragma code_page(65001)
1 ICON "{icon}"
1 24 "{manifest}"
1 VERSIONINFO
FILEVERSION {major},{minor},{patch},0
PRODUCTVERSION {major},{minor},{patch},0
FILEFLAGSMASK 0x3fL
FILEFLAGS 0x0L
FILEOS 0x40004L
FILETYPE 0x1L
FILESUBTYPE 0x0L
BEGIN
  BLOCK "StringFileInfo"
  BEGIN
    BLOCK "040904b0"
    BEGIN
      VALUE "CompanyName", "FrankenRemote"
      VALUE "FileDescription", "FrankenRemote remote desktop for Tailscale"
      VALUE "FileVersion", "{dotted}"
      VALUE "ProductName", "FrankenRemote"
      VALUE "ProductVersion", "{dotted}"
    END
  END
  BLOCK "VarFileInfo"
  BEGIN
    VALUE "Translation", 0x409, 1200
  END
END
"#,
        icon = rc_path(&out.join("app.ico")),
        manifest = rc_path(&out.join("app.manifest")),
    )
}

fn manifest([major, minor, patch]: [u16; 3]) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0">
  <assemblyIdentity type="win32" name="FrankenRemote" version="{major}.{minor}.{patch}.0" processorArchitecture="*"/>
  <description>FrankenRemote</description>
  <dependency>
    <dependentAssembly>
      <assemblyIdentity type="win32" name="Microsoft.Windows.Common-Controls" version="6.0.0.0" processorArchitecture="*" publicKeyToken="6595b64144ccf1df" language="*"/>
    </dependentAssembly>
  </dependency>
  <trustInfo xmlns="urn:schemas-microsoft-com:asm.v3">
    <security>
      <requestedPrivileges>
        <requestedExecutionLevel level="asInvoker" uiAccess="false"/>
      </requestedPrivileges>
    </security>
  </trustInfo>
  <compatibility xmlns="urn:schemas-microsoft-com:compatibility.v1">
    <application>
      <supportedOS Id="{{8e0f7a12-bfb3-4fe8-b9a5-48fd50a15a9a}}"/>
    </application>
  </compatibility>
  <application xmlns="urn:schemas-microsoft-com:asm.v3">
    <windowsSettings>
      <dpiAware xmlns="http://schemas.microsoft.com/SMI/2005/WindowsSettings">true/pm</dpiAware>
      <dpiAwareness xmlns="http://schemas.microsoft.com/SMI/2016/WindowsSettings">PerMonitorV2</dpiAwareness>
    </windowsSettings>
  </application>
</assembly>
"#
    )
}

/// A multi-resolution icon: a rounded tile with a blue-to-teal gradient and
/// a white monitor whose screen shows a pointer. 32-bit BMP entries with
/// alpha; each pixel is 4x4 supersampled.
fn icon() -> Vec<u8> {
    const SIZES: [u32; 7] = [16, 20, 24, 32, 48, 64, 256];
    let images: Vec<Vec<u8>> = SIZES.iter().map(|&size| bitmap(size)).collect();
    let mut file = Vec::new();
    file.extend_from_slice(&0_u16.to_le_bytes());
    file.extend_from_slice(&1_u16.to_le_bytes());
    file.extend_from_slice(&u16::try_from(SIZES.len()).unwrap_or(0).to_le_bytes());
    let mut offset = 6 + 16 * u32::try_from(SIZES.len()).unwrap_or(0);
    for (size, image) in SIZES.iter().zip(&images) {
        let edge = u8::try_from(*size).unwrap_or(0); // 256 is written as 0
        file.extend_from_slice(&[edge, edge, 0, 0]);
        file.extend_from_slice(&1_u16.to_le_bytes());
        file.extend_from_slice(&32_u16.to_le_bytes());
        let length = u32::try_from(image.len()).unwrap_or(0);
        file.extend_from_slice(&length.to_le_bytes());
        file.extend_from_slice(&offset.to_le_bytes());
        offset += length;
    }
    for image in images {
        file.extend_from_slice(&image);
    }
    file
}

fn bitmap(size: u32) -> Vec<u8> {
    let mask_row = size.div_ceil(32) * 4;
    let pixels = size * size * 4;
    let mut data = Vec::with_capacity(40 + (pixels + mask_row * size) as usize);
    let header = [
        40_u32,
        size,
        size * 2,
        u32::from(1_u16) | (32 << 16),
        0,
        pixels + mask_row * size,
        0,
        0,
        0,
        0,
    ];
    for value in header {
        data.extend_from_slice(&value.to_le_bytes());
    }
    // Bottom-up rows of premultiplication-free BGRA.
    for row in (0..size).rev() {
        for column in 0..size {
            let [red, green, blue, alpha] = sample(size, column, row);
            data.extend_from_slice(&[blue, green, red, alpha]);
        }
    }
    // The AND mask is unused with 32-bit alpha but must be present.
    data.resize(data.len() + (mask_row * size) as usize, 0);
    data
}

#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
fn sample(size: u32, column: u32, row: u32) -> [u8; 4] {
    const GRID: u32 = 4;
    let mut sum = [0.0_f32; 4];
    for sy in 0..GRID {
        for sx in 0..GRID {
            let across = (column as f32 + (sx as f32 + 0.5) / GRID as f32) / size as f32;
            let down = (row as f32 + (sy as f32 + 0.5) / GRID as f32) / size as f32;
            let [red, green, blue, alpha] = shade(across, down);
            // Accumulate premultiplied so edges blend correctly.
            sum[0] += red * alpha;
            sum[1] += green * alpha;
            sum[2] += blue * alpha;
            sum[3] += alpha;
        }
    }
    let count = (GRID * GRID) as f32;
    let alpha = sum[3] / count;
    let channel = |value: f32| {
        if alpha <= 0.0 {
            0
        } else {
            ((value / count / alpha).clamp(0.0, 1.0) * 255.0).round() as u8
        }
    };
    [
        channel(sum[0]),
        channel(sum[1]),
        channel(sum[2]),
        (alpha.clamp(0.0, 1.0) * 255.0).round() as u8,
    ]
}

/// Color at a point of the unit square (x right, y down).
fn shade(x: f32, y: f32) -> [f32; 4] {
    let inset = 0.04;
    if !rounded(x, y, inset, inset, 1.0 - inset, 1.0 - inset, 0.2) {
        return [0.0; 4];
    }
    // Tile gradient: indigo (top left) to teal (bottom right).
    let t = f32::midpoint(x, y).clamp(0.0, 1.0);
    let mix = |a: f32, b: f32| a + (b - a) * t;
    let tile = [mix(0.30, 0.06), mix(0.36, 0.66), mix(0.96, 0.75), 1.0];
    let white = [1.0, 1.0, 1.0, 1.0];
    // Monitor body and its screen.
    let (left, top, right, bottom) = (0.17, 0.22, 0.83, 0.66);
    let border = 0.055;
    if rounded(x, y, left, top, right, bottom, 0.06) {
        if rounded(
            x,
            y,
            left + border,
            top + border,
            right - border,
            bottom - border,
            0.025,
        ) {
            // Screen: a darker tint of the tile with a pointer.
            let screen = [tile[0] * 0.55, tile[1] * 0.55, tile[2] * 0.6, 1.0];
            return if pointer(x, y) { white } else { screen };
        }
        return white;
    }
    // Neck and foot of the stand.
    if (0.45..=0.55).contains(&x) && (bottom..=0.75).contains(&y) {
        return white;
    }
    if rounded(x, y, 0.32, 0.74, 0.68, 0.80, 0.03) {
        return white;
    }
    tile
}

/// A classic arrow pointer inside the screen.
fn pointer(x: f32, y: f32) -> bool {
    // Triangle tip at (0.43, 0.32), base corners at (0.43, 0.56) and (0.60, 0.50).
    let inside = |(ax, ay): (f32, f32), (bx, by): (f32, f32), (cx, cy): (f32, f32)| {
        let d1 = (x - bx) * (ay - by) - (ax - bx) * (y - by);
        let d2 = (x - cx) * (by - cy) - (bx - cx) * (y - cy);
        let d3 = (x - ax) * (cy - ay) - (cx - ax) * (y - ay);
        let negative = d1 < 0.0 || d2 < 0.0 || d3 < 0.0;
        let positive = d1 > 0.0 || d2 > 0.0 || d3 > 0.0;
        !(negative && positive)
    };
    let head = inside((0.43, 0.31), (0.43, 0.55), (0.61, 0.49));
    // The tail, a short slanted bar from the head toward the lower right.
    let tail = {
        let (px, py) = (x - 0.50, y - 0.50);
        let (dx, dy) = (0.6_f32, 0.8_f32);
        let along = px * dx + py * dy;
        let across = (px * dy - py * dx).abs();
        (0.0..=0.09).contains(&along) && across <= 0.022
    };
    head || tail
}

fn rounded(x: f32, y: f32, left: f32, top: f32, right: f32, bottom: f32, radius: f32) -> bool {
    if x < left || x > right || y < top || y > bottom {
        return false;
    }
    let cx = x.clamp(left + radius, right - radius);
    let cy = y.clamp(top + radius, bottom - radius);
    (x - cx).powi(2) + (y - cy).powi(2) <= radius * radius
}
