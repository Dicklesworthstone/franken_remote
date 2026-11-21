"use strict";
// FrankenRemote same-origin bootstrap + WSS degraded compatibility client.
//
// Security & transport invariants (AGENTS.md §§3.4, 5, 6; Plan §§12.5, 16.4):
// - Nonces and auxiliary channel tickets are NEVER placed in URLs or query strings.
// - Bootstrap runs strictly via same-origin POST /api/session/bootstrap with exact Origin.
// - Browser sockets open binary-only (`arraybuffer`) and authenticate in the FIRST
//   message (`FRBA` + 32-byte nonce on /ws/control; `FRAT` + session + role + 32-byte
//   ticket on /ws/video and /ws/audio). Bare session IDs attach nothing.
// - WSS is always labeled honestly as degraded (TCP head-of-line blocking).
// - Cumulative 0x0081 ReceiverPressure credit is sent ONLY when VideoFrame.close()
//   frees receive budget, never merely because the WebSocket delivered bytes.

(function (global) {
    const AUTH_MAGIC = [0x46, 0x52, 0x42, 0x41]; // "FRBA"
    const ATTACH_MAGIC = [0x46, 0x52, 0x41, 0x54]; // "FRAT"
    const WIRE_MAGIC = [0x46, 0x52, 0x44, 0x30]; // "FRD0"
    const KIND_RECEIVER_PRESSURE = 0x0081;
    const CHANNEL_ROLE_VIDEO = 1;
    const DIR_VIEWER_TO_HOST = 1;

    function hexToBytes(hex) {
        if (typeof hex !== "string" || hex.length % 2 !== 0 || !/^[0-9a-f]*$/i.test(hex)) {
            throw new Error("invalid hex token");
        }
        const out = new Uint8Array(hex.length / 2);
        for (let i = 0; i < out.length; i += 1) {
            const byte = parseInt(hex.slice(i * 2, i * 2 + 2), 16);
            if (Number.isNaN(byte)) {
                throw new Error("invalid hex byte");
            }
            out[i] = byte;
        }
        return out;
    }

    function wsOriginUrl(path) {
        const proto = global.location.protocol === "https:" ? "wss:" : "ws:";
        return proto + "//" + global.location.host + path;
    }

    function cancelledConnection() {
        const error = new Error("Connection attempt was cancelled.");
        error.name = "AbortError";
        error.code = "browser_connection_cancelled";
        return error;
    }

    function encodeReceiverPressureRecord(params) {
        // Matches crates/fr-wire/src/receiver_pressure.rs (RECORD_BYTES = 115).
        const buf = new ArrayBuffer(115);
        const view = new DataView(buf);
        const bytes = new Uint8Array(buf);
        bytes.set(WIRE_MAGIC, 0);
        view.setUint16(4, 0, false); // version = 0
        view.setUint16(6, KIND_RECEIVER_PRESSURE, false);
        view.setUint32(8, params.controlBindingId >>> 0, false);
        view.setUint32(12, 0, false); // flags
        view.setUint32(16, 115 - 24, false); // payload_len = 91
        view.setUint32(20, 0, false); // reserved

        // ControlBinding (48 bytes: host_boot, os_session, remote_session u128 BE)
        bytes.set(params.hostBootBytes, 24);
        bytes.set(params.osSessionBytes, 40);
        bytes.set(params.remoteSessionBytes, 56);

        view.setUint32(72, params.channelBinding >>> 0, false);
        view.setUint8(76, params.channelRole & 0xff);
        view.setUint8(77, DIR_VIEWER_TO_HOST);
        view.setBigUint64(78, BigInt(params.creditEpoch), false);
        view.setBigUint64(86, BigInt(params.releasedBytes), false);
        view.setBigUint64(94, BigInt(params.releasedFrames), false);
        view.setUint8(102, params.pressureTier & 0xff);
        view.setBigUint64(103, BigInt(params.queuedBytes), false);
        view.setUint32(111, params.queuedItems >>> 0, false);
        return bytes;
    }

    class FrankenWssClient {
        constructor(callbacks) {
            this.callbacks = callbacks || {};
            this.controlSocket = null;
            this.videoSocket = null;
            this.sessionMeta = null;
            this.transportProfile = "wss_degraded_tcp_hol_dedicated";
            this.creditEpoch = 1;
            this.videoBindingId = 2;
            this.controlBindingId = 1;
            this.closed = true;
            this.connectionAttempt = null;
        }

        async bootstrap(role, signal) {
            if (global.location.protocol !== "https:") {
                const error = new Error("Browser sessions require same-origin HTTPS.");
                error.code = "browser_https_required";
                throw error;
            }
            const resp = await global.fetch("/api/session/bootstrap", {
                method: "POST",
                credentials: "same-origin",
                cache: "no-store",
                signal: signal,
                headers: {
                    "Content-Type": "application/json",
                },
                body: JSON.stringify({ role: role || "observer" }),
            });
            if (!resp.ok) {
                const detail = await resp.text();
                throw new Error("Bootstrap refused (" + resp.status + "): " + detail);
            }
            const data = await resp.json();
            if (!data || typeof data.nonce_hex !== "string" || data.nonce_hex.length !== 64) {
                throw new Error("Malformed bootstrap response from host");
            }
            // A bootstrap nonce proves neither observation nor a usable browser
            // desktop. Older hosts with no explicit capability are also refused.
            // The host keeps this false until the Rust/WASM session adapter and
            // real media ownership path are connected; never invent binding IDs
            // or try the legacy socket-auth envelope to bypass that refusal.
            if (data.desktop_available !== true) {
                const error = new Error("Browser desktop sessions are unavailable on this host.");
                error.code = "browser_session_unavailable";
                throw error;
            }
            const transportProfile = data.transport_profile || "wss_degraded_tcp_hol_dedicated";
            const sessionMeta = {
                sessionIdHex: data.session_id_hex,
                hostBootHex: data.host_boot_hex,
                osSessionHex: data.os_session_hex,
                controlBindingId: data.control_binding_id || 1,
                videoBindingId: data.video_binding_id || 2,
                creditEpoch: data.credit_epoch || 1,
                role: data.role || "observer",
            };
            return {
                nonceBytes: hexToBytes(data.nonce_hex),
                sessionMeta: sessionMeta,
                transportProfile: transportProfile,
            };
        }

        _ownsAttempt(attempt) {
            return attempt !== null && this.connectionAttempt === attempt && !this.closed;
        }

        async connect(role) {
            this.disconnect();
            const attempt = {
                abort: new global.AbortController(),
                rejectOpen: null,
            };
            this.connectionAttempt = attempt;
            this.closed = false;

            try {
                const boot = await this.bootstrap(role || "observer", attempt.abort.signal);
                // Aborting fetch is best effort; a queued response/body result
                // must still belong to this exact attempt before any mutation.
                if (!this._ownsAttempt(attempt)) throw cancelledConnection();
                this.sessionMeta = boot.sessionMeta;
                this.transportProfile = boot.transportProfile;
                if (typeof this.callbacks.onTransportProfile === "function") {
                    this.callbacks.onTransportProfile(this.transportProfile);
                }
                await this._openControlChannel(boot.nonceBytes, attempt);
            } catch (error) {
                if (this.connectionAttempt === attempt) this.disconnect("connect_failed");
                throw error;
            }
        }

        _openControlChannel(nonceBytes, attempt) {
            if (!this._ownsAttempt(attempt)) return Promise.reject(cancelledConnection());
            return new Promise((resolve, reject) => {
                const ws = new global.WebSocket(wsOriginUrl("/ws/control"));
                ws.binaryType = "arraybuffer";
                this.controlSocket = ws;

                let settled = false;
                const ownsSocket = () => this._ownsAttempt(attempt) && this.controlSocket === ws;
                const rejectOpen = (error) => {
                    if (settled) return;
                    settled = true;
                    attempt.rejectOpen = null;
                    reject(error);
                };
                attempt.rejectOpen = rejectOpen;
                const end = (reason, error) => {
                    if (!ownsSocket()) return;
                    rejectOpen(error || new Error(reason));
                    this.disconnect(reason);
                    if (typeof this.callbacks.onClosed === "function") {
                        this.callbacks.onClosed(reason);
                    }
                };
                ws.onopen = () => {
                    if (!ownsSocket()) return;
                    // First message MUST be FRBA + 32-byte single-use bootstrap nonce.
                    const authFrame = new Uint8Array(36);
                    authFrame.set(AUTH_MAGIC, 0);
                    authFrame.set(nonceBytes, 4);
                    try {
                        ws.send(authFrame);
                    } catch (_) {
                        end("control_socket_send_failed", new Error("Control WSS authentication failed"));
                        return;
                    }
                    if (!settled) {
                        settled = true;
                        attempt.rejectOpen = null;
                        resolve();
                    }
                };

                ws.onmessage = (ev) => {
                    if (!ownsSocket()) return;
                    if (!(ev.data instanceof ArrayBuffer)) {
                        // Text frames are forbidden on binary WSS channels.
                        end("protocol_text_frame_refused");
                        return;
                    }
                    this._handleControlBinary(new Uint8Array(ev.data));
                };

                ws.onerror = () => {
                    end("control_socket_error", new Error("Control WSS connection failed"));
                };

                ws.onclose = (ev) => {
                    end(ev.reason || "control_socket_closed", new Error("Control WSS closed before opening"));
                };
            });
        }

        attachVideoChannel(sessionIdBytes, ticketBytes) {
            const attempt = this.connectionAttempt;
            if (!this._ownsAttempt(attempt) || !this.controlSocket ||
                    this.controlSocket.readyState !== global.WebSocket.OPEN) return false;
            if (this.videoSocket) {
                const old = this.videoSocket;
                this.videoSocket = null;
                try {
                    old.close(1000, "channel_replaced");
                } catch (_) {
                    // Ignore close error on replaced channel.
                }
            }

            const ws = new global.WebSocket(wsOriginUrl("/ws/video"));
            ws.binaryType = "arraybuffer";
            this.videoSocket = ws;
            const ownsSocket = () => this._ownsAttempt(attempt) && this.videoSocket === ws;

            ws.onopen = () => {
                if (!ownsSocket()) return;
                // First message MUST be FRAT + 16-byte session_id + 1-byte role(video=1) + 32-byte ticket.
                const frame = new Uint8Array(53);
                frame.set(ATTACH_MAGIC, 0);
                frame.set(sessionIdBytes.subarray(0, 16), 4);
                frame[20] = CHANNEL_ROLE_VIDEO;
                frame.set(ticketBytes.subarray(0, 32), 21);
                ws.send(frame);
            };

            ws.onmessage = (ev) => {
                if (!ownsSocket()) return;
                if (!(ev.data instanceof ArrayBuffer)) {
                    this.disconnect("protocol_text_frame_refused");
                    if (typeof this.callbacks.onClosed === "function") {
                        this.callbacks.onClosed("protocol_text_frame_refused");
                    }
                    return;
                }
                if (typeof this.callbacks.onVideoBinary === "function") {
                    this.callbacks.onVideoBinary(new Uint8Array(ev.data));
                }
            };

            ws.onclose = (ev) => {
                if (!ownsSocket()) return;
                this.videoSocket = null;
                if (typeof this.callbacks.onVideoClosed === "function") {
                    this.callbacks.onVideoClosed(ev.reason || "video_socket_closed");
                }
            };
            return true;
        }

        sendReceiverPressure(releasedBytes, releasedFrames, queuedBytes, queuedItems) {
            if (!this.controlSocket || this.controlSocket.readyState !== global.WebSocket.OPEN || !this.sessionMeta) {
                return false;
            }
            const pressureTier = queuedItems >= 3 ? 2 : queuedItems >= 1 ? 1 : 0;
            const record = encodeReceiverPressureRecord({
                controlBindingId: this.sessionMeta.controlBindingId,
                hostBootBytes: hexToBytes(this.sessionMeta.hostBootHex),
                osSessionBytes: hexToBytes(this.sessionMeta.osSessionHex),
                remoteSessionBytes: hexToBytes(this.sessionMeta.sessionIdHex),
                channelBinding: this.sessionMeta.videoBindingId,
                channelRole: CHANNEL_ROLE_VIDEO,
                creditEpoch: this.sessionMeta.creditEpoch,
                releasedBytes: releasedBytes,
                releasedFrames: releasedFrames,
                pressureTier: pressureTier,
                queuedBytes: queuedBytes,
                queuedItems: queuedItems,
            });
            this.controlSocket.send(record);
            return true;
        }

        sendControlRecord(bytes) {
            if (!this.controlSocket || this.controlSocket.readyState !== global.WebSocket.OPEN) {
                return false;
            }
            this.controlSocket.send(bytes);
            return true;
        }

        _handleControlBinary(bytes) {
            if (typeof this.callbacks.onControlBinary === "function") {
                this.callbacks.onControlBinary(bytes);
            }
        }

        disconnect(reason) {
            const attempt = this.connectionAttempt;
            this.connectionAttempt = null;
            this.closed = true;
            this.sessionMeta = null;
            const video = this.videoSocket;
            const control = this.controlSocket;
            this.videoSocket = null;
            this.controlSocket = null;
            if (attempt) {
                attempt.abort.abort();
                if (attempt.rejectOpen) attempt.rejectOpen(cancelledConnection());
            }
            if (video) {
                try {
                    video.close(1000, reason || "client_disconnect");
                } catch (_) {
                    // Ignore close error.
                }
            }
            if (control) {
                try {
                    control.close(1000, reason || "client_disconnect");
                } catch (_) {
                    // Ignore close error.
                }
            }
        }
    }

    global.FrankenWss = {
        FrankenWssClient: FrankenWssClient,
        encodeReceiverPressureRecord: encodeReceiverPressureRecord,
        hexToBytes: hexToBytes,
    };
})(window);
