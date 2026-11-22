//! Exercise the real HTTP socket boundary before any browser authority exists.

use super::*;

#[test]
fn repeated_fetch_metadata_never_overrides_an_earlier_value() {
    let runtime = RuntimeBuilder::current_thread()
        .enable_platform_reactor(true)
        .build()
        .unwrap();
    runtime.block_on(async {
        let ingress = test_ingress(4096, 4);
        for (name, rejected, accepted) in [
            ("Sec-Fetch-Site", "cross-site", "same-origin"),
            ("Sec-Fetch-Mode", "navigate", "cors"),
            ("Sec-Fetch-Dest", "iframe", "empty"),
        ] {
            for (first, second) in [(rejected, accepted), (accepted, rejected), (accepted, accepted)] {
                let request = bootstrap_request("observer").replace(
                    &format!("{name}: {accepted}"),
                    &format!("{name}: {first}\r\n{}: {second}", name.to_ascii_lowercase()),
                );
                let response =
                    send_http_raw(&ingress, PEER_IP, Some(EXPECTED_HOST), request.as_bytes()).await;
                assert!(response.starts_with("HTTP/1.1 400 "), "{name}");
                assert!(!response.contains("nonce_hex"));
            }
        }
        assert_eq!(
            ingress
                .policy()
                .with_nonce_manager(|manager| manager.pending_count())
                .unwrap(),
            0
        );
    });
}

#[test]
fn repeated_websocket_fields_refuse_before_upgrade() {
    let runtime = RuntimeBuilder::current_thread()
        .enable_platform_reactor(true)
        .build()
        .unwrap();
    runtime.block_on(async {
        let ingress = test_ingress(4096, 4);
        for (name, value) in [
            ("Host", EXPECTED_HOST_HDR),
            ("Origin", EXPECTED_ORIGIN),
            ("Upgrade", "websocket"),
            ("Connection", "Upgrade"),
            ("Sec-WebSocket-Key", "dGhlIHNhbXBsZSBub25jZQ=="),
            ("Sec-WebSocket-Version", "13"),
            ("Sec-WebSocket-Extensions", "permessage-deflate"),
        ] {
            let mut request = upgrade_request("/ws/control");
            if name == "Sec-WebSocket-Extensions" {
                request =
                    request.replace("\r\n\r\n", &format!("\r\n{name}: {value}\r\n\r\n"));
            }
            request = request.replace(
                "\r\n\r\n",
                &format!("\r\n{}: {value}\r\n\r\n", name.to_ascii_lowercase()),
            );
            let response =
                send_http_raw(&ingress, PEER_IP, Some(EXPECTED_HOST), request.as_bytes()).await;
            assert!(response.starts_with("HTTP/1.1 400 "), "{name}");
            assert!(!response.contains("Sec-WebSocket-Accept"));
        }
    });
}

#[test]
fn malformed_field_bytes_and_signed_lengths_are_not_trimmed_into_valid_requests() {
    let runtime = RuntimeBuilder::current_thread()
        .enable_platform_reactor(true)
        .build()
        .unwrap();
    runtime.block_on(async {
        let ingress = test_ingress(4096, 4);
        for header in [
            "X@Header: ignored",
            "X(Header): ignored",
            "X-\u{00e9}: ignored",
            "X-Test: value\x0b",
            "X-Test: value\x0c",
            "X-Test: value\r",
            "X-Test: value\n",
            "X-Test: value\0",
        ] {
            let request = bootstrap_request("observer")
                .replace("\r\n\r\n", &format!("\r\n{header}\r\n\r\n"));
            let response =
                send_http_raw(&ingress, PEER_IP, Some(EXPECTED_HOST), request.as_bytes()).await;
            assert!(response.starts_with("HTTP/1.1 400 "));
        }
        for length in ["+19", "-0", "19\u{00a0}", "0x13"] {
            let request = bootstrap_request("observer")
                .replace("Content-Length: 19", &format!("Content-Length: {length}"));
            let response =
                send_http_raw(&ingress, PEER_IP, Some(EXPECTED_HOST), request.as_bytes()).await;
            assert!(response.starts_with("HTTP/1.1 400 "));
        }
        assert_eq!(
            ingress
                .policy()
                .with_nonce_manager(|manager| manager.pending_count())
                .unwrap(),
            0
        );
    });
}

#[test]
fn ordinary_optional_whitespace_and_case_insensitive_names_remain_usable() {
    let runtime = RuntimeBuilder::current_thread()
        .enable_platform_reactor(true)
        .build()
        .unwrap();
    runtime.block_on(async {
        let ingress = test_ingress(4096, 4);
        let request = bootstrap_request("observer")
            .replace("Host: ", "hOsT:\t ")
            .replace("Origin: ", "oRiGiN: \t")
            .replace("Sec-Fetch-Site: same-origin", "sec-fetch-site:\t same-origin \t")
            .replace("Content-Length: 19", "content-length:\t19 \t")
            .replace("\r\n\r\n", "\r\nX-Extension: one\r\nX-Extension: two\r\n\r\n");
        let response =
            send_http_raw(&ingress, PEER_IP, Some(EXPECTED_HOST), request.as_bytes()).await;
        assert!(response.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(response.contains("\"desktop_available\":false"));
        assert!(!response.contains("nonce_hex"));
    });
}

#[test]
fn non_ascii_security_fields_cannot_disguise_denied_metadata() {
    let runtime = RuntimeBuilder::current_thread()
        .enable_platform_reactor(true)
        .build()
        .unwrap();
    runtime.block_on(async {
        let ingress = test_ingress(4096, 4);
        for (original, replacement) in [
            ("Sec-Fetch-Site: same-origin", "Sec-Fetch-Site: cross-site\u{00a0}"),
            ("Sec-Fetch-Dest: empty", "Sec-Fetch-Dest: iframe\u{00a0}"),
            ("Sec-Fetch-Mode: cors", "Sec-Fetch-Mode: navigate\u{00a0}"),
        ] {
            let request = bootstrap_request("observer").replace(original, replacement);
            let response =
                send_http_raw(&ingress, PEER_IP, Some(EXPECTED_HOST), request.as_bytes()).await;
            assert!(response.starts_with("HTTP/1.1 400 "));
        }
        let request = bootstrap_request("observer").replace(
            &format!("Origin: {EXPECTED_ORIGIN}"),
            &format!("Origin: {EXPECTED_ORIGIN}\u{00a0}"),
        );
        let response =
            send_http_raw(&ingress, PEER_IP, Some(EXPECTED_HOST), request.as_bytes()).await;
        assert!(response.starts_with("HTTP/1.1 400 "));
        assert_eq!(
            ingress
                .policy()
                .with_nonce_manager(|manager| manager.pending_count())
                .unwrap(),
            0
        );
    });
}
