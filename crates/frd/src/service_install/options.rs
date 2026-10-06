//! Strict installer parsing. Omitted authority flags inherit saved startup
//! policy; explicit `none` and `own-user` MUST remain in the service definition.
use super::{InstallOptions, ServiceError, ServiceKind};
use std::{
    collections::BTreeSet,
    path::{Component, Path, PathBuf},
};

impl InstallOptions {
    /// Parse arguments after `install`, rejecting ambiguous requests before I/O.
    pub fn parse_cli(args: &[String]) -> Result<Self, ServiceError> {
        let mut options = Self::default();
        let mut seen = BTreeSet::new();
        let mut iter = args.iter();
        while let Some(flag) = iter.next() {
            if flag == "--" {
                // Everything after `--` is for `frd run` (validated below).
                options.run_args = iter.by_ref().cloned().collect();
                break;
            }
            let key = match flag.as_str() {
                "--user" | "--system" => "service-kind",
                other => other,
            };
            if !seen.insert(key) {
                return Err(ServiceError::InvalidOptions);
            }
            match flag.as_str() {
                "--json" => {}
                "--dry-run" => options.dry_run = true,
                "--user" => options.kind = ServiceKind::default_for_platform(),
                "--system" => options.kind = system_kind(),
                "--port" => {
                    options.service_port = value(&mut iter)?
                        .parse()
                        .map_err(|_| ServiceError::InvalidOptions)?;
                }
                "--socket" => options.socket_path = Some(PathBuf::from(value(&mut iter)?)),
                "--config" => options.config_path = Some(PathBuf::from(value(&mut iter)?)),
                "--approval" => {
                    options.approval_mode = match value(&mut iter)? {
                        "unattended" => "none".into(),
                        value => value.to_owned(),
                    }
                }
                "--sharing" => value(&mut iter)?.clone_into(&mut options.sharing_scope),
                "--software-explicit" => options.software_explicit = true,
                _ => return Err(ServiceError::InvalidOptions),
            }
        }
        options.validate()?;
        Ok(options)
    }

    /// No untrusted value is silently replaced by a more permissive default.
    /// This validates programmatic callers as well as the CLI, even for dry runs.
    pub fn validate(&self) -> Result<(), ServiceError> {
        if self.service_port == 0
            || !matches!(self.approval_mode.as_str(), "" | "local" | "none")
            || !matches!(self.sharing_scope.as_str(), "" | "own-user" | "tailnet")
        {
            return Err(ServiceError::InvalidOptions);
        }
        let encoder_selected = self.validate_run_args()?;
        if matches!(
            self.kind,
            ServiceKind::SystemdUser | ServiceKind::SystemdSystem
        ) {
            // Reject missing/contradictory selection, not a hardware choice
            // merely because the CPU legacy flag is absent. Native hardware
            // availability still has to be established by the actual worker.
            if !self.software_explicit && !encoder_selected {
                return Err(ServiceError::HostProfileUnavailable {
                    code: "hardware_hevc_unavailable",
                    detail: "frd run needs an explicit encoder: pass --software-explicit or pass -- --encoder nvenc|vaapi|software; no automatic selection or fallback",
                });
            }
            if self.approval_mode == "local" {
                return Err(ServiceError::HostProfileUnavailable {
                    code: "local_approval_unavailable",
                    detail: "frd run cannot show local approval prompts yet; install with --approval none",
                });
            }
        }
        for path in std::iter::once(&self.exec_path)
            .chain(self.socket_path.iter())
            .chain(self.config_path.iter())
            .chain(self.custom_unit_dir.iter())
        {
            validate_path(path)?;
        }
        if self.config_path.is_some()
            && !matches!(
                self.kind,
                ServiceKind::SystemdUser | ServiceKind::SystemdSystem
            )
        {
            return Err(ServiceError::UnsupportedPlatform {
                detail: "saved host policy is currently supported only by the Linux host".into(),
            });
        }
        if self.kind == ServiceKind::WindowsService
            && (self.socket_path.is_some()
                || !self.approval_mode.is_empty()
                || !self.sharing_scope.is_empty())
        {
            return Err(ServiceError::UnsupportedPlatform {
                detail: "Windows service registration cannot yet preserve these overrides".into(),
            });
        }
        Ok(())
    }
}
impl InstallOptions {
    /// Parse carried flags with the real host parser and return whether they
    /// explicitly select an encoder. The renderer retains those original args;
    /// this never inserts a software flag, changes authority policy or probes GPU
    /// availability at installation time.
    fn validate_run_args(&self) -> Result<bool, ServiceError> {
        use crate::host_policy::options::RunOptions;
        if self.run_args.is_empty() {
            return Ok(false);
        }
        if !matches!(
            self.kind,
            ServiceKind::SystemdUser | ServiceKind::SystemdSystem
        ) {
            return Err(ServiceError::UnsupportedPlatform {
                detail: "frd run options are currently supported only by the Linux host".into(),
            });
        }
        let run = RunOptions::parse(&self.run_args).map_err(|_| ServiceError::InvalidOptions)?;
        let encoder_selected = run
            .selected_encoder()
            .map_err(|_| ServiceError::InvalidOptions)?
            .is_some();
        // Set by the installer's own flags; a service never runs `--once`.
        // `--encoder` is the explicit host-side alternative to the legacy
        // installer software flag, never permission to emit both selectors.
        if run.port.is_some()
            || run.socket.is_some()
            || run.config.is_some()
            || run.approval.is_some()
            || run.sharing.is_some()
            || run.software_explicit
            || run.once
            || (self.software_explicit && encoder_selected)
        {
            return Err(ServiceError::InvalidOptions);
        }
        if run.clipboard && run.input_agent.is_none() {
            return Err(ServiceError::HostProfileUnavailable {
                code: "clipboard_requires_input_agent",
                detail: "--clipboard follows the controller's lease; it needs --input-agent",
            });
        }
        if run.files.is_some() && run.input_agent.is_none() {
            return Err(ServiceError::HostProfileUnavailable {
                code: "files_requires_input_agent",
                detail: "--files receives the controller's sends; it needs --input-agent",
            });
        }
        if self.kind == ServiceKind::SystemdSystem && !run.headless {
            return Err(ServiceError::HostProfileUnavailable {
                code: "system_service_requires_headless",
                detail: "a system unit has no user's X display; install a user unit, or pass -- --headless",
            });
        }
        for path in [
            &run.worker,
            &run.input_agent,
            &run.files,
            &run.session_monitor,
            &run.observation_indicator,
            &run.audio_server,
            &run.trust_roots,
        ]
        .into_iter()
        .flatten()
        {
            validate_path(path)?;
        }
        Ok(encoder_selected)
    }
}
fn value<'a>(iter: &mut std::slice::Iter<'a, String>) -> Result<&'a str, ServiceError> {
    iter.next()
        .map(String::as_str)
        .filter(|v| !v.is_empty() && !v.starts_with('-'))
        .ok_or(ServiceError::InvalidOptions)
}
fn validate_path(path: &Path) -> Result<(), ServiceError> {
    let text = path.to_str().ok_or(ServiceError::InvalidOptions)?;
    if !path.is_absolute()
        || text.len() > 4096
        || text.chars().any(char::is_control)
        || path.components().any(|c| matches!(c, Component::ParentDir))
    {
        return Err(ServiceError::InvalidOptions);
    }
    Ok(())
}
const fn system_kind() -> ServiceKind {
    if cfg!(target_os = "macos") {
        ServiceKind::LaunchdDaemon
    } else if cfg!(target_os = "windows") {
        ServiceKind::WindowsService
    } else {
        ServiceKind::SystemdSystem
    }
}

#[cfg(test)]
mod tests;
#[cfg(all(test, target_os = "linux"))]
mod encoder_tests;
