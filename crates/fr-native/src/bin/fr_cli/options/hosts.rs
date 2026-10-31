use super::{
    BTreeSet, Command, Failure, HostsProbeOptions, InspectProbe, Options, PathBuf, next_arg,
    next_path, next_port, usage,
};
use fr_client::discovery::batch::MAX_BATCH_TARGETS;

pub(super) fn parse(args: &[String]) -> Result<Options, Failure> {
    let (mut index, mut json, mut socket) = (1, false, None);
    let (mut probe, mut experimental, mut ipv6) = (false, false, false);
    let (mut roots, mut port, mut limit) = (None, 8443, MAX_BATCH_TARGETS);
    let mut seen = BTreeSet::new();
    while index < args.len() {
        let flag = args[index].as_str();
        index += 1;
        if !seen.insert(flag) {
            return Err(usage());
        }
        match flag {
            "--json" => json = true,
            "--socket" => socket = Some(next_path(args, &mut index)?),
            "--probe" => probe = true,
            "--experimental-native" => experimental = true,
            "--trust-roots" => roots = Some(next_path(args, &mut index)?),
            "--ipv6" => ipv6 = true,
            "--port" => port = next_port(args, &mut index)?,
            "--probe-limit" => {
                limit = next_arg(args, &mut index)?.parse().map_err(|_| usage())?;
                if !(1..=MAX_BATCH_TARGETS).contains(&limit) {
                    return Err(usage());
                }
            }
            _ => return Err(usage()),
        }
    }
    if !probe
        && seen
            .iter()
            .any(|flag| !matches!(*flag, "--json" | "--socket"))
    {
        return Err(usage());
    }
    if probe && !experimental {
        return Err(Failure::new(
            "native_transport_unqualified",
            "Capability discovery opens the experimental native transport; explicitly select --experimental-native.",
            2,
        ));
    }
    Ok(Options {
        command: if probe {
            Command::ProbeHosts(HostsProbeOptions {
                port,
                limit,
                transport: InspectProbe {
                    roots: roots.unwrap_or_else(|| PathBuf::from(fr_tailnet::trust::SYSTEM_BUNDLE)),
                    ipv6,
                },
            })
        } else {
            Command::Hosts
        },
        json,
        socket,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn options(words: &str) -> Result<Options, Failure> {
        super::super::parse(
            &words
                .split_whitespace()
                .map(str::to_owned)
                .collect::<Vec<_>>(),
        )
    }
    #[test]
    fn ordinary_discovery_stays_local_and_probe_bounds_are_explicit() {
        assert!(matches!(
            options("hosts --json").unwrap().command,
            Command::Hosts
        ));
        let Command::ProbeHosts(p) = options("hosts --probe --experimental-native --probe-limit 7 --port 9443 --ipv6 --trust-roots /tmp/roots.pem --json").unwrap().command else { panic!("probe hosts"); };
        assert_eq!(p.limit, 7);
        assert_eq!(p.port, 9443);
        assert!(p.transport.ipv6);
        assert_eq!(p.transport.roots, PathBuf::from("/tmp/roots.pem"));
        let Command::ProbeHosts(p) = options("hosts --probe --experimental-native")
            .unwrap()
            .command
        else {
            panic!("probe hosts");
        };
        assert_eq!(p.limit, MAX_BATCH_TARGETS);
    }
    #[test]
    fn ambiguous_unbounded_and_implicit_network_options_refuse() {
        for flags in [
            "--probe",
            "--experimental-native",
            "--port 8443",
            "--ipv6",
            "--probe-limit 1",
            "--probe --experimental-native --probe-limit 0",
            "--probe --experimental-native --probe-limit 65",
            "--probe --experimental-native --probe-limit 999999999999999999999999",
            "--probe --experimental-native --probe-limit 2 --probe-limit 3",
            "--probe --experimental-native --trust-roots relative",
            "--probe --probe --experimental-native",
            "--probe --experimental-native --port 0",
            "--probe --experimental-native node-or-subnet",
        ] {
            assert!(options(&format!("hosts {flags}")).is_err(), "{flags}");
        }
    }
}
