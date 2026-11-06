//! Local microphone endpoint and privacy-surface configuration. The daemon
//! supervises installed children; it never links a codec or opens PipeWire.
use super::{Error, Options};
use crate::{input_process::ProcessLaunch, microphone_process::Launch, native_microphone};
use std::path::PathBuf;

/// Separate host enable, configured only by the local operator. Debug omits
/// server paths and native installation details.
#[derive(Clone)]
pub struct MicrophoneOptions {
    pub server: PathBuf,
    pub device: PathBuf,
    pub codec: PathBuf,
    pub indicator: PathBuf,
}
impl std::fmt::Debug for MicrophoneOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MicrophoneOptions([local endpoint and privacy surface])")
    }
}
pub(super) fn check(options: &Options) -> Result<(), Error> {
    if let Some(microphone) = &options.microphone {
        if options.input_agent.is_none() {
            return Err(Error::Configuration);
        }
        profile(options, microphone, 1)?;
    }
    Ok(())
}
pub(super) fn profile(
    options: &Options,
    microphone: &MicrophoneOptions,
    epoch: u128,
) -> Result<native_microphone::Configuration, Error> {
    let launch = Launch::new(&microphone.device, &microphone.codec, &microphone.server)
        .map_err(|_| Error::Configuration)?;
    let indicator = ProcessLaunch::new(
        &microphone.indicator,
        &options.display,
        options.xauthority.as_deref(),
        epoch,
    )
    .map_err(|_| Error::Configuration)?;
    Ok(native_microphone::Configuration::new(launch, indicator))
}
