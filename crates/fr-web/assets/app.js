/*
 * app.js — FrankenRemote Browser Client Controller
 *
 * Implements Plan §16.3 & §16.4:
 * 1. WebCodecs HEVC configuration & hardware decode.
 * 2. Same-origin bootstrap POST with single-use nonce.
 * 3. Bounded first message application authentication over WebTransport / WebSocket.
 * 4. Prompt VideoFrame closure and bounded decode queues.
 * 5. Hidden tab throttling and bfcache authority revocation.
 * 6. Direct Touch and Trackpad input interaction modes.
 */

const CURRENT_ASSET_VERSION = "1.0.0";

class FrankenWebClient {
    constructor() {
        this.hostAddress = "";
        this.authToken = "";
        this.socket = null;
        this.worker = null;
        this.touchMode = "direct"; // "direct" or "trackpad"
        this.isPointerLocked = false;
        this.inputLease = null;
        this.geometryGen = 1;

        this.canvas = document.getElementById("display-canvas");
        this.ctx = this.canvas.getContext("2d", { alpha: false, desynchronized: true });

        this.virtualCursor = document.getElementById("virtual-cursor");
        this.virtualCursorPos = { x: 200, y: 300 };

        this.desktopResolution = { width: 1920, height: 1080 };
        this.mediaStream = null;
        this.isMicEnabled = false;

        this.setupEventListeners();
        this.setupWorker();
        this.checkCodecSupport();
    }

    async checkCodecSupport() {
        if (!("VideoDecoder" in window)) {
            this.showModal("Browser Unsupported", "This browser does not support WebCodecs. Please use a recent version of Chrome, Edge, or Safari.");
            return false;
        }

        try {
            const support = await VideoDecoder.isConfigSupported({
                codec: "hvc1.1.6.L93.B0",
                codedWidth: 1920,
                codedHeight: 1080,
                hardwareAcceleration: "prefer-hardware"
            });

            if (!support.supported) {
                this.showModal("HEVC Unavailable", "Your browser does not have hardware HEVC (H.265) decoding enabled. FrankenRemote requires HEVC to deliver workstation text quality and low latency.");
                return false;
            }
            return true;
        } catch (e) {
            console.warn("Codec support query failed:", e);
            return false;
        }
    }

    setupWorker() {
        this.worker = new Worker("worker.js");
        this.worker.onmessage = (e) => {
            const { type, frame, message } = e.data;
            if (type === "frame" && frame) {
                this.renderFrame(frame);
            } else if (type === "error") {
                console.error("Decoder worker error:", message);
            }
        };
    }

    renderFrame(frame) {
        if (this.canvas.width !== frame.displayWidth || this.canvas.height !== frame.displayHeight) {
            this.canvas.width = frame.displayWidth;
            this.canvas.height = frame.displayHeight;
            this.desktopResolution.width = frame.displayWidth;
            this.desktopResolution.height = frame.displayHeight;
        }

        // Draw decoded frame to presentation surface
        this.ctx.drawImage(frame, 0, 0, this.canvas.width, this.canvas.height);

        // Crucial: close VideoFrame immediately to return GPU surface to system pool
        frame.close();
    }

    setupEventListeners() {
        // Connect button
        document.getElementById("connect-btn").addEventListener("click", () => this.connect());

        // Mode toggle
        const btnToggleMode = document.getElementById("btn-toggle-mode");
        btnToggleMode.addEventListener("click", () => {
            this.touchMode = (this.touchMode === "direct") ? "trackpad" : "direct";
            btnToggleMode.textContent = (this.touchMode === "trackpad") ? "👆 Direct Touch" : "🖱️ Trackpad";
            document.getElementById("mode-badge").textContent = (this.touchMode === "trackpad") ? "Trackpad Mode" : "Direct Touch";
            this.virtualCursor.classList.toggle("hidden", this.touchMode !== "trackpad");
        });

        // Pointer Lock
        document.getElementById("btn-lock-pointer").addEventListener("click", () => {
            this.canvas.requestPointerLock();
        });

        document.addEventListener("pointerlockchange", () => {
            this.isPointerLocked = (document.pointerLockElement === this.canvas);
        });

        // Fullscreen
        document.getElementById("btn-toggle-fullscreen").addEventListener("click", () => {
            if (!document.fullscreenElement) {
                document.getElementById("app-container").requestFullscreen().catch(console.warn);
            } else {
                document.exitFullscreen().catch(console.warn);
            }
        });

        // Modifiers toggle
        const btnToggleModifiers = document.getElementById("btn-toggle-modifiers");
        btnToggleModifiers.addEventListener("click", () => {
            document.getElementById("modifiers-bar").classList.toggle("hidden");
        });

        // Modifiers click
        document.querySelectorAll(".mod-key").forEach(btn => {
            btn.addEventListener("click", () => {
                const key = btn.getAttribute("data-key");
                this.sendKey(key, "keydown");
                setTimeout(() => this.sendKey(key, "keyup"), 50);
            });
        });

        // Push to talk toggle
        document.getElementById("btn-toggle-talk").addEventListener("click", () => this.toggleMicrophone());

        // Disconnect
        document.getElementById("btn-disconnect").addEventListener("click", () => this.disconnect());

        // Canvas mouse/pointer interactions
        this.canvas.addEventListener("pointermove", (e) => this.handlePointerMove(e));
        this.canvas.addEventListener("pointerdown", (e) => this.handlePointerDown(e));
        this.canvas.addEventListener("pointerup", (e) => this.handlePointerUp(e));
        this.canvas.addEventListener("wheel", (e) => this.handleWheel(e), { passive: false });
        this.canvas.addEventListener("contextmenu", (e) => e.preventDefault());

        // Keyboard inputs
        window.addEventListener("keydown", (e) => this.handleKeyDown(e));
        window.addEventListener("keyup", (e) => this.handleKeyUp(e));

        // Modal OK
        document.getElementById("modal-btn-ok").addEventListener("click", () => {
            document.getElementById("alert-modal").classList.add("hidden");
        });

        // Constitutional Invariant: Tab hidden immediately revokes input authority
        document.addEventListener("visibilitychange", () => {
            if (document.hidden) {
                console.log("Tab backgrounded: dropping remote control authority");
                this.inputLease = null;
                if (this.socket && this.socket.readyState === WebSocket.OPEN) {
                    this.socket.send(JSON.stringify({ type: "suspend_control" }));
                }
            } else {
                console.log("Tab resumed: requesting fresh recovery IDR frame");
                if (this.socket && this.socket.readyState === WebSocket.OPEN) {
                    this.socket.send(JSON.stringify({ type: "resume_control" }));
                }
            }
        });

        // bfcache restoration
        window.addEventListener("pageshow", (e) => {
            if (e.persisted) {
                console.log("bfcache restoration: forcing clean reconnect");
                this.disconnect();
                this.connect();
            }
        });

        window.addEventListener("pagehide", () => {
            this.disconnect();
        });
    }

    async connect() {
        const hostInput = document.getElementById("host-input").value.trim();
        const tokenInput = document.getElementById("token-input").value.trim();
        const statusEl = document.getElementById("connect-status");

        if (!hostInput) {
            statusEl.textContent = "Please enter host address";
            statusEl.style.color = "#ef4444";
            return;
        }

        statusEl.textContent = "Bootstrapping session...";
        statusEl.style.color = "#3b82f6";

        try {
            // 1. Same-Origin Bootstrap POST to obtain short-lived single-use nonce
            const bootstrapUrl = (location.origin === "null" || !location.origin.startsWith("http")) 
                ? `https://${hostInput}/api/bootstrap`
                : `${location.origin}/api/bootstrap`;

            const resp = await fetch(bootstrapUrl, {
                method: "POST",
                headers: {
                    "Content-Type": "application/json",
                    "X-FrankenRemote-Asset-Version": CURRENT_ASSET_VERSION
                },
                body: JSON.stringify({
                    role: "viewer_controller",
                    token: tokenInput
                })
            });

            if (!resp.ok) {
                throw new Error(`Bootstrap failed: HTTP ${resp.status}`);
            }

            const data = await resp.json();
            const nonce = data.nonce;
            const assetVersion = data.asset_version || CURRENT_ASSET_VERSION;

            // Check version skew
            if (assetVersion !== CURRENT_ASSET_VERSION) {
                this.showModal("Asset Update Available", "A newer version of the workstation assets is available. The page will reload.");
                setTimeout(() => location.reload(), 2000);
                return;
            }

            // 2. Open Transport (WebTransport / WSS)
            const wsProto = (location.protocol === "https:") ? "wss:" : "ws:";
            const wsUrl = `${wsProto}//${hostInput}/ws/session`;

            this.socket = new WebSocket(wsUrl);
            this.socket.binaryType = "arraybuffer";

            this.socket.onopen = () => {
                // 3. Send Bounded First Message Application Authentication
                this.socket.send(JSON.stringify({
                    type: "first_auth",
                    nonce: nonce,
                    origin: location.origin,
                    version: CURRENT_ASSET_VERSION
                }));
            };

            this.socket.onmessage = (event) => {
                this.handleSocketMessage(event.data);
            };

            this.socket.onerror = (e) => {
                statusEl.textContent = "Transport connection error";
                statusEl.style.color = "#ef4444";
            };

            this.socket.onclose = () => {
                this.showConnectScreen();
            };

        } catch (err) {
            statusEl.textContent = err.message;
            statusEl.style.color = "#ef4444";
        }
    }

    handleSocketMessage(data) {
        if (typeof data === "string") {
            try {
                const msg = JSON.parse(data);
                switch (msg.type) {
                    case "auth_ok":
                        this.inputLease = msg.lease_handle;
                        this.showSessionScreen();
                        break;
                    case "auth_fail":
                        this.showModal("Authentication Failed", msg.reason || "Token invalid or expired");
                        this.disconnect();
                        break;
                    case "codec_config":
                        this.worker.postMessage({
                            type: "configure",
                            data: {
                                codec: msg.codec || "hvc1.1.6.L93.B0",
                                description: new Uint8Array(msg.description || []),
                                width: msg.width,
                                height: msg.height
                            }
                        });
                        break;
                    case "quality":
                        document.getElementById("stat-rtt").textContent = `${msg.rtt_ms || 0}ms`;
                        document.getElementById("stat-fps").textContent = `${msg.fps || 0} FPS`;
                        document.getElementById("stat-bitrate").textContent = `${((msg.bitrate_kbps || 0) / 1000).toFixed(1)}M`;
                        break;
                }
            } catch (e) {
                console.error("JSON parse error:", e);
            }
        } else if (data instanceof ArrayBuffer) {
            // Binary access unit fragment from media stream
            const bytes = new Uint8Array(data);
            const isKey = (bytes[0] & 0x01) !== 0;
            this.worker.postMessage({
                type: "decode",
                data: {
                    isKey: isKey,
                    timestamp: performance.now() * 1000,
                    bytes: bytes.slice(1)
                }
            });
        }
    }

    showSessionScreen() {
        document.getElementById("connect-screen").classList.add("hidden");
        document.getElementById("session-screen").classList.remove("hidden");
    }

    showConnectScreen() {
        document.getElementById("session-screen").classList.add("hidden");
        document.getElementById("connect-screen").classList.remove("hidden");
        if (this.worker) {
            this.worker.postMessage({ type: "reset" });
        }
    }

    disconnect() {
        if (this.socket) {
            this.socket.close();
            this.socket = null;
        }
        this.inputLease = null;
        this.showConnectScreen();
    }

    handlePointerMove(e) {
        if (!this.inputLease || document.hidden) return;

        if (this.touchMode === "direct") {
            const rect = this.canvas.getBoundingClientRect();
            const x = Math.round((e.clientX - rect.left) * (this.desktopResolution.width / rect.width));
            const y = Math.round((e.clientY - rect.top) * (this.desktopResolution.height / rect.height));
            this.sendPointer(x, y, 0, e.buttons);
        } else if (this.touchMode === "trackpad") {
            const dx = e.movementX || 0;
            const dy = e.movementY || 0;
            this.virtualCursorPos.x = Math.max(0, Math.min(window.innerWidth, this.virtualCursorPos.x + dx));
            this.virtualCursorPos.y = Math.max(0, Math.min(window.innerHeight, this.virtualCursorPos.y + dy));

            this.virtualCursor.style.left = `${this.virtualCursorPos.x}px`;
            this.virtualCursor.style.top = `${this.virtualCursorPos.y}px`;

            const rect = this.canvas.getBoundingClientRect();
            const x = Math.round((this.virtualCursorPos.x - rect.left) * (this.desktopResolution.width / rect.width));
            const y = Math.round((this.virtualCursorPos.y - rect.top) * (this.desktopResolution.height / rect.height));
            this.sendPointer(x, y, 0, e.buttons);
        }
    }

    handlePointerDown(e) {
        if (!this.inputLease || document.hidden) return;
        this.virtualCursor.classList.add("pressed");
        this.sendPointerEvent(e, 1);
    }

    handlePointerUp(e) {
        if (!this.inputLease || document.hidden) return;
        this.virtualCursor.classList.remove("pressed");
        this.sendPointerEvent(e, 2);
    }

    sendPointerEvent(e, action) {
        const rect = this.canvas.getBoundingClientRect();
        const clientX = (this.touchMode === "trackpad") ? this.virtualCursorPos.x : e.clientX;
        const clientY = (this.touchMode === "trackpad") ? this.virtualCursorPos.y : e.clientY;

        const x = Math.round((clientX - rect.left) * (this.desktopResolution.width / rect.width));
        const y = Math.round((clientY - rect.top) * (this.desktopResolution.height / rect.height));

        this.sendPointer(x, y, action, e.button || 1);
    }

    handleWheel(e) {
        if (!this.inputLease || document.hidden) return;
        e.preventDefault();
        if (this.socket && this.socket.readyState === WebSocket.OPEN) {
            this.socket.send(JSON.stringify({
                type: "scroll",
                dx: Math.round(e.deltaX),
                dy: Math.round(e.deltaY),
                lease: this.inputLease
            }));
        }
    }

    handleKeyDown(e) {
        if (!this.inputLease || document.hidden) return;
        if (["Tab", "Alt", "Meta", "Control"].includes(e.key)) {
            e.preventDefault();
        }
        this.sendKey(e.code, "keydown");
    }

    handleKeyUp(e) {
        if (!this.inputLease || document.hidden) return;
        this.sendKey(e.code, "keyup");
    }

    sendPointer(x, y, action, button) {
        if (this.socket && this.socket.readyState === WebSocket.OPEN && this.inputLease) {
            this.socket.send(JSON.stringify({
                type: "pointer",
                x: x,
                y: y,
                action: action,
                button: button,
                lease: this.inputLease
            }));
        }
    }

    sendKey(keyCode, action) {
        if (this.socket && this.socket.readyState === WebSocket.OPEN && this.inputLease) {
            this.socket.send(JSON.stringify({
                type: "key",
                code: keyCode,
                action: action,
                lease: this.inputLease
            }));
        }
    }

    async toggleMicrophone() {
        const btn = document.getElementById("btn-toggle-talk");
        if (this.isMicEnabled) {
            if (this.mediaStream) {
                this.mediaStream.getTracks().forEach(t => t.stop());
                this.mediaStream = null;
            }
            this.isMicEnabled = false;
            btn.classList.remove("active");
            btn.textContent = "🎙️ Talk";
        } else {
            try {
                this.mediaStream = await navigator.mediaDevices.getUserMedia({ audio: true, video: false });
                this.isMicEnabled = true;
                btn.classList.add("active");
                btn.textContent = "🔴 Mute";
            } catch (err) {
                this.showModal("Microphone Permission Denied", "Could not access microphone: " + err.message);
            }
        }
    }

    showModal(title, body) {
        document.getElementById("modal-title").textContent = title;
        document.getElementById("modal-body").textContent = body;
        document.getElementById("alert-modal").classList.remove("hidden");
    }
}

window.addEventListener("DOMContentLoaded", () => {
    new FrankenWebClient();
});
