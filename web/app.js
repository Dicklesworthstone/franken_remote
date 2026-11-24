"use strict";
// DOM projection of the shared Rust/WASM owner. Transport or decoder callbacks
// cannot manufacture observation, source freshness, or an input grant.
(function (global) {
    const element = (id) => document.getElementById(id);
    const elConnection = element("connection-state");
    const elTransport = element("transport-badge");
    const elAuthority = element("authority-badge");
    const elFreshness = element("freshness-badge");
    const elBanner = element("degradation-banner");
    const elCanvas = element("viewport");
    const elOverlay = element("refusal-overlay");
    const btnConnect = element("btn-connect");
    const btnReqControl = element("btn-request-control");
    const btnRelControl = element("btn-release-control");
    const btnFullscreen = element("btn-fullscreen");
    const btnDisconnect = element("btn-disconnect");
    let connectionAttempt = null;
    let connecting = false;
    const btnProbe = element("btn-probe");
    const btnCancelProbe = element("btn-cancel-probe");
    const probeStatus = element("probe-status");
    const probe = global.FrankenHevcProbe
        ? new global.FrankenHevcProbe.Probe(element("probe-canvas"), showProbe)
        : null;

    function showProbe(state) {
        const stages = {
            loading: "Loading the local Rust decoder owner…",
            configuration: "Checking this HEVC configuration…",
            single_idr: "Decoding and drawing the first picture…",
            idle: "Checking that the decoder can remain idle…",
            after_idle: "Decoding a continuation after idle…",
            burst: "Checking the final picture of a burst…",
            last_picture: "Checking a delayed final picture…",
            reset_idr: "Checking a fresh decoder after reset…",
            reset_continuation: "Checking continuation after reset…",
        };
        const failures = {
            browser_https_required: "Open the host's HTTPS address to run the test.",
            browser_client_unavailable: "Install the matching browser WASM package on this host.",
            browser_client_version_mismatch: "Reload after installing the matching browser files.",
            webcodecs_unavailable: "This browser has no WebCodecs video decoder.",
            no_supported_hevc_decoder: "This browser rejected the sample's HEVC configuration.",
            hevc_probe_failed: "This browser could not check the HEVC configuration.",
            hevc_decoder_configuration_failed: "The browser could not initialize its HEVC decoder.",
            browser_probe_hidden: "The test stopped when the page became hidden.",
            browser_probe_cancelled: "The local test was cancelled.",
            browser_probe_timeout: "The test exceeded its 12-second deadline.",
            browser_probe_frame_timeout: "The decoder did not draw and release a picture within two seconds.",
            browser_probe_canvas_unavailable: "The test canvas could not be drawn or read.",
            browser_probe_blank_canvas: "The decoder did not produce the expected visible test pattern.",
            browser_probe_pixels_unchanged: "The continuation did not update the test pattern.",
            browser_probe_replay_mismatch: "Resetting the decoder did not reproduce the same test pictures.",
            browser_probe_decode_failed: "The HEVC decode test failed.",
            browser_probe_sample_invalid: "The Rust media owner refused the packaged test sample.",
            browser_probe_cleanup_incomplete: "The browser has not confirmed decoder cleanup.",
            browser_probe_clock_invalid: "The browser clock changed during the test.",
        };
        const message = state.status === "running" ? stages[state.stage]
            : state.status === "passed"
                ? `The HEVC test passed with ${state.presented} canvas presentations and ${state.released} released frames.`
                : failures[state.code] || "The local video test could not complete.";
        probeStatus.textContent = message + (state.status !== "running" && state.busy
            ? " Decoder cleanup is pending; another test is blocked until it finishes." : "");
        btnCancelProbe.disabled = !state.busy;
        btnCancelProbe.textContent = state.status === "running" ? "Cancel test" : "Retry cleanup";
        buttons();
    }

    btnProbe.addEventListener("click", () => {
        if (connecting || !probe || probe.busy) return;
        element("probe-panel").classList.remove("hidden");
        probe.start();
    });
    btnCancelProbe.addEventListener("click", () => { if (probe) probe.cancel(); });

    function badge(el, text, style) {
        el.textContent = text;
        el.className = "badge " + style;
    }

    function buttons() {
        btnConnect.disabled = connecting;
        btnProbe.disabled = connecting || !probe || probe.busy;
        btnDisconnect.disabled = !connecting;
        // This ABI has startup/refusal support only. The Rust session/media
        // bridge must supply actual observation and input operations first.
        btnReqControl.disabled = true;
        btnRelControl.disabled = true;
        btnFullscreen.disabled = true;
    }

    function clearRefusal() {
        elOverlay.classList.add("hidden");
    }

    function showRefusal(error) {
        element("refusal-title").textContent = "Session Refused";
        element("refusal-detail").textContent = error.message;
        elOverlay.classList.remove("hidden");
    }

    function project(snapshot) {
        if (!connectionAttempt) return;
        const labels = {
            bootstrapping: "Connecting...",
            awaiting_transport: "Connecting secure transport...",
            awaiting_host: "Awaiting session authorization",
            refused: "Refused",
            closed: "Disconnected",
            disconnected: "Disconnected",
        };
        badge(elConnection, labels[snapshot.state_label] || "Awaiting session authorization", "badge-neutral");
        badge(elTransport, "Transport: WSS (Degraded)", "badge-warn");
        badge(elAuthority, "Role: View-Only", "badge-neutral");
        badge(elFreshness, "View: Unverified", "badge-neutral");
        elBanner.textContent =
            "Compatibility transport uses TCP. Desktop observation and control require separate host authorization.";
        elBanner.classList.remove("hidden");
        buttons();
    }

    const client = new global.FrankenWss.FrankenWssClient({
        onState: project,
        onClosed: (reason) => closeConnection(reason),
        onRefused: (error) => {
            closeConnection(error.code, "Refused");
            showRefusal(error);
        },
    });

    function closeConnection(reason, label) {
        connectionAttempt = null;
        connecting = false;
        client.disconnect(reason);
        clearRefusal();
        badge(elConnection, label || "Disconnected", "badge-neutral");
        badge(elAuthority, "Role: View-Only", "badge-neutral");
        badge(elFreshness, "View: Unverified", "badge-neutral");
        buttons();
    }

    btnConnect.addEventListener("click", async () => {
        if (connecting) return;
        if (probe) probe.cancel();
        const attempt = {};
        connectionAttempt = attempt;
        connecting = true;
        clearRefusal();
        badge(elConnection, "Connecting...", "badge-neutral");
        buttons();
        try {
            await client.connect("observer");
            if (connectionAttempt !== attempt || client.closed) return;
            // Only the Rust owner's subsequent state can authorize observation.
            badge(elConnection, "Awaiting session authorization", "badge-neutral");
        } catch (error) {
            if (connectionAttempt !== attempt) return;
            closeConnection("connection_refused", "Refused");
            showRefusal(error);
        }
    });

    btnDisconnect.addEventListener("click", () => closeConnection("user_requested"));
    btnFullscreen.addEventListener("click", () => {
        if (!btnFullscreen.disabled && typeof elCanvas.requestFullscreen === "function") {
            elCanvas.requestFullscreen().catch(() => {});
        }
    });

    client.lifecycle(document.hidden ? "hidden" : "visible");
    if (typeof document.hasFocus === "function") {
        client.lifecycle(document.hasFocus() ? "focus_gained" : "focus_lost");
    }
    document.addEventListener("visibilitychange", () => {
        client.lifecycle(document.hidden ? "hidden" : "visible");
        if (document.hidden && probe) probe.cancel();
    });
    global.addEventListener("blur", () => client.lifecycle("focus_lost"));
    global.addEventListener("focus", () => client.lifecycle("focus_gained"));
    global.addEventListener("pagehide", () => {
        if (probe) probe.cancel();
        client.lifecycle("pagehide");
        closeConnection("pagehide");
    });
    global.addEventListener("pageshow", (event) => {
        if (!event.persisted) return;
        if (probe) probe.cancel();
        client.lifecycle("bfcache_restore");
        closeConnection("bfcache_restore");
        client.lifecycle(document.hidden ? "hidden" : "visible");
        if (typeof document.hasFocus === "function") {
            client.lifecycle(document.hasFocus() ? "focus_gained" : "focus_lost");
        }
    });
    buttons();
})(window);
