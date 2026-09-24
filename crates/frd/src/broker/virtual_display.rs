#![forbid(unsafe_code)]
//! Headless virtual display provisioning and EDID emulation (plan sections 2, 5.2, bead fr-j8y).
//!
//! Provides automated virtual display creation for headless hosts and monitorless servers:
//! - Standard 128-byte VESA/CTA-861 EDID v1.4 block generation, parsing, and checksum validation.
//! - Virtual X11 display lifecycle via isolated `Xvfb` child process (`-displayfd 1`, `-nolisten tcp`).
//! - Automatic display number allocation and cleanup on termination (no zombie X servers).

use core::fmt;
use std::io::{BufRead, BufReader, Read};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};

/// Protected absolute location; never resolved through a caller's PATH.
const XVFB: &str = "/usr/bin/Xvfb";

/// Configuration for headless virtual display provisioning.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VirtualDisplayConfig {
    /// Whether virtual display auto-provisioning is enabled for headless operation.
    pub enabled: bool,
    /// Virtual display pixel width (default 1920).
    pub width: u32,
    /// Virtual display pixel height (default 1080).
    pub height: u32,
    /// Color bit depth (default 24).
    pub depth: u32,
    /// Specific display number requested, or `None` for automatic assignment.
    pub display_num: Option<u32>,
    /// Whether to generate and validate an emulated EDID structure.
    pub emulate_edid: bool,
}

impl Default for VirtualDisplayConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            width: 1920,
            height: 1080,
            depth: 24,
            display_num: None,
            emulate_edid: true,
        }
    }
}

/// Errors occurring during virtual display provisioning or EDID validation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VirtualDisplayError {
    /// The required headless display server binary (e.g. `Xvfb`) was not found in PATH.
    BinaryNotFound(String),
    /// Failed to spawn the virtual display process.
    SpawnFailed(String),
    /// Displayfd pipe closed before providing a valid display number.
    DisplayFdClosed,
    /// Invalid display number received from displayfd.
    InvalidDisplayNumber(String),
    /// The virtual display process terminated prematurely.
    ProcessDiedEarly(String),
    /// Invalid EDID block length (must be 128 bytes).
    InvalidEdidLength(usize),
    /// Invalid EDID fixed header pattern.
    InvalidEdidHeader,
    /// Invalid EDID checksum (sum of all 128 bytes must equal 0 mod 256).
    InvalidEdidChecksum { calculated: u8 },
    /// Unsupported resolution dimensions.
    UnsupportedResolution { width: u32, height: u32 },
}

impl fmt::Display for VirtualDisplayError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BinaryNotFound(bin) => {
                write!(f, "virtual display binary '{bin}' not found in PATH")
            }
            Self::SpawnFailed(err) => write!(f, "failed to spawn virtual display: {err}"),
            Self::DisplayFdClosed => {
                f.write_str("displayfd closed without returning a display number")
            }
            Self::InvalidDisplayNumber(num) => {
                write!(f, "invalid display number from displayfd: '{num}'")
            }
            Self::ProcessDiedEarly(msg) => {
                write!(f, "virtual display process terminated unexpectedly: {msg}")
            }
            Self::InvalidEdidLength(len) => {
                write!(f, "invalid EDID length {len} (expected 128 bytes)")
            }
            Self::InvalidEdidHeader => f.write_str("invalid EDID fixed header"),
            Self::InvalidEdidChecksum { calculated } => {
                write!(
                    f,
                    "invalid EDID checksum (calculated sum byte: 0x{calculated:02x}, expected 0x00)"
                )
            }
            Self::UnsupportedResolution { width, height } => {
                write!(f, "unsupported virtual display resolution {width}x{height}")
            }
        }
    }
}

impl std::error::Error for VirtualDisplayError {}

/// VESA EDID 1.4 block generator and validator for virtual/emulated displays.
pub struct EdidBlock;

/// Summary information extracted from an EDID block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EdidSummary {
    /// Manufacturer 3-letter ASCII code.
    pub manufacturer: String,
    /// Product code ID.
    pub product_code: u16,
    /// Horizontal active pixels.
    pub width: u32,
    /// Vertical active pixels.
    pub height: u32,
    /// Refresh rate in Hz (nominal).
    pub refresh_hz: u32,
}

impl EdidBlock {
    /// Fixed EDID 1.4 header bytes: 00 FF FF FF FF FF FF 00.
    pub const HEADER: [u8; 8] = [0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00];

    /// Generate an authentic 128-byte VESA EDID 1.4 block for 1920x1080 @ 60Hz.
    #[must_use]
    pub fn generate_1080p() -> [u8; 128] {
        let mut edid = [0u8; 128];
        // 0..8: Fixed Header
        edid[0..8].copy_from_slice(&Self::HEADER);

        // 8..10: Manufacturer ID "FRD"
        // 'F'=6, 'R'=18, 'D'=4 -> (6 << 10) | (18 << 5) | 4 = 0x1A44 -> [0x1A, 0x44]
        edid[8] = 0x1A;
        edid[9] = 0x44;

        // 10..12: Product Code = 0x0001
        edid[10] = 0x01;
        edid[11] = 0x00;

        // 12..16: Serial number = 1
        edid[12] = 0x01;
        edid[13] = 0x00;
        edid[14] = 0x00;
        edid[15] = 0x00;

        // 16..18: Week 1, Year 2026 (2026 - 1990 = 36)
        edid[16] = 0x01;
        edid[17] = 36;

        // 18..20: EDID version 1.4
        edid[18] = 0x01;
        edid[19] = 0x04;

        // 20: Digital video input, 8 bits per primary color
        edid[20] = 0xA5;

        // 21..22: Screen size 53cm x 30cm (~24 inch 16:9 display)
        edid[21] = 53;
        edid[22] = 30;

        // 23: Display gamma 2.2 ((2.2 * 100) - 100 = 120)
        edid[23] = 120;

        // 24: Feature support (RGB 4:4:4, preferred timing mode)
        edid[24] = 0x02;

        // 38..54: Standard timings: 1920x1080 @ 60Hz (D1C0 in standard timing format)
        edid[38] = 0xD1;
        edid[39] = 0xC0;

        // 54..72: Detailed Timing Descriptor #1 (1920x1080 @ 60Hz, 148.50 MHz pixel clock)
        // Pixel clock: 148.5 MHz = 14850 = 0x3A02 -> [0x02, 0x3A]
        edid[54] = 0x02;
        edid[55] = 0x3A;
        // Horizontal active: 1920 (0x780), blanking: 280 (0x118)
        edid[56] = 0x80; // H active lower 8 bits (0x80)
        edid[57] = 0x18; // H blanking lower 8 bits (0x18)
        edid[58] = 0x71; // Upper 4 bits H active (7) and H blanking (1)
        // Vertical active: 1080 (0x438), blanking: 45 (0x02D)
        edid[59] = 0x38; // V active lower 8 bits (0x38)
        edid[60] = 0x2D; // V blanking lower 8 bits (0x2D)
        edid[61] = 0x40; // Upper 4 bits V active (4) and V blanking (0)
        // Sync offset and pulse widths
        edid[62] = 0x58; // H front porch 88, sync pulse 44
        edid[63] = 0x2C;
        edid[64] = 0x45;
        edid[65] = 0x00;
        // Image size 531mm x 298mm
        edid[66] = 0x13;
        edid[67] = 0x2A;
        edid[68] = 0x21;
        // Flags: digital separate sync, positive polarity
        edid[71] = 0x1E;

        // 72..90: Display Name descriptor "FRD Virtual"
        edid[72] = 0x00;
        edid[73] = 0x00;
        edid[74] = 0x00;
        edid[75] = 0xFC; // Monitor name tag
        edid[76] = 0x00;
        let name = b"FRD Virtual\n";
        edid[77..77 + name.len()].copy_from_slice(name);

        // Calculate checksum: byte 127 must make the sum of all 128 bytes equal 0 mod 256.
        let sum: u8 = edid[0..127].iter().fold(0u8, |acc, &b| acc.wrapping_add(b));
        edid[127] = (0u8).wrapping_sub(sum);

        edid
    }

    /// Validate a 128-byte EDID structure and extract its primary timing summary.
    pub fn validate(edid: &[u8]) -> Result<EdidSummary, VirtualDisplayError> {
        if edid.len() != 128 {
            return Err(VirtualDisplayError::InvalidEdidLength(edid.len()));
        }
        if edid[0..8] != Self::HEADER {
            return Err(VirtualDisplayError::InvalidEdidHeader);
        }
        let total_sum: u8 = edid.iter().fold(0u8, |acc, &b| acc.wrapping_add(b));
        if total_sum != 0 {
            return Err(VirtualDisplayError::InvalidEdidChecksum {
                calculated: total_sum,
            });
        }

        // Decode manufacturer
        let m1 = (edid[8] >> 2) & 0x1F;
        let m2 = ((edid[8] & 0x03) << 3) | ((edid[9] >> 5) & 0x07);
        let m3 = edid[9] & 0x1F;
        let manufacturer = format!(
            "{}{}{}",
            (b'@' + m1) as char,
            (b'@' + m2) as char,
            (b'@' + m3) as char,
        );

        let product_code = u16::from_le_bytes([edid[10], edid[11]]);

        // Extract detailed timing 1
        let h_active = (u32::from(edid[58] >> 4) << 8) | u32::from(edid[56]);
        let v_active = (u32::from(edid[61] >> 4) << 8) | u32::from(edid[59]);

        Ok(EdidSummary {
            manufacturer,
            product_code,
            width: h_active,
            height: v_active,
            refresh_hz: 60,
        })
    }
}

/// An active virtual display instance owned by the host broker.
pub struct VirtualDisplayInstance {
    /// X11 display string (e.g. `":99"`).
    pub display: String,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Emulated EDID 128-byte block if enabled.
    pub edid: Option<[u8; 128]>,
    /// Private MIT-MAGIC-COOKIE-1 authority file; only holders can connect.
    pub xauthority: Option<PathBuf>,
    /// Child process handle for supervised Xvfb instance.
    child: Option<Child>,
}

impl fmt::Debug for VirtualDisplayInstance {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VirtualDisplayInstance")
            .field("display", &self.display)
            .field("width", &self.width)
            .field("height", &self.height)
            .finish_non_exhaustive()
    }
}

impl VirtualDisplayInstance {
    /// Create an instance wrapping an existing or pre-provisioned display.
    #[must_use]
    pub fn existing(display: String, width: u32, height: u32) -> Self {
        Self {
            display,
            width,
            height,
            edid: None,
            xauthority: None,
            child: None,
        }
    }

    /// Check if the supervised child process is still running.
    pub fn is_alive(&mut self) -> bool {
        if let Some(child) = &mut self.child {
            matches!(child.try_wait(), Ok(None))
        } else {
            false
        }
    }

    /// Explicitly terminate the virtual display process cleanly.
    pub fn stop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        if let Some(path) = self.xauthority.take() {
            let _ = std::fs::remove_file(&path);
            if let Some(dir) = path.parent() {
                let _ = std::fs::remove_dir(dir);
            }
        }
    }
}

impl Drop for VirtualDisplayInstance {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Write a fresh random cookie as a single `FamilyWild` Xauthority entry in a
/// new 0700 directory. The server loads it via `-auth`; clients (the capture
/// worker) use it via `XAUTHORITY`. Without it, any local user could connect.
#[cfg(target_os = "linux")]
fn private_authority() -> Result<PathBuf, VirtualDisplayError> {
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    let failed =
        |e: &dyn fmt::Display| VirtualDisplayError::SpawnFailed(format!("xauthority: {e}"));
    let mut random = [0u8; 32];
    getrandom::fill(&mut random).map_err(|e| failed(&e))?;
    let (tag, cookie) = random.split_at(16);
    let tag = tag.iter().fold(String::new(), |mut hex, b| {
        let _ = std::fmt::Write::write_fmt(&mut hex, format_args!("{b:02x}"));
        hex
    });
    let dir = std::env::temp_dir().join(format!("frd-xvfb-{tag}"));
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&dir)
        .map_err(|e| failed(&e))?;
    let mut entry = Vec::with_capacity(64);
    entry.extend_from_slice(&0xffff_u16.to_be_bytes()); // FamilyWild
    for field in [&b""[..], &b""[..], &b"MIT-MAGIC-COOKIE-1"[..], cookie] {
        let len = u16::try_from(field.len()).map_err(|e| failed(&e))?;
        entry.extend_from_slice(&len.to_be_bytes());
        entry.extend_from_slice(field);
    }
    let path = dir.join("Xauthority");
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .map_err(|e| failed(&e))?;
    std::io::Write::write_all(&mut file, &entry).map_err(|e| failed(&e))?;
    Ok(path)
}

/// Headless virtual display lifecycle manager.
pub struct VirtualDisplayManager;

impl VirtualDisplayManager {
    /// Probe if a virtual display server (Xvfb) is available on the system.
    #[must_use]
    pub fn is_available() -> bool {
        #[cfg(target_os = "linux")]
        {
            Command::new(XVFB)
                .arg("-help")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|s| s.success() || s.code().is_some())
        }
        #[cfg(not(target_os = "linux"))]
        {
            false
        }
    }

    /// Provision an isolated headless virtual display using Xvfb.
    #[cfg(target_os = "linux")]
    pub fn start(
        config: &VirtualDisplayConfig,
    ) -> Result<VirtualDisplayInstance, VirtualDisplayError> {
        let width = config.width;
        let height = config.height;
        let depth = config.depth;

        if width == 0 || height == 0 || width > 8192 || height > 8192 {
            return Err(VirtualDisplayError::UnsupportedResolution { width, height });
        }

        let xauthority = private_authority()?;
        let mut cmd = Command::new(XVFB);
        cmd.arg("-auth").arg(&xauthority);
        cmd.args([
            "-displayfd",
            "1",
            "-screen",
            "0",
            &format!("{width}x{height}x{depth}"),
            "-nolisten",
            "tcp",
            "+extension",
            "DAMAGE",
            "+extension",
            "COMPOSITE",
            "-noreset",
        ]);

        cmd.stdout(Stdio::piped()).stderr(Stdio::inherit());

        let mut child = cmd.spawn().map_err(|e| {
            let _ = std::fs::remove_file(&xauthority);
            if e.kind() == std::io::ErrorKind::NotFound {
                VirtualDisplayError::BinaryNotFound(XVFB.into())
            } else {
                VirtualDisplayError::SpawnFailed(e.to_string())
            }
        })?;

        let stdout = child
            .stdout
            .take()
            .ok_or(VirtualDisplayError::DisplayFdClosed)?;
        let mut reader = BufReader::new(stdout.take(32));
        let mut display_line = String::new();

        if reader.read_line(&mut display_line).is_err() || display_line.trim().is_empty() {
            let _ = child.kill();
            let _ = child.wait();
            return Err(VirtualDisplayError::DisplayFdClosed);
        }

        let num_str = display_line.trim();
        let display_num = num_str.parse::<u32>().map_err(|_| {
            let _ = child.kill();
            let _ = child.wait();
            VirtualDisplayError::InvalidDisplayNumber(num_str.to_string())
        })?;

        let display = format!(":{display_num}");

        let edid = if config.emulate_edid {
            Some(EdidBlock::generate_1080p())
        } else {
            None
        };

        Ok(VirtualDisplayInstance {
            display,
            width,
            height,
            edid,
            xauthority: Some(xauthority),
            child: Some(child),
        })
    }

    /// Headless provisioning stub on non-Linux platforms.
    #[cfg(not(target_os = "linux"))]
    pub fn start(
        _config: &VirtualDisplayConfig,
    ) -> Result<VirtualDisplayInstance, VirtualDisplayError> {
        Err(VirtualDisplayError::BinaryNotFound(
            "Xvfb (Linux only)".into(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edid_1080p_generation_and_validation() {
        let edid = EdidBlock::generate_1080p();
        assert_eq!(edid.len(), 128);

        // Sum must be zero mod 256
        let sum: u8 = edid.iter().fold(0u8, |acc, &b| acc.wrapping_add(b));
        assert_eq!(sum, 0, "EDID checksum must equal 0 mod 256");

        let summary = EdidBlock::validate(&edid).expect("EDID must validate successfully");
        assert_eq!(summary.manufacturer, "FRD");
        assert_eq!(summary.product_code, 1);
        assert_eq!(summary.width, 1920);
        assert_eq!(summary.height, 1080);
        assert_eq!(summary.refresh_hz, 60);
    }

    #[test]
    fn edid_validation_rejects_corrupted_checksum() {
        let mut edid = EdidBlock::generate_1080p();
        edid[10] ^= 0xFF; // Corrupt a byte
        let err = EdidBlock::validate(&edid).unwrap_err();
        assert!(matches!(
            err,
            VirtualDisplayError::InvalidEdidChecksum { .. }
        ));
    }

    #[test]
    fn edid_validation_rejects_bad_header() {
        let mut edid = EdidBlock::generate_1080p();
        edid[0] = 0xAA;
        let err = EdidBlock::validate(&edid).unwrap_err();
        assert_eq!(err, VirtualDisplayError::InvalidEdidHeader);
    }

    #[test]
    fn edid_validation_rejects_truncated_length() {
        let edid = [0u8; 64];
        let err = EdidBlock::validate(&edid).unwrap_err();
        assert_eq!(err, VirtualDisplayError::InvalidEdidLength(64));
    }

    #[test]
    fn virtual_display_rejects_zero_and_oversized_resolutions() {
        let bad_cfg = VirtualDisplayConfig {
            enabled: true,
            width: 0,
            height: 1080,
            ..Default::default()
        };
        #[cfg(target_os = "linux")]
        {
            let res = VirtualDisplayManager::start(&bad_cfg);
            assert!(matches!(
                res,
                Err(VirtualDisplayError::UnsupportedResolution { .. })
            ));
        }

        let oversized_cfg = VirtualDisplayConfig {
            enabled: true,
            width: 9000,
            height: 1080,
            ..Default::default()
        };
        #[cfg(target_os = "linux")]
        {
            let res = VirtualDisplayManager::start(&oversized_cfg);
            assert!(matches!(
                res,
                Err(VirtualDisplayError::UnsupportedResolution { .. })
            ));
        }
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn virtual_display_manager_live_xvfb_provisioning_and_cleanup() {
        if !VirtualDisplayManager::is_available() {
            eprintln!("Xvfb not available in test environment; skipping live test");
            return;
        }

        let config = VirtualDisplayConfig {
            enabled: true,
            width: 640,
            height: 480,
            depth: 24,
            display_num: None,
            emulate_edid: true,
        };

        let mut instance =
            VirtualDisplayManager::start(&config).expect("live Xvfb start must succeed");
        assert!(
            instance.display.starts_with(':'),
            "display should be formatted as :N"
        );
        assert_eq!(instance.width, 640);
        assert_eq!(instance.height, 480);
        assert!(
            instance.is_alive(),
            "virtual display child should be running"
        );

        // Stop cleanly
        instance.stop();
        assert!(!instance.is_alive(), "virtual display should be stopped");
    }

    /// Raw X11 connection setup: status byte 1 = Success, 0 = Failed.
    #[cfg(target_os = "linux")]
    fn setup_status(display: &str, cookie: Option<&[u8]>) -> u8 {
        use std::io::Write;
        let number = display.trim_start_matches(':');
        let mut socket =
            std::os::unix::net::UnixStream::connect(format!("/tmp/.X11-unix/X{number}")).unwrap();
        let (name, data): (&[u8], &[u8]) = match cookie {
            Some(c) => (b"MIT-MAGIC-COOKIE-1", c),
            None => (b"", b""),
        };
        let pad = |n: usize| (4 - n % 4) % 4;
        let mut request = vec![b'l', 0, 11, 0, 0, 0];
        request.extend_from_slice(&u16::try_from(name.len()).unwrap().to_le_bytes());
        request.extend_from_slice(&u16::try_from(data.len()).unwrap().to_le_bytes());
        request.extend_from_slice(&[0, 0]);
        request.extend_from_slice(name);
        request.extend(std::iter::repeat_n(0, pad(name.len())));
        request.extend_from_slice(data);
        request.extend(std::iter::repeat_n(0, pad(data.len())));
        socket.write_all(&request).unwrap();
        let mut status = [0u8; 1];
        socket.read_exact(&mut status).unwrap();
        status[0]
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn headless_display_refuses_clients_without_its_private_cookie() {
        use std::os::unix::fs::PermissionsExt;
        if !std::path::Path::new(XVFB).exists() {
            eprintln!("SKIPPED: {XVFB} not installed");
            return;
        }
        let mut instance = VirtualDisplayManager::start(&VirtualDisplayConfig {
            enabled: true,
            width: 320,
            height: 240,
            ..VirtualDisplayConfig::default()
        })
        .unwrap();
        let authority = instance.xauthority.clone().unwrap();
        let bytes = std::fs::read(&authority).unwrap();
        let cookie = &bytes[bytes.len() - 16..];
        let mode = std::fs::metadata(&authority).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert_eq!(setup_status(&instance.display, None), 0, "unauthenticated");
        assert_eq!(
            setup_status(&instance.display, Some(&[0u8; 16])),
            0,
            "wrong cookie"
        );
        assert_eq!(
            setup_status(&instance.display, Some(cookie)),
            1,
            "owner cookie"
        );
        instance.stop();
        assert!(!authority.exists(), "cookie removed on stop");
    }
}
