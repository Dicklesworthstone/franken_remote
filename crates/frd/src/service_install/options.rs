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
