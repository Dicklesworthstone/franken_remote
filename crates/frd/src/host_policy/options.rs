//! Strict local command parsing and startup resolution. No filesystem mutation.
use super::{Approval, Change, Error, Policy, Sharing, Store, default_path};
use std::path::PathBuf;

/// Effective startup options; overrides never modify the saved policy.
#[derive(Debug)]
pub struct Effective {
    pub saved: Policy,
    pub approval: Approval,
    pub sharing: Sharing,
    pub port: u16,
}

#[derive(Debug, Default)]
pub struct RunOptions {
    pub port: Option<u16>,
    pub socket: Option<PathBuf>,
    pub config: Option<PathBuf>,
    pub approval: Option<Approval>,
    pub sharing: Option<Sharing>,
    pub headless: bool,
    /// Absolute path of the capture worker; defaults next to the frd binary.
    pub worker: Option<PathBuf>,
    /// X11 display to share; defaults to the DISPLAY frd was started with.
    pub display: Option<String>,
    /// Tailscale interface for ingress enforcement; defaults to tailscale0.
    pub interface: Option<String>,
    /// Local PEM CA bundle for the host's own certificate chain.
    pub trust_roots: Option<PathBuf>,
    /// Serve one OS-share lifetime, then exit.
    pub once: bool,
}
impl RunOptions {
    /// Arguments after `run`. Unknown, duplicate or valueless options refuse
    /// BEFORE the runtime or `LocalAPI` is opened. Typos cannot disable approval.
    pub fn parse(args: &[String]) -> Result<Self, Error> {
        let mut options = Self::default();
        let mut iter = args.iter();
        let mut json = false;
        while let Some(flag) = iter.next() {
            if flag == "--json" {
                if json {
                    return Err(Error::InvalidArgument);
                }
                json = true;
                continue;
            }
            if flag == "--headless" {
                if options.headless {
                    return Err(Error::InvalidArgument);
                }
                options.headless = true;
                continue;
            }
            if flag == "--once" {
                if options.once {
                    return Err(Error::InvalidArgument);
                }
                options.once = true;
                continue;
            }
            let value = value(&mut iter)?;
            match flag.as_str() {
                "--port" => {
                    let port = value.parse::<u16>().map_err(|_| Error::InvalidArgument)?;
                    if port == 0 {
                        return Err(Error::InvalidArgument);
                    }
                    set(&mut options.port, port)?;
                }
                "--socket" => set(&mut options.socket, PathBuf::from(value))?,
                "--config" => set(&mut options.config, PathBuf::from(value))?,
                "--approval" => set(&mut options.approval, Approval::parse(value)?)?,
                "--sharing" => set(&mut options.sharing, Sharing::parse(value)?)?,
                "--worker" => set(&mut options.worker, PathBuf::from(value))?,
                "--display" => set(&mut options.display, value.to_owned())?,
                "--interface" => set(&mut options.interface, value.to_owned())?,
                "--trust-roots" => set(&mut options.trust_roots, PathBuf::from(value))?,
                _ => return Err(Error::InvalidArgument),
            }
        }
        Ok(options)
    }
    pub fn resolve(&self) -> Result<Effective, Error> {
        if self.port == Some(0) {
            return Err(Error::InvalidArgument);
        }
        let path = match &self.config {
            Some(p) => p.clone(),
            None => default_path()?,
        };
        let saved = Store::new(&path)?.load()?;
        Ok(Effective {
            saved,
            approval: self.approval.unwrap_or(saved.approval_mode),
            sharing: self.sharing.unwrap_or(saved.sharing_scope),
            port: self.port.unwrap_or(8443),
        })
    }
}

pub struct PolicyCommand {
    pub path: PathBuf,
    pub change: Option<Change>,
}
impl PolicyCommand {
    /// Arguments after approval/sharing. `unattended` remains an accepted CLI
    /// alias for `none`; disk serialization has only the canonical spelling.
    pub fn parse(args: &[String], approval: bool) -> Result<Self, Error> {
        let mut iter = args.iter();
        let mut positionals = Vec::new();
        let mut path = None;
        let mut json = false;
        while let Some(arg) = iter.next() {
            match arg.as_str() {
                "--json" if !json => json = true,
                "--config" => set(&mut path, PathBuf::from(value(&mut iter)?))?,
                other if !other.starts_with('-') && positionals.len() < 2 => {
                    positionals.push(other);
                }
                _ => return Err(Error::InvalidArgument),
            }
        }
        let change = match positionals.as_slice() {
            [] | ["get"] => None,
            ["set", mode] if approval => Some(Change::Approval(Approval::parse(mode)?)),
            ["set", scope] => Some(Change::Sharing(Sharing::parse(scope)?)),
            _ => return Err(Error::InvalidArgument),
        };
        Ok(Self {
            path: match path {
                Some(p) => p,
                None => default_path()?,
            },
            change,
        })
    }
}
fn set<T>(slot: &mut Option<T>, value: T) -> Result<(), Error> {
    if slot.is_some() {
        return Err(Error::InvalidArgument);
    }
    *slot = Some(value);
    Ok(())
}
fn value<'a>(iter: &mut std::slice::Iter<'a, String>) -> Result<&'a str, Error> {
    iter.next()
        .map(String::as_str)
        .filter(|s| !s.is_empty() && !s.starts_with('-'))
        .ok_or(Error::InvalidArgument)
}
