//! An explicit local probe never inherits a display or encoder choice.
use super::{BTreeSet, Command, Failure, Options, PathBuf, next_arg, next_path, next_port, usage};

pub struct DoctorOptions {
    pub port: u16,
    pub roots: Option<PathBuf>,
    pub native: Option<NativeDoctorOptions>,
}
pub struct NativeDoctorOptions {
    pub worker: PathBuf,
    pub encoder: &'static str,
    pub display: String,
    pub xauthority: Option<PathBuf>,
    pub input_agent: Option<PathBuf>,
}

pub(super) fn parse(args: &[String]) -> Result<Options, Failure> {
    let mut index = 1;
    let mut seen = BTreeSet::new();
    let (mut json, mut native) = (false, false);
    let (mut socket, mut roots, mut worker, mut xauthority, mut input_agent) =
        (None, None, None, None, None);
    let (mut encoder, mut display) = (None, None);
    let mut port = 8443;
    while index < args.len() {
        let flag = args[index].as_str();
        index += 1;
        if !seen.insert(flag) {
            return Err(usage());
        }
        match flag {
            "--json" => json = true,
            "--socket" => socket = Some(next_path(args, &mut index)?),
            "--port" => port = next_port(args, &mut index)?,
            "--trust-roots" => roots = Some(next_path(args, &mut index)?),
            "--probe-native" => native = true,
            "--worker" => worker = Some(next_path(args, &mut index)?),
            "--xauthority" => xauthority = Some(next_path(args, &mut index)?),
            "--input-agent" => input_agent = Some(next_path(args, &mut index)?),
            "--encoder" => {
                encoder = Some(match next_arg(args, &mut index)? {
                    "software" => "software",
                    "nvenc" => "nvenc",
                    "vaapi" => "vaapi",
                    _ => return Err(usage()),
                });
            }
            "--x-display" => {
                let value = next_arg(args, &mut index)?;
                let mut parts = value.strip_prefix(':').ok_or_else(usage)?.split('.');
                let valid = |part: &str| {
                    !part.is_empty()
                        && part.len() <= 5
                        && part.bytes().all(|byte| byte.is_ascii_digit())
                        && part.parse::<u16>().is_ok()
                };
                if !parts.next().is_some_and(valid)
                    || !parts.next().is_none_or(valid)
                    || parts.next().is_some()
                {
                    return Err(usage());
                }
                display = Some(value.to_string());
            }
            _ => return Err(usage()),
        }
    }
    let native = if native {
        if seen.contains("--port") || socket.is_some() || roots.is_some() {
            return Err(Failure::new(
                "native_probe_local_only",
                "Run fr doctor separately for tailnet and certificate checks; --probe-native checks only this local display and selected workers.",
                2,
            ));
        }
        let required = || {
            Failure::new(
                "native_probe_configuration_required",
                "Use --probe-native --worker /absolute/fr-media-worker --encoder software|nvenc|vaapi --x-display :N, with optional --xauthority and --input-agent absolute paths.",
                2,
            )
        };
        Some(NativeDoctorOptions {
            worker: worker.ok_or_else(required)?,
            encoder: encoder.ok_or_else(required)?,
            display: display.ok_or_else(required)?,
            xauthority,
            input_agent,
        })
    } else {
        if worker.is_some()
            || encoder.is_some()
            || display.is_some()
            || xauthority.is_some()
            || input_agent.is_some()
        {
            return Err(Failure::new(
                "native_probe_consent_required",
                "Pass --probe-native to explicitly allow one local screen capture and encode probe; ordinary fr doctor never opens the desktop.",
                2,
            ));
        }
        None
    };
    Ok(Options {
        command: Command::Doctor(DoctorOptions {
            port,
            roots,
            native,
        }),
        json,
        socket,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options(args: &str) -> Result<Options, Failure> {
        parse(
            &args
                .split_whitespace()
                .map(str::to_owned)
                .collect::<Vec<_>>(),
        )
    }

    #[test]
    fn native_probe_requires_explicit_complete_context() {
        for args in [
            "doctor --probe-native",
            "doctor --probe-native --worker /worker --encoder nvenc",
            "doctor --worker /worker --encoder nvenc --x-display :1",
            "doctor --input-agent /input",
        ] {
            assert!(options(args).is_err());
        }
        for encoder in ["software", "nvenc", "vaapi"] {
            let options = options(&format!("doctor --probe-native --worker /worker --encoder {encoder} --x-display :12.1 --xauthority /cookie --input-agent /input --json")).unwrap();
            let Command::Doctor(doctor) = options.command else {
                panic!("doctor");
            };
            let native = doctor.native.unwrap();
            assert_eq!(native.encoder, encoder);
            assert_eq!(native.display, ":12.1");
            assert_eq!(native.input_agent, Some(PathBuf::from("/input")));
            assert_eq!(native.xauthority, Some(PathBuf::from("/cookie")));
            assert!(options.json);
        }
    }

    #[test]
    fn native_probe_rejects_ambiguous_modes_paths_and_backends() {
        let base = "doctor --probe-native --worker /worker --encoder nvenc --x-display :1";
        for extra in [
            "--port 8443",
            "--socket /socket",
            "--trust-roots /roots",
            "--encoder vaapi",
            "--probe-native",
            "--control",
        ] {
            assert!(options(&format!("{base} {extra}")).is_err());
        }
        for encoder in ["auto", "h264", "Software", ""] {
            assert!(
                options(&format!(
                    "doctor --probe-native --worker /worker --x-display :1 --encoder {encoder}"
                ))
                .is_err()
            );
        }
        for display in ["remote:0", ":", ":-1", ":65536", ":1.2.3", ":1."] {
            assert!(options(&format!("doctor --probe-native --worker /worker --encoder software --x-display {display}")).is_err());
        }
        for worker in ["relative", "/a/../worker"] {
            assert!(
                options(&format!(
                    "doctor --probe-native --worker {worker} --encoder software --x-display :1"
                ))
                .is_err()
            );
        }
    }
}
