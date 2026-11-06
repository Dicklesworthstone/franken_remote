use super::*;

fn capability<'a>(report: &'a Report<'_>, name: &str) -> &'a output::DoctorCapabilityReport {
    report
        .capabilities
        .iter()
        .find(|row| row.name == name)
        .unwrap()
}
fn input() -> Capabilities {
    INPUT_OPERATIONS[..4]
        .iter()
        .fold(Capabilities::default(), |caps, (capability, _)| {
            caps.with(*capability)
        })
}
fn admitted() -> EncoderProbe {
    EncoderProbe::Admitted {
        width: 1920,
        height: 1080,
    }
}

#[test]
fn successful_rows_only_claim_the_exact_probe_and_keep_other_capabilities_unknown() {
    for encoder in ["software", "nvenc", "vaapi"] {
        let report = Report::new(
            encoder,
            ":1",
            Ok(admitted()),
            Some(Ok(input().with(Capability::LineScroll))),
        );
        assert_eq!(report.refusal_code, None);
        assert_eq!(
            capability(&report, "video_encode").hardware_accelerated,
            Some(encoder != "software")
        );
        assert_eq!(capability(&report, "screen_capture").status, "passed");
        assert_eq!(
            report.input_capabilities,
            [
                "keys",
                "repeat",
                "absolute_pointer",
                "buttons",
                "line_scroll"
            ]
        );
        assert!(
            capability(&report, "input_injection")
                .detail
                .contains("no key, pointer or text was injected")
        );
        for name in [
            "video_decode",
            "clipboard_sync",
            "audio_playback",
            "audio_microphone",
            "file_transfer",
        ] {
            assert_eq!(capability(&report, name).status, "not_tested");
            assert_eq!(capability(&report, name).hardware_accelerated, None);
        }
        assert!(
            report
                .permissions
                .iter()
                .filter(|row| row.capability != "screen_capture")
                .all(|row| row.status == "not_tested")
        );
        let json: serde_json::Value = serde_json::from_str(&report.render(true).unwrap()).unwrap();
        assert_eq!(json["scope"], "local_native_probe");
        assert_eq!(json["encoder"], encoder);
        assert_eq!(json["tailnet_status"], "not_tested");
        assert_eq!(json["outcome"], "success");
        assert_eq!(json["capabilities"].as_array().unwrap().len(), 8);
        assert!(
            report
                .render(false)
                .unwrap()
                .contains("No decode, presentation, sustained-rate or remote-session measurement")
        );
    }
}

#[test]
fn combined_refusals_never_invent_capture_failure_or_echo_untrusted_details() {
    for result in [
        Ok(EncoderProbe::Refused("Unsupported".into())),
        Ok(EncoderProbe::Refused(
            "/private/path\nsecret payload".into(),
        )),
        Ok(EncoderProbe::TimedOut),
        Err(WorkerError::SpawnFailed(SpawnFailure::NotFound)),
        Err(WorkerError::SpawnFailed(SpawnFailure::PermissionDenied)),
    ] {
        let report = Report::new("nvenc", ":1", result, None);
        assert!(report.refusal_code.is_some());
        assert_eq!(capability(&report, "screen_capture").status, "not_tested");
        assert_eq!(capability(&report, "input_injection").status, "not_tested");
        assert_eq!(
            capability(&report, "video_encode").hardware_accelerated,
            None
        );
        for json in [false, true] {
            let rendered = report.render(json).unwrap();
            assert!(!rendered.contains("private/path"));
            assert!(!rendered.contains("secret payload"));
            assert!(rendered.contains(report.refusal_code.unwrap()));
        }
    }
}

#[test]
fn invalid_dimensions_and_partial_input_support_cannot_pass_readiness() {
    for (width, height) in [(0, 1), (1, 0), (u32::MAX, u32::MAX), (8192, 8192)] {
        let report = Report::new(
            "software",
            ":1",
            Ok(EncoderProbe::Admitted { width, height }),
            None,
        );
        assert_eq!(report.refusal_code, Some("native_probe_invalid_reply"));
        assert_eq!(capability(&report, "screen_capture").status, "not_tested");
    }
    let report = Report::new(
        "software",
        ":1",
        Ok(admitted()),
        Some(Ok(Capabilities::default().with(Capability::Absolute))),
    );
    assert_eq!(report.refusal_code, Some("native_input_capability_missing"));
    assert_eq!(report.input_capabilities, ["absolute_pointer"]);
    assert!(
        capability(&report, "input_injection")
            .detail
            .contains("Missing: keys, repeat, buttons")
    );
    for (error, code) in [
        (PlatformError::Unsupported, "native_input_unsupported"),
        (PlatformError::Permission, "native_input_permission_denied"),
        (
            PlatformError::GeometryChanged,
            "native_input_geometry_changed",
        ),
        (PlatformError::Unavailable, "native_input_unavailable"),
    ] {
        let report = Report::new("software", ":1", Ok(admitted()), Some(Err(error)));
        assert_eq!(report.refusal_code, Some(code));
        assert_eq!(capability(&report, "video_encode").status, "passed");
        assert!(report.input_capabilities.is_empty());
    }
}

#[test]
fn real_missing_worker_probe_returns_nonzero_without_runtime_or_tailnet() {
    let options = NativeDoctorOptions {
        worker: "/nonexistent-fr-doctor-worker/image".into(),
        encoder: "software",
        display: ":1".into(),
        xauthority: Some("/private-cookie".into()),
        input_agent: None,
    };
    let (text, exit) = run(&options, true).unwrap();
    let report: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(exit, 1);
    assert_eq!(report["refusal_code"], "native_worker_missing");
    assert_eq!(report["outcome"], "refusal");
    assert!(!text.contains("nonexistent-fr-doctor-worker"));
    assert!(!text.contains("private-cookie"));
}
