//! Hybrid GPU qualification, adapter pairing, and hardware HEVC encoder selection for Windows (plan §8.3, §10.3).
//!
//! Enforces:
//! 1. Qualifying capture device against the actual display adapter (hybrid GPUs where display is wired to iGPU).
//! 2. Cross-adapter surface sharing strategies (`ZeroCopySameAdapter`, `SharedNtHandle`, `StagingCpuCopy`).
//! 3. Probing `FFmpeg` hardware encoders (`NVENC`, `AMF`, `QSV`) on the matching or paired adapter.
//! 4. Selecting optimal pairing to avoid high-cost bus transfers across discrete/integrated GPUs.

use std::fmt;

/// Locally unique identifier (LUID) identifying a DXGI adapter on Windows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct AdapterLuid {
    pub low_part: u32,
    pub high_part: i32,
}

impl fmt::Display for AdapterLuid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:08X}:{:08X}", self.high_part, self.low_part)
    }
}

/// GPU hardware vendor classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GpuVendor {
    Nvidia,
    Amd,
    Intel,
    MicrosoftBasicRender,
    Other(u32),
}

impl GpuVendor {
    /// Classify vendor by PCI Vendor ID.
    #[must_use]
    pub const fn from_vendor_id(vendor_id: u32) -> Self {
        match vendor_id {
            0x10DE => Self::Nvidia,
            0x1002 | 0x1022 => Self::Amd,
            0x8086 => Self::Intel,
            0x1414 => Self::MicrosoftBasicRender,
            other => Self::Other(other),
        }
    }
}

/// Hardware HEVC encoder capabilities supported on Windows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HardwareEncoderKind {
    #[default]
    None,
    /// NVIDIA NVENC hardware HEVC encoder.
    Nvenc,
    /// AMD AMF hardware HEVC encoder.
    Amf,
    /// Intel Quick Sync Video (QSV) hardware HEVC encoder.
    Qsv,
}

/// Metadata and capabilities of a probed DXGI graphics adapter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdapterDesc {
    pub index: u32,
    pub luid: AdapterLuid,
    pub description: String,
    pub vendor: GpuVendor,
    pub vendor_id: u32,
    pub device_id: u32,
    pub dedicated_video_memory_bytes: u64,
    pub shared_system_memory_bytes: u64,
    pub output_count: u32,
    pub is_software: bool,
    pub supported_encoder: HardwareEncoderKind,
}

/// Cross-adapter texture transfer strategy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrossAdapterStrategy {
    /// Capture and encode occur on the exact same GPU adapter; zero-copy texture sharing.
    ZeroCopySameAdapter,
    /// D3D11 shared NT handle (`D3D11_RESOURCE_MISC_SHARED_NTHANDLE`) cross-adapter texture sharing.
    SharedNtHandle,
    /// Staging CPU copy fallback when direct cross-adapter hardware sharing is unavailable.
    StagingCpuCopy,
}

/// Selected pairing between capture adapter and encode adapter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdapterPairing {
    pub capture_adapter: AdapterLuid,
    pub encode_adapter: AdapterLuid,
    pub strategy: CrossAdapterStrategy,
    pub encoder_kind: HardwareEncoderKind,
    pub estimated_copy_overhead_micros: u64,
}

/// Topology of graphics adapters on the host system.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HybridTopology {
    /// Single GPU handles both display outputs and video encoding.
    SingleGpu,
    /// Muxless hybrid: integrated GPU drives display outputs; discrete GPU provides high-power encoding.
    MuxlessHybrid,
    /// Multi-GPU system with independent display outputs on each card.
    MultiGpuDiscrete,
}

/// Selector and evaluator for hybrid GPU environments on Windows.
pub struct HybridGpuSelector;

impl HybridGpuSelector {
    /// Determine topology from the list of enumerated adapters.
    #[must_use]
    pub fn detect_topology(adapters: &[AdapterDesc]) -> HybridTopology {
        let hardware_adapters: Vec<&AdapterDesc> =
            adapters.iter().filter(|a| !a.is_software).collect();

        if hardware_adapters.len() <= 1 {
            return HybridTopology::SingleGpu;
        }

        let has_igpu_with_outputs = hardware_adapters
            .iter()
            .any(|a| matches!(a.vendor, GpuVendor::Intel | GpuVendor::Amd) && a.output_count > 0);
        let has_dgpu_encoder = hardware_adapters.iter().any(|a| {
            matches!(a.vendor, GpuVendor::Nvidia | GpuVendor::Amd)
                && a.supported_encoder != HardwareEncoderKind::None
        });

        if has_igpu_with_outputs && has_dgpu_encoder {
            HybridTopology::MuxlessHybrid
        } else {
            HybridTopology::MultiGpuDiscrete
        }
    }

    /// Select optimal adapter pairing for a target output display.
    #[must_use]
    pub fn select_optimal_pairing(
        adapters: &[AdapterDesc],
        target_output_index: u32,
    ) -> Option<AdapterPairing> {
        // 1. Find adapter owning target output
        let capture_adapter = adapters
            .iter()
            .find(|a| a.output_count > target_output_index)?;

        // 2. Check if capture adapter itself has a qualified hardware encoder
        if capture_adapter.supported_encoder != HardwareEncoderKind::None {
            return Some(AdapterPairing {
                capture_adapter: capture_adapter.luid,
                encode_adapter: capture_adapter.luid,
                strategy: CrossAdapterStrategy::ZeroCopySameAdapter,
                encoder_kind: capture_adapter.supported_encoder,
                estimated_copy_overhead_micros: 0,
            });
        }

        // 3. Otherwise find a discrete GPU with hardware encoder support
        let dgpu = adapters.iter().find(|a| {
            !a.is_software
                && a.luid != capture_adapter.luid
                && a.supported_encoder != HardwareEncoderKind::None
        });

        if let Some(dgpu_adapter) = dgpu {
            Some(AdapterPairing {
                capture_adapter: capture_adapter.luid,
                encode_adapter: dgpu_adapter.luid,
                strategy: CrossAdapterStrategy::SharedNtHandle,
                encoder_kind: dgpu_adapter.supported_encoder,
                estimated_copy_overhead_micros: 450, // ~0.45ms for cross-adapter GPU copy
            })
        } else {
            // Software fallback on the same adapter
            Some(AdapterPairing {
                capture_adapter: capture_adapter.luid,
                encode_adapter: capture_adapter.luid,
                strategy: CrossAdapterStrategy::ZeroCopySameAdapter,
                encoder_kind: HardwareEncoderKind::None,
                estimated_copy_overhead_micros: 0,
            })
        }
    }
}
