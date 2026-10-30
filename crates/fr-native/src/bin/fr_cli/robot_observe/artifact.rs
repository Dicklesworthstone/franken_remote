//! Explicit local screenshot publication after the native session has cleaned up.
//! One bounded PNG allocation; no image codec dependency, remote path, or pixel log.
use super::Failure;
use fr_core::limits::ProtocolLimits;
use fr_media::worker::presentation_snapshot::MAX_PIXEL_BYTES;
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::{fs::OpenOptions, io::Write, path::Path};

const BLOCK_BYTES: usize = 65_535;
const MAX_PNG_BYTES: usize = MAX_PIXEL_BYTES + 8192 + 513 * 5 + 63;
const SIGNATURE: [u8; 8] = [137, b'P', b'N', b'G', 13, 10, 26, 10];

pub(super) struct Saved {
    pub byte_count: usize,
    pub sha256: String,
}

/// Validate and encode before opening the user-selected destination. Atomic
/// exclusive creation refuses an existing file or symlink, including a dangling
/// link. A write/sync failure can leave a partial NEW file; no artifact is claimed.
pub(super) fn write(path: &Path, width: u32, height: u32, bgra: &[u8]) -> Result<Saved, Failure> {
    let png = encode(width, height, bgra)?;
    let saved = Saved {
        byte_count: png.len(),
        sha256: fr_client::robot::sha256::sha256_hex(&png),
    };
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options.open(path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::AlreadyExists {
            Failure::new(
                "observation_artifact_exists",
                "The screenshot destination already exists. Choose a new path; existing files and symlinks are never overwritten.",
                1,
            )
        } else {
            Failure::new(
                "observation_artifact_unwritable",
                "The screenshot destination could not be created. Check the selected local directory and permissions; no existing file was replaced.",
                1,
            )
        }
    })?;
    file.write_all(&png).and_then(|()| file.sync_all()).map_err(|_| {
        Failure::new(
            "observation_artifact_write_failed",
            "The new screenshot file could not be completed. It may contain an incomplete image; no successful artifact is claimed. Check local storage before choosing a new path.",
            1,
        )
    })?;
    Ok(saved)
}

fn invalid() -> Failure {
    Failure::new(
        "observation_artifact_invalid",
        "The native image dimensions or pixel length exceed the screenshot contract. No screenshot file was opened.",
        1,
    )
}
fn allocation() -> Failure {
    Failure::new(
        "observation_artifact_resources",
        "The bounded PNG image could not be allocated. No screenshot file was opened.",
        1,
    )
}

/// PNG RGBA8 with filter None and zlib stored blocks. Pixel conversion writes
/// directly into the single final allocation, without a second filtered raster.
fn encode(width: u32, height: u32, bgra: &[u8]) -> Result<Vec<u8>, Failure> {
    ProtocolLimits::ABSOLUTE
        .validate_coded_dimensions(width, height)
        .map_err(|_| invalid())?;
    let stride = usize::try_from(width)
        .ok()
        .and_then(|n| n.checked_mul(4))
        .ok_or_else(invalid)?;
    let rows = usize::try_from(height).map_err(|_| invalid())?;
    let pixels = stride.checked_mul(rows).ok_or_else(invalid)?;
    if pixels > MAX_PIXEL_BYTES || pixels != bgra.len() {
        return Err(invalid());
    }
    let filtered = stride
        .checked_add(1)
        .and_then(|n| n.checked_mul(rows))
        .ok_or_else(invalid)?;
    let zlib = filtered
        .checked_add(filtered.div_ceil(BLOCK_BYTES) * 5 + 6)
        .ok_or_else(invalid)?;
    let length = zlib
        .checked_add(57)
        .filter(|n| *n <= MAX_PNG_BYTES)
        .ok_or_else(invalid)?;
    let mut png = Vec::new();
    png.try_reserve_exact(length).map_err(|_| allocation())?;
    if png.capacity() > MAX_PNG_BYTES {
        return Err(allocation());
    }
    png.extend_from_slice(&SIGNATURE);
    let mut ihdr = [0; 13];
    ihdr[..4].copy_from_slice(&width.to_be_bytes());
    ihdr[4..8].copy_from_slice(&height.to_be_bytes());
    ihdr[8] = 8;
    ihdr[9] = 6;
    chunk(&mut png, *b"IHDR", &ihdr)?;
    png.extend_from_slice(&u32::try_from(zlib).map_err(|_| invalid())?.to_be_bytes());
    let idat = png.len();
    png.extend_from_slice(b"IDAT");
    png.extend_from_slice(&[0x78, 0x01]);
    let mut raw = bgra.chunks_exact(stride).flat_map(|row| {
        std::iter::once(0).chain(
            row.as_chunks::<4>()
                .0
                .iter()
                .flat_map(|p| [p[2], p[1], p[0], p[3]]),
        )
    });
    let mut remaining = filtered;
    let (mut a, mut b) = (1_u32, 0_u32);
    while remaining != 0 {
        let count = remaining.min(BLOCK_BYTES);
        png.push(u8::from(count == remaining));
        let count16 = u16::try_from(count).map_err(|_| invalid())?;
        png.extend_from_slice(&count16.to_le_bytes());
        png.extend_from_slice(&(!count16).to_le_bytes());
        for byte in raw.by_ref().take(count) {
            png.push(byte);
            a += u32::from(byte);
            if a >= 65_521 {
                a -= 65_521;
            }
            b += a;
            if b >= 65_521 {
                b -= 65_521;
            }
        }
        remaining -= count;
    }
    png.extend_from_slice(&((b << 16) | a).to_be_bytes());
    let crc = crc32(&png[idat..]);
    png.extend_from_slice(&crc.to_be_bytes());
    chunk(&mut png, *b"IEND", &[])?;
    if png.len() != length {
        return Err(invalid());
    }
    Ok(png)
}

fn chunk(png: &mut Vec<u8>, kind: [u8; 4], data: &[u8]) -> Result<(), Failure> {
    png.extend_from_slice(
        &u32::try_from(data.len())
            .map_err(|_| invalid())?
            .to_be_bytes(),
    );
    let start = png.len();
    png.extend_from_slice(&kind);
    png.extend_from_slice(data);
    let crc = crc32(&png[start..]);
    png.extend_from_slice(&crc.to_be_bytes());
    Ok(())
}

fn crc32(bytes: &[u8]) -> u32 {
    const TABLE: [u32; 256] = {
        let mut values = [0; 256];
        let mut n = 0;
        let mut value = 0_u32;
        while n < 256 {
            let mut crc = value;
            let mut bit = 0;
            while bit < 8 {
                crc = if crc & 1 == 0 {
                    crc >> 1
                } else {
                    (crc >> 1) ^ 0xedb8_8320
                };
                bit += 1;
            }
            values[n] = crc;
            n += 1;
            value += 1;
        }
        values
    };
    let mut crc = u32::MAX;
    for byte in bytes {
        let index = usize::from((crc ^ u32::from(*byte)).to_le_bytes()[0]);
        crc = (crc >> 8) ^ TABLE[index];
    }
    !crc
}

#[cfg(test)]
#[path = "artifact_tests.rs"]
mod tests;
