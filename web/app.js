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

    function badge(el, text, style) {
        el.textContent = text;
        el.className = "badge " + style;
    }

    function buttons() {
        btnConnect.disabled = connecting;
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
    });
    global.addEventListener("blur", () => client.lifecycle("focus_lost"));
    global.addEventListener("focus", () => client.lifecycle("focus_gained"));
    global.addEventListener("pagehide", () => {
        client.lifecycle("pagehide");
        closeConnection("pagehide");
    });
    global.addEventListener("pageshow", (event) => {
        if (!event.persisted) return;
        client.lifecycle("bfcache_restore");
        closeConnection("bfcache_restore");
        client.lifecycle(document.hidden ? "hidden" : "visible");
        if (typeof document.hasFocus === "function") {
            client.lifecycle(document.hasFocus() ? "focus_gained" : "focus_lost");
        }
    });
    buttons();
})(window);
