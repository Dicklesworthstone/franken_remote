//! `FrankenRemote` host broker subsystem (plan sections 5.1, 5.2, 19.2, 21.1).
//!
//! Owns tailnet listeners, configuration, peer identity cache, session registry,
//! browser asset serving, process-role family capability enforcement, local IPC,
//! and the idle envelope measurement harness.

pub mod browser_assets;
pub mod config;
pub mod idle_harness;
pub mod ipc;
pub mod peer_cache;
pub mod process_role;
pub mod service;
pub mod session_registry;

pub use browser_assets::{AssetError, AssetResponse, BrowserAssets, STRICT_CSP};
pub use config::{
    ApprovalMode, AudioSettings, ConfigError, DaemonConfig, DesktopSelection, SharingScope,
    TransferSettings,
};
pub use idle_harness::{
    BrokerStateInspect, IdleEnvelopeViolation, IdleHarness, IdleMeasurementReport,
    IdleStateInspector, MAX_IDLE_CPU_PERCENT, MAX_IDLE_RSS_BYTES,
};
pub use ipc::{
    HEADER_BYTES, IPC_MAGIC, IPC_VERSION, IpcError, IpcHeader, IpcMessage, IpcMessageKind,
    MAX_IPC_PAYLOAD_BYTES, SocketCredentials, verify_incoming_message,
};
pub use peer_cache::{PeerCacheError, PeerIdentity, PeerIdentityCache};
pub use process_role::{ProcessGeneration, ProcessRole, RoleCapability};
pub use service::{BrokerService, DesktopAvailability, DesktopUnavailableReason};
pub use session_registry::{
    MediaReadinessState, RegistryError, SessionRecord, SessionRegistry, TeardownPlan,
};
