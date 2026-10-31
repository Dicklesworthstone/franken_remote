use super::*;
fn parsed(words: &str) -> Result<Options, Failure> { parse(&words.split_whitespace().map(str::to_owned).collect::<Vec<_>>()) }
#[test]
fn probe_only_options_require_both_explicit_probe_and_transport_consent() {
    let Command::Inspect(options) = parsed("inspect n-peer --json").unwrap().command else {
        panic!("inspect command");
    };
    assert!(options.probe.is_none());
    let Command::Inspect(options) =
        parsed("inspect n-peer --probe --experimental-native --trust-roots /tmp/roots.pem --ipv6")
            .unwrap()
            .command
    else {
        panic!("inspect command");
    };
    let probe = options.probe.unwrap();
    assert_eq!(probe.roots, PathBuf::from("/tmp/roots.pem"));
    assert!(probe.ipv6);
    for flags in [
        "--probe",
        "--experimental-native",
        "--trust-roots /tmp/roots.pem",
        "--ipv6",
        "--probe --experimental-native --trust-roots relative",
        "--probe --probe --experimental-native",
    ] {
        assert!(
            parsed(&format!("inspect n-peer {flags}")).is_err(),
            "{flags}"
        );
    }
    let Command::Inspect(options) = parsed("inspect n-peer --probe --experimental-native")
        .unwrap()
        .command
    else {
        panic!("inspect command");
    };
    assert_eq!(
        options.probe.unwrap().roots,
        PathBuf::from(fr_tailnet::trust::SYSTEM_BUNDLE)
    );
}
