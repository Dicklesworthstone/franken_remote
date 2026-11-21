"use strict";
// FrankenRemote browser workstation controller shell.
//
// Enforces constitutional invariants (AGENTS.md §§4, 5, 6; Plan §16):
// - Observation, media readiness, and input control are strictly separate states.
//   Receiving a first decoded/presented frame NEVER grants input control.
// - Hidden tabs, pagehide, and bfcache restoration immediately suspend/revoke
//   control and require a fresh usable-view check before control can be requested.
// - Physical key identity (`KeyboardEvent.code`) is kept separate from committed text.
// - WSS degraded transport profile is explicitly surfaced in the UI.

(function (global) {
    const elConnection = document.getElementById("connection-state");
    const elTransport = document.getElementById("transport-badge");
    const elAuthority = document.getElementById("authority-badge");
    const elFreshness = document.getElementById("freshness-badge");
    const elBanner = document.getElementById("degradation-banner");
    const elCanvas = document.getElementById("viewport");
    const elOverlay = document.getElementById("refusal-overlay");
    const elOverlayTitle = document.getElementById("refusal-title");
    const elOverlayDetail = document.getElementById("refusal-detail");

    const btnConnect = document.getElementById("btn-connect");
    const btnReqControl = document.getElementById("btn-request-control");
    const btnRelControl = document.getElementById("btn-release-control");
    const btnFullscreen = document.getElementById("btn-fullscreen");
    const btnDisconnect = document.getElementById("btn-disconnect");

    const state = {
        connected: false,
        connecting: false,
        viewFresh: false,
        controlGranted: false,
        controlSuspended: false,
        generation: 0,
    };

    function setBadge(el, text, cls) {
        if (!el) return;
        el.textContent = text;
        el.className = "badge " + cls;
    }

    function showRefusal(title, detail) {
        if (!elOverlay) return;
        elOverlayTitle.textContent = title;
        elOverlayDetail.textContent = detail;
        elOverlay.classList.remove("hidden");
    }

    function clearRefusal() {
        if (!elOverlay) return;
        elOverlay.classList.add("hidden");
    }

    function syncButtons() {
        btnConnect.disabled = state.connected || state.connecting;
        btnDisconnect.disabled = !state.connected && !state.connecting;
        btnFullscreen.disabled = !state.connected;
        // Control can only be requested once connected AND view freshness is established.
        btnReqControl.disabled = !state.connected || !state.viewFresh || state.controlGranted;
        btnRelControl.disabled = !state.controlGranted;
    }

    function suspendOrRevokeControl(reason) {
        if (state.controlGranted) {
            state.controlGranted = false;
            state.controlSuspended = true;
            setBadge(elAuthority, "Role: View-Only (" + reason + ")", "badge-warn");
        }
        state.viewFresh = false;
        setBadge(elFreshness, "View: Stale (" + reason + ")", "badge-warn");
        syncButtons();
    }

    let wssClient = null;
    let connectionAttempt = null;
    const player = new global.FrankenHevc.WebCodecsHevcPlayer(elCanvas, {
        onConfigured: (info) => {
            state.generation = info.generation;
            setBadge(elFreshness, "View: Decoder Configured", "badge-neutral");
        },
        onFirstFrameDecoded: () => {
            // Receiving a first frame NEVER grants input control!
            setBadge(elFreshness, "View: First Frame Decoded", "badge-neutral");
        },
        onFramePresented: () => {
            if (!document.hidden) {
                state.viewFresh = true;
                setBadge(elFreshness, "View: Fresh", "badge-ok");
                syncButtons();
            }
        },
        onFrameClosed: (credit) => {
            if (wssClient) {
                wssClient.sendReceiverPressure(
                    credit.releasedBytes,
                    credit.releasedFrames,
                    0,
                    credit.decodeQueueSize
                );
            }
        },
        onDecoderReclaimed: (reason) => {
            suspendOrRevokeControl("decoder_reclaimed");
            showRefusal("HEVC Decoder Reclaimed", "Decoder reset required (" + reason + ").");
        },
    });

    wssClient = new global.FrankenWss.FrankenWssClient({
        onTransportProfile: (profile) => {
            const isWss = profile.indexOf("wss_") === 0;
            setBadge(
                elTransport,
                isWss ? "Transport: WSS (Degraded)" : "Transport: " + profile,
                isWss ? "badge-warn" : "badge-ok"
            );
            if (isWss && elBanner) {
                elBanner.textContent =
                    "Compatibility transport selected (" +
                    profile +
                    "): TCP head-of-line blocking applies; session authorization is still required.";
                elBanner.classList.remove("hidden");
            }
        },
        onControlBinary: (bytes) => {
            // Server-issued binary control frames (e.g. auxiliary ticket grants or FRD0 records).
            if (bytes.length >= 4 && bytes[0] === 0x46 && bytes[1] === 0x52 && bytes[2] === 0x54 && bytes[3] === 0x4b) {
                // "FRTK" + 1-byte role + 16-byte session_id + 32-byte single-use ticket
                if (bytes.length >= 53 && bytes[4] === 1) {
                    const sessionId = bytes.subarray(5, 21);
                    const ticket = bytes.subarray(21, 53);
                    wssClient.attachVideoChannel(sessionId, ticket);
                }
            }
        },
        onVideoBinary: (bytes) => {
            // Length-prefixed binary HEVC header + payload from host video WSS channel.
            if (bytes.length < 18) return;
            const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
            const tag = view.getUint8(0);
            if (tag === 0x01) {
                // DecoderConfiguration envelope: [0x01][gen:u32][w:u32][h:u32][codecLen:u16][codec][hvcc]
                const gen = view.getUint32(1, false);
                const width = view.getUint32(5, false);
                const height = view.getUint32(9, false);
                const codecLen = view.getUint16(13, false);
                if (15 + codecLen > bytes.length) return;
                const codec = new TextDecoder().decode(bytes.subarray(15, 15 + codecLen));
                const hvcc = bytes.subarray(15 + codecLen);
                const attempt = connectionAttempt;
                player.configure({
                    generation: gen,
                    codedWidth: width,
                    codedHeight: height,
                    codec: codec,
                    hvcc: hvcc,
                }).then((probe) => {
                    if (connectionAttempt !== attempt || wssClient.closed) return;
                    if (!probe.supported) {
                        closeConnection(probe.code);
                        showRefusal("HEVC Unavailable (" + probe.code + ")", probe.reason);
                    }
                });
            } else if (tag === 0x02) {
                // EncodedVideoChunk envelope: [0x02][gen:u32][flags:u8][frameId:u32][tsUs:u64][data...]
                const gen = view.getUint32(1, false);
                const isKey = (view.getUint8(5) & 0x01) !== 0;
                const frameId = view.getUint32(6, false);
                const tsUs = Number(view.getBigUint64(10, false));
                const payload = bytes.subarray(18);
                player.submitAccessUnit({
                    generation: gen,
                    isKey: isKey,
                    frameId: frameId,
                    timestampUs: tsUs,
                    sourceTimestampUs: tsUs,
                    chargedBytes: bytes.byteLength,
                    data: payload,
                });
            }
        },
        onClosed: (reason) => {
            closeConnection(reason);
        },
    });

    function closeConnection(reason, label) {
        // Invalidate the UI owner before aborting fetch or closing sockets:
        // any queued continuation belongs to the retired attempt.
        connectionAttempt = null;
        wssClient.disconnect(reason);
        state.connected = false;
        state.connecting = false;
        suspendOrRevokeControl(reason);
        player.reset();
        clearRefusal();
        setBadge(elConnection, label || "Disconnected (" + reason + ")", "badge-neutral");
        syncButtons();
    }

    // Browser lifecycle fencing: hidden tabs, pagehide, and bfcache restore
    // immediately revoke control and invalidate stale view freshness.
    document.addEventListener("visibilitychange", () => {
        const visible = !document.hidden;
        player.setVisible(visible);
        if (!visible) {
            suspendOrRevokeControl("tab_hidden");
        }
    });

    global.addEventListener("pagehide", () => {
        closeConnection("pagehide");
        player.setVisible(false);
    });

    global.addEventListener("pageshow", (ev) => {
        if (ev.persisted) {
            // bfcache restoration: require fresh connection & usable view check.
            closeConnection("bfcache_restore");
            player.setVisible(!document.hidden);
        }
    });

    btnConnect.addEventListener("click", async () => {
        if (state.connected || state.connecting) return;
        const attempt = {};
        connectionAttempt = attempt;
        state.connecting = true;
        clearRefusal();
        setBadge(elConnection, "Connecting...", "badge-neutral");
        syncButtons();
        try {
            await wssClient.connect("observer");
            if (connectionAttempt !== attempt || wssClient.closed) return;
            // WebSocket.onopen is only transport readiness. The Rust session
            // adapter must validate SessionOpened before observation is shown.
            setBadge(elConnection, "Awaiting session authorization", "badge-neutral");
            syncButtons();
        } catch (err) {
            if (connectionAttempt !== attempt) return;
            closeConnection("connection_refused");
            setBadge(elConnection, "Refused", "badge-danger");
            showRefusal("Connection Refused", err && err.message ? err.message : String(err));
            syncButtons();
        }
    });

    btnRelControl.addEventListener("click", () => {
        suspendOrRevokeControl("user_released");
    });

    btnFullscreen.addEventListener("click", () => {
        if (elCanvas && typeof elCanvas.requestFullscreen === "function") {
            elCanvas.requestFullscreen().catch(() => {});
        }
    });

    btnDisconnect.addEventListener("click", () => {
        closeConnection("user_requested", "Disconnected");
    });

    syncButtons();
})(window);
