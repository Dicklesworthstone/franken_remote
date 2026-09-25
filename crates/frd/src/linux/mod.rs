//! Linux host adapters that remain after the 2026-09-24 removal of the Wayland
//! portal, `PipeWire` and EIS models (none were wired to a real portal, `PipeWire`
//! or libei). `frd run` hosts X11 through the fr-native capture worker; these
//! modules are not on that path.

pub mod systemd_session;
pub mod x11;

pub use systemd_session::{GraphicalSessionEnvironment, SessionEnvironmentError, SessionType};

pub use x11::{
    PlatformSecurityModel, PlatformSecurityReport, RecordingX11Poster, X11EventPoster,
    X11InputSink, X11RecordedEvent,
};
