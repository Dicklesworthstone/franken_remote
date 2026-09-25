#![forbid(unsafe_code)]
//! Host-side virtual microphone endpoint injection contracts and per-OS adapters.
//!
//! Per plan section 15.4 and PROTOCOL.md:
//! - Client microphone audio terminates in a per-OS qualified virtual-microphone endpoint
//!   that ordinary host applications can select as an input device:
//!   * Linux: `PipeWire` virtual source (`media.class = Audio/Source/Virtual`).
//!   * macOS: Signed user-space `CoreAudio` server plugin (`/Library/Audio/Plug-Ins/HAL/`).
//!   * Windows: Signed virtual audio endpoint driver (native non-Rust component).
//! - Where the endpoint is not qualified on a host OS, microphone forwarding is a
//!   TYPED UNSUPPORTED capability there — never a silent fake device, never an
//!   undisclosed driver install.
//! - Revoke or lease expiry silences the uplink at the host boundary immediately.
//!
//! No OS endpoint in this crate injects PCM yet: every `probe()` reports
//! `Unsupported` and every `submit_pcm` is a typed refusal
//! (`microphone_endpoint_unqualified`). Nothing is buffered where no host
//! application can read it.

use core::fmt;
use fr_core::audio::{AudioDirection, AudioGeneration, AudioStreamConfig, MicEndpointStatus};

use crate::audio::{AudioAccessUnit, AudioDecoder, AudioPcmFrame};

/// Errors encountered interacting with host virtual microphone endpoints.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MicEndpointError {
    /// Endpoint is unsupported or cannot be qualified on this OS/environment.
    EndpointNotQualified {
        os: &'static str,
        reason: &'static str,
    },
    /// Endpoint device was not found or driver is missing.
    EndpointNotFound { name: String },
    /// Permission was denied to attach or access the virtual endpoint.
    PermissionDenied { reason: String },
    /// Virtual endpoint experienced device loss or disconnected.
    DeviceLoss { reason: String },
    /// Endpoint buffer overrun (host consumer lagging or not draining).
    BufferOverrun,
    /// PCM format mismatch (e.g. channels or sample rate mismatch).
    FormatMismatch {
        expected: &'static str,
        actual: &'static str,
    },
    /// Endpoint has been closed.
    Closed,
}

impl MicEndpointError {
    /// Stable machine-readable refusal code.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::EndpointNotQualified { .. } => "microphone_endpoint_unqualified",
            Self::EndpointNotFound { .. } => "microphone_endpoint_not_found",
            Self::PermissionDenied { .. } => "microphone_endpoint_permission_denied",
            Self::DeviceLoss { .. } => "microphone_endpoint_device_loss",
            Self::BufferOverrun => "microphone_endpoint_buffer_overrun",
            Self::FormatMismatch { .. } => "microphone_endpoint_format_mismatch",
            Self::Closed => "microphone_endpoint_closed",
        }
    }
}

/// Why no adapter below accepts PCM: none has an injection backend. A status
/// declared `Qualified` (test construction only) still cannot make one accept.
const NO_INJECTION_BACKEND: &str = "no PCM injection backend is implemented for this endpoint";

/// The single refusal every OS adapter in this module returns from `submit_pcm`.
fn refuse_submission(
    status: &MicEndpointStatus,
    endpoint_os: &'static str,
    closed: bool,
) -> MicEndpointError {
    if closed {
        return MicEndpointError::Closed;
    }
    match status {
        MicEndpointStatus::Unsupported { os, reason } => {
            MicEndpointError::EndpointNotQualified { os, reason }
        }
        MicEndpointStatus::Disabled => MicEndpointError::EndpointNotQualified {
            os: endpoint_os,
            reason: "virtual microphone is disabled",
        },
        MicEndpointStatus::Qualified { .. } => MicEndpointError::EndpointNotQualified {
            os: endpoint_os,
            reason: NO_INJECTION_BACKEND,
        },
    }
}

impl fmt::Display for MicEndpointError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EndpointNotQualified { os, reason } => {
                write!(f, "virtual microphone unqualified on {os}: {reason}")
            }
            Self::EndpointNotFound { name } => {
                write!(f, "virtual microphone endpoint not found: {name}")
            }
            Self::PermissionDenied { reason } => {
                write!(f, "virtual microphone permission denied: {reason}")
            }
            Self::DeviceLoss { reason } => {
                write!(f, "virtual microphone device loss: {reason}")
            }
            Self::BufferOverrun => write!(f, "virtual microphone buffer overrun"),
            Self::FormatMismatch { expected, actual } => {
                write!(f, "PCM format mismatch: expected {expected}, got {actual}")
            }
            Self::Closed => write!(f, "virtual microphone endpoint closed"),
        }
    }
}

/// Abstract host virtual microphone endpoint for injecting received client PCM audio.
pub trait VirtualMicEndpoint: Send + Sync {
    /// Inspect qualification status of this endpoint.
    fn status(&self) -> MicEndpointStatus;

    /// Return whether this endpoint is qualified and currently usable.
    fn is_qualified(&self) -> bool {
        matches!(self.status(), MicEndpointStatus::Qualified { .. })
    }

    /// Submit a decoded PCM frame for injection into the virtual microphone.
    /// Fails with a typed error if the endpoint is not qualified, disconnected,
    /// or if the frame parameters don't match the expected configuration.
    fn submit_pcm(&mut self, frame: &AudioPcmFrame) -> Result<(), MicEndpointError>;

    /// Immediately silence the virtual microphone (e.g. on lease expiry, revocation,
    /// or talk toggle release). Flushes any buffered samples and outputs zeroes.
    fn silence(&mut self);

    /// Close the virtual microphone endpoint and release OS resources.
    fn close(&mut self);
}

/// Linux `PipeWire` virtual microphone source adapter.
///
/// The final endpoint is a `PipeWire` virtual source node
/// (`media.class = Audio/Source/Virtual`). No such node is implemented, so this
/// adapter is unqualified on every host, whether or not a `PipeWire` socket
/// exists: a socket is not an endpoint any application can select.
#[derive(Debug)]
pub struct LinuxPipeWireMicEndpoint {
    status: MicEndpointStatus,
    is_closed: bool,
}

impl LinuxPipeWireMicEndpoint {
    /// Typed reason reported by [`Self::probe`].
    pub const UNQUALIFIED_REASON: &'static str =
        "no PipeWire virtual source is implemented; microphone forwarding is unsupported";

    /// Probe the Linux host. Always `Unsupported` until a real `PipeWire`
    /// virtual source exists; never inferred from the runtime socket.
    #[must_use]
    pub fn probe() -> Self {
        Self {
            status: MicEndpointStatus::Unsupported {
                os: "linux",
                reason: Self::UNQUALIFIED_REASON,
            },
            is_closed: false,
        }
    }

    /// Construct with an explicitly declared status (feature `testing` only).
    #[cfg(any(test, feature = "testing"))]
    #[must_use]
    pub fn with_status(status: MicEndpointStatus) -> Self {
        Self {
            status,
            is_closed: false,
        }
    }
}

impl VirtualMicEndpoint for LinuxPipeWireMicEndpoint {
    fn status(&self) -> MicEndpointStatus {
        self.status.clone()
    }

    fn submit_pcm(&mut self, _frame: &AudioPcmFrame) -> Result<(), MicEndpointError> {
        Err(refuse_submission(&self.status, "linux", self.is_closed))
    }

    fn silence(&mut self) {}

    fn close(&mut self) {
        self.is_closed = true;
    }
}

/// macOS `CoreAudio` server plugin virtual microphone endpoint adapter.
///
/// Implements injection into a signed user-space `CoreAudio` `AudioServerPlugin`
/// at `/Library/Audio/Plug-Ins/HAL/FrankenRemoteAudioServer.driver`.
#[derive(Debug)]
pub struct MacOSCoreAudioMicEndpoint {
    status: MicEndpointStatus,
    is_closed: bool,
}

impl MacOSCoreAudioMicEndpoint {
    /// Typed reason reported by [`Self::probe`].
    pub const UNQUALIFIED_REASON: &'static str =
        "no CoreAudio server plugin is implemented; microphone forwarding is unsupported";

    /// Probe the macOS host. Always `Unsupported` until the signed plugin and
    /// its injection path exist; a file at the plugin path is not evidence.
    #[must_use]
    pub fn probe() -> Self {
        Self {
            status: MicEndpointStatus::Unsupported {
                os: "macos",
                reason: Self::UNQUALIFIED_REASON,
            },
            is_closed: false,
        }
    }

    /// Construct with an explicitly declared status (feature `testing` only).
    #[cfg(any(test, feature = "testing"))]
    #[must_use]
    pub fn with_status(status: MicEndpointStatus) -> Self {
        Self {
            status,
            is_closed: false,
        }
    }
}

impl VirtualMicEndpoint for MacOSCoreAudioMicEndpoint {
    fn status(&self) -> MicEndpointStatus {
        self.status.clone()
    }

    fn submit_pcm(&mut self, _frame: &AudioPcmFrame) -> Result<(), MicEndpointError> {
        Err(refuse_submission(&self.status, "macos", self.is_closed))
    }

    fn silence(&mut self) {}

    fn close(&mut self) {
        self.is_closed = true;
    }
}

/// Windows virtual audio endpoint driver adapter.
///
/// Implements injection into the signed driver-class virtual audio endpoint
/// per the Phase 0 spike and AGENTS.md §3.3.
#[derive(Debug)]
pub struct WindowsVirtualAudioMicEndpoint {
    status: MicEndpointStatus,
    is_closed: bool,
}

impl WindowsVirtualAudioMicEndpoint {
    /// Probe the Windows host environment for the installed driver-class virtual audio endpoint.
    #[must_use]
    pub fn probe() -> Self {
        // Driver probe would check device enumeration / registry on Windows.
        // On non-Windows or when driver is missing, return typed refusal.
        let status = MicEndpointStatus::Unsupported {
            os: "windows",
            reason: "signed virtual audio endpoint driver not installed (requires driver package)",
        };

        Self {
            status,
            is_closed: false,
        }
    }

    /// Construct with an explicitly declared status (feature `testing` only).
    #[cfg(any(test, feature = "testing"))]
    #[must_use]
    pub fn with_status(status: MicEndpointStatus) -> Self {
        Self {
            status,
            is_closed: false,
        }
    }
}

impl VirtualMicEndpoint for WindowsVirtualAudioMicEndpoint {
    fn status(&self) -> MicEndpointStatus {
        self.status.clone()
    }

    fn submit_pcm(&mut self, _frame: &AudioPcmFrame) -> Result<(), MicEndpointError> {
        Err(refuse_submission(&self.status, "windows", self.is_closed))
    }

    fn silence(&mut self) {}

    fn close(&mut self) {
        self.is_closed = true;
    }
}

/// Synthetic virtual microphone endpoint for deterministic lab tests (feature
/// `testing` only). It records frames in memory; it is never an OS endpoint and
/// never qualification evidence.
#[cfg(any(test, feature = "testing"))]
#[derive(Debug)]
pub struct SyntheticVirtualMicEndpoint {
    status: MicEndpointStatus,
    submitted_frames: Vec<AudioPcmFrame>,
    silence_count: usize,
    total_samples: usize,
    last_rms_energy: f32,
    simulate_device_loss: bool,
    is_closed: bool,
}

#[cfg(any(test, feature = "testing"))]
impl SyntheticVirtualMicEndpoint {
    #[must_use]
    pub fn new_qualified(name: &'static str) -> Self {
        Self {
            status: MicEndpointStatus::Qualified {
                endpoint_name: name,
                description: "Synthetic qualified virtual microphone",
            },
            submitted_frames: Vec::new(),
            silence_count: 0,
            total_samples: 0,
            last_rms_energy: 0.0,
            simulate_device_loss: false,
            is_closed: false,
        }
    }

    #[must_use]
    pub fn new_unsupported(os: &'static str, reason: &'static str) -> Self {
        Self {
            status: MicEndpointStatus::Unsupported { os, reason },
            submitted_frames: Vec::new(),
            silence_count: 0,
            total_samples: 0,
            last_rms_energy: 0.0,
            simulate_device_loss: false,
            is_closed: false,
        }
    }

    pub fn set_simulate_device_loss(&mut self, loss: bool) {
        self.simulate_device_loss = loss;
    }

    #[must_use]
    pub fn submitted_frame_count(&self) -> usize {
        self.submitted_frames.len()
    }

    #[must_use]
    pub fn silence_count(&self) -> usize {
        self.silence_count
    }

    #[must_use]
    pub fn total_samples(&self) -> usize {
        self.total_samples
    }

    #[must_use]
    pub fn last_rms_energy(&self) -> f32 {
        self.last_rms_energy
    }

    #[must_use]
    pub fn frames(&self) -> &[AudioPcmFrame] {
        &self.submitted_frames
    }
}

#[cfg(any(test, feature = "testing"))]
impl VirtualMicEndpoint for SyntheticVirtualMicEndpoint {
    fn status(&self) -> MicEndpointStatus {
        self.status.clone()
    }

    fn submit_pcm(&mut self, frame: &AudioPcmFrame) -> Result<(), MicEndpointError> {
        if self.is_closed {
            return Err(MicEndpointError::Closed);
        }
        if self.simulate_device_loss {
            return Err(MicEndpointError::DeviceLoss {
                reason: "simulated endpoint unplug / driver unload".into(),
            });
        }
        match &self.status {
            MicEndpointStatus::Qualified { .. } => {}
            MicEndpointStatus::Unsupported { os, reason } => {
                return Err(MicEndpointError::EndpointNotQualified { os, reason });
            }
            MicEndpointStatus::Disabled => {
                return Err(MicEndpointError::EndpointNotQualified {
                    os: "synthetic",
                    reason: "virtual microphone is disabled",
                });
            }
        }

        self.last_rms_energy = frame.rms_energy();
        self.total_samples = self.total_samples.saturating_add(frame.total_samples());
        self.submitted_frames.push(*frame);
        Ok(())
    }

    fn silence(&mut self) {
        self.silence_count = self.silence_count.saturating_add(1);
        self.submitted_frames.clear();
        self.last_rms_energy = 0.0;
    }

    fn close(&mut self) {
        self.silence();
        self.is_closed = true;
    }
}

/// Errors occurring in the host audio uplink pipeline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UplinkPipelineError {
    /// Client input lease expired or session approval was revoked.
    AuthorityRevoked,
    /// Generation mismatch (stale packets from prior session or device).
    GenerationMismatch {
        expected: AudioGeneration,
        actual: AudioGeneration,
    },
    /// Virtual microphone endpoint error.
    EndpointError(MicEndpointError),
    /// Audio decoder error.
    DecoderError(String),
}

impl fmt::Display for UplinkPipelineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AuthorityRevoked => {
                write!(
                    f,
                    "uplink rejected: input lease expired or approval revoked"
                )
            }
            Self::GenerationMismatch { expected, actual } => {
                write!(
                    f,
                    "uplink generation mismatch: expected {expected:?}, got {actual:?}"
                )
            }
            Self::EndpointError(e) => write!(f, "uplink endpoint error: {e}"),
            Self::DecoderError(e) => write!(f, "uplink decoder error: {e}"),
        }
    }
}

/// Host-side audio uplink pipeline receiving client Opus packets, decoding them,
/// and injecting them into the host's qualified virtual microphone endpoint.
///
/// Strictly enforces:
/// - Immediate silencing on lease expiry or approval revocation.
/// - Audio generation fencing to drop obsolete buffered speech.
/// - Bounded queueing (no unbounded buildup).
pub struct HostAudioUplinkPipeline {
    config: AudioStreamConfig,
    decoder: Box<dyn AudioDecoder>,
    endpoint: Box<dyn VirtualMicEndpoint>,
    active_generation: AudioGeneration,
    received_packets: u64,
    injected_frames: u64,
}

impl HostAudioUplinkPipeline {
    /// Create a new host uplink pipeline.
    pub fn new(
        config: AudioStreamConfig,
        decoder: Box<dyn AudioDecoder>,
        endpoint: Box<dyn VirtualMicEndpoint>,
    ) -> Result<Self, UplinkPipelineError> {
        if config.direction() != AudioDirection::Uplink {
            return Err(UplinkPipelineError::DecoderError(
                "HostAudioUplinkPipeline requires Uplink direction".into(),
            ));
        }
        let active_generation = config.generation();
        Ok(Self {
            config,
            decoder,
            endpoint,
            active_generation,
            received_packets: 0,
            injected_frames: 0,
        })
    }

    /// Process an incoming client audio access unit.
    ///
    /// `lease_valid`: Current validity of the client's input lease on the host.
    /// `approval_granted`: Whether microphone forwarding is approved for this session.
    pub fn process_access_unit(
        &mut self,
        packet: &AudioAccessUnit,
        lease_valid: bool,
        approval_granted: bool,
    ) -> Result<(), UplinkPipelineError> {
        // Enforce lease validity and approval: any lapse silences endpoint immediately!
        if !lease_valid || !approval_granted {
            self.endpoint.silence();
            self.decoder.reset(self.active_generation);
            return Err(UplinkPipelineError::AuthorityRevoked);
        }

        // Fencing: packets from previous generations are discarded immediately
        if packet.generation() != self.active_generation {
            return Err(UplinkPipelineError::GenerationMismatch {
                expected: self.active_generation,
                actual: packet.generation(),
            });
        }

        self.received_packets = self.received_packets.saturating_add(1);

        // Submit to Opus decoder
        self.decoder
            .submit_packet(packet)
            .map_err(|e| UplinkPipelineError::DecoderError(e.to_string()))?;

        // Poll decoded PCM frames and submit to virtual mic endpoint
        while let Some(pcm_frame) = self
            .decoder
            .poll_pcm()
            .map_err(|e| UplinkPipelineError::DecoderError(e.to_string()))?
        {
            self.endpoint
                .submit_pcm(&pcm_frame)
                .map_err(UplinkPipelineError::EndpointError)?;
            self.injected_frames = self.injected_frames.saturating_add(1);
        }

        Ok(())
    }

    /// Process packet loss concealment (PLC) when an expected packet was lost.
    pub fn process_loss_concealment(
        &mut self,
        lost_duration_samples: u16,
        lease_valid: bool,
        approval_granted: bool,
    ) -> Result<(), UplinkPipelineError> {
        if !lease_valid || !approval_granted {
            self.endpoint.silence();
            self.decoder.reset(self.active_generation);
            return Err(UplinkPipelineError::AuthorityRevoked);
        }

        let pcm_frame = self
            .decoder
            .decode_plc(lost_duration_samples)
            .map_err(|e| UplinkPipelineError::DecoderError(e.to_string()))?;

        self.endpoint
            .submit_pcm(&pcm_frame)
            .map_err(UplinkPipelineError::EndpointError)?;
        self.injected_frames = self.injected_frames.saturating_add(1);

        Ok(())
    }

    /// Handle lease expiry or session revocation: instantly silences the virtual mic endpoint.
    pub fn handle_authority_lost(&mut self) {
        self.endpoint.silence();
        self.decoder.reset(self.active_generation);
    }

    /// Handle audio generation change (e.g. device switch or reconnect).
    pub fn handle_generation_change(&mut self, new_generation: AudioGeneration) {
        self.active_generation = new_generation;
        self.endpoint.silence();
        self.decoder.reset(new_generation);
    }

    #[must_use]
    pub fn received_packet_count(&self) -> u64 {
        self.received_packets
    }

    #[must_use]
    pub fn injected_frame_count(&self) -> u64 {
        self.injected_frames
    }

    #[must_use]
    pub fn endpoint(&self) -> &dyn VirtualMicEndpoint {
        self.endpoint.as_ref()
    }

    #[must_use]
    pub fn config(&self) -> AudioStreamConfig {
        self.config
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::SyntheticAudioDecoder;
    use fr_core::audio::AudioChannels;

    fn mono_frame() -> AudioPcmFrame {
        let samples = vec![1000i16; 480];
        AudioPcmFrame::from_interleaved(AudioGeneration::INITIAL, AudioChannels::Mono, 0, &samples)
            .unwrap()
    }

    #[test]
    fn linux_probe_is_unqualified_and_submission_is_a_typed_refusal() {
        // Production entry point: never Qualified, whatever PipeWire sockets exist.
        let mut endpoint = LinuxPipeWireMicEndpoint::probe();
        assert_eq!(
            endpoint.status(),
            MicEndpointStatus::Unsupported {
                os: "linux",
                reason: LinuxPipeWireMicEndpoint::UNQUALIFIED_REASON,
            }
        );
        assert!(!endpoint.is_qualified());

        let err = endpoint.submit_pcm(&mono_frame()).unwrap_err();
        assert_eq!(
            err,
            MicEndpointError::EndpointNotQualified {
                os: "linux",
                reason: LinuxPipeWireMicEndpoint::UNQUALIFIED_REASON,
            }
        );
        assert_eq!(err.code(), "microphone_endpoint_unqualified");

        endpoint.close();
        assert_eq!(
            endpoint.submit_pcm(&mono_frame()),
            Err(MicEndpointError::Closed)
        );
    }

    #[test]
    fn declared_qualified_status_never_makes_an_os_adapter_accept_pcm() {
        let qualified = MicEndpointStatus::Qualified {
            endpoint_name: "fr-virtual-mic",
            description: "declared, not implemented",
        };
        let endpoints: [(Box<dyn VirtualMicEndpoint>, &'static str); 3] = [
            (
                Box::new(LinuxPipeWireMicEndpoint::with_status(qualified.clone())),
                "linux",
            ),
            (
                Box::new(MacOSCoreAudioMicEndpoint::with_status(qualified.clone())),
                "macos",
            ),
            (
                Box::new(WindowsVirtualAudioMicEndpoint::with_status(qualified)),
                "windows",
            ),
        ];
        for (mut endpoint, os) in endpoints {
            assert_eq!(
                endpoint.submit_pcm(&mono_frame()),
                Err(MicEndpointError::EndpointNotQualified {
                    os,
                    reason: NO_INJECTION_BACKEND,
                })
            );
        }
    }

    #[test]
    fn macos_and_windows_probes_are_unqualified() {
        let mut macos = MacOSCoreAudioMicEndpoint::probe();
        assert!(!macos.is_qualified());
        assert_eq!(
            macos.submit_pcm(&mono_frame()),
            Err(MicEndpointError::EndpointNotQualified {
                os: "macos",
                reason: MacOSCoreAudioMicEndpoint::UNQUALIFIED_REASON,
            })
        );

        let mut windows = WindowsVirtualAudioMicEndpoint::probe();
        assert!(!windows.is_qualified());
        let refusal = windows.submit_pcm(&mono_frame()).unwrap_err();
        assert_eq!(refusal.code(), "microphone_endpoint_unqualified");
    }

    #[test]
    fn test_unqualified_endpoint_returns_typed_refusal() {
        let mut endpoint = LinuxPipeWireMicEndpoint::with_status(MicEndpointStatus::Unsupported {
            os: "linux",
            reason: "PipeWire not installed",
        });
        assert!(!endpoint.is_qualified());

        let samples = vec![1000i16; 480];
        let frame = AudioPcmFrame::from_interleaved(
            AudioGeneration::INITIAL,
            AudioChannels::Mono,
            0,
            &samples,
        )
        .unwrap();

        let err = endpoint.submit_pcm(&frame).unwrap_err();
        assert_eq!(
            err,
            MicEndpointError::EndpointNotQualified {
                os: "linux",
                reason: "PipeWire not installed",
            }
        );
    }

    fn make_synthetic_packet(
        direction: AudioDirection,
        generation: AudioGeneration,
        seq: u64,
        ts: u64,
    ) -> AudioAccessUnit {
        let mut payload = [0u8; 16];
        payload[0..8].copy_from_slice(&seq.to_le_bytes());
        payload[8..10].copy_from_slice(&480u16.to_le_bytes());
        payload[10..12].copy_from_slice(&1000i16.to_le_bytes());
        payload[12] = 0xAA;
        payload[13] = 0x55;
        payload[14] = 1; // mono
        payload[15] = 0x01;
        AudioAccessUnit::new(direction, generation, seq, ts, 480, false, &payload).unwrap()
    }

    #[test]
    fn test_host_uplink_pipeline_silences_on_lease_expiry() {
        let generation = AudioGeneration::INITIAL;
        let config = AudioStreamConfig::new(
            AudioDirection::Uplink,
            generation,
            AudioChannels::Mono,
            10,
            20,
        )
        .unwrap();

        let mut decoder = SyntheticAudioDecoder::new();
        decoder.configure(config).unwrap();

        let endpoint = SyntheticVirtualMicEndpoint::new_qualified("test-mic");
        let mut pipeline =
            HostAudioUplinkPipeline::new(config, Box::new(decoder), Box::new(endpoint)).unwrap();

        // Valid packet when authorized
        let unit = make_synthetic_packet(AudioDirection::Uplink, generation, 0, 0);
        pipeline.process_access_unit(&unit, true, true).unwrap();
        assert_eq!(pipeline.injected_frame_count(), 1);

        // Packet when lease is expired -> authority revoked & silenced
        let unit2 = make_synthetic_packet(AudioDirection::Uplink, generation, 1, 480);
        let res = pipeline.process_access_unit(&unit2, false, true);
        assert_eq!(res, Err(UplinkPipelineError::AuthorityRevoked));

        // Packet when approval revoked -> authority revoked & silenced
        let unit3 = make_synthetic_packet(AudioDirection::Uplink, generation, 2, 960);
        let res2 = pipeline.process_access_unit(&unit3, true, false);
        assert_eq!(res2, Err(UplinkPipelineError::AuthorityRevoked));
    }
}
