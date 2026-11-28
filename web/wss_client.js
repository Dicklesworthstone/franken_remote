"use strict";
// Browser I/O only. The actual Rust/WASM adapter owns bootstrap validation,
// canonical wire bytes, startup deadlines, refusal parsing and session state.
// Socket readiness never grants observation, media readiness or input authority.
(function (global) {
    // Package I/O must also settle before a Rust adapter exists. This watchdog
    // never replaces or extends the Rust handshake deadline from startedMs.
    const CLIENT_LOAD_TIMEOUT_MS = 15000;

    function failure(value, fallback) {
        const code = typeof value === "string" && /^[a-z][a-z0-9_]{1,79}$/.test(value)
            ? value
            : value && typeof value.code === "string" && /^[a-z][a-z0-9_]{1,79}$/.test(value.code)
                ? value.code : fallback;
        const descriptions = {
            browser_https_required: "Browser sessions require same-origin HTTPS.",
            browser_session_unavailable: "This host cannot provide a browser desktop session yet.",
            browser_client_unavailable: "The host browser client package is unavailable.",
            browser_client_load_timeout: "The browser client package did not finish loading. Reload to try again.",
            browser_client_version_mismatch: "The browser client files do not match. Reload after updating the host.",
            browser_connection_cancelled: "Connection attempt was cancelled.",
            browser_transport_closed: "The host closed the browser connection.",
            browser_transport_failed: "The browser transport could not connect.",
            browser_approval_expired: "The host did not approve this connection before its original deadline. Connect again to make a new request.",
            host_local_approval_denied: "The host declined this viewing request.",
            host_approval_expired: "The viewing request expired before local approval completed.",
            browser_display_selection_expired: "Display selection did not finish before the connection's original deadline. Connect again to choose a display.",
            browser_display_selection_invalid: "The display selection no longer matches this session. Connect again to choose from a fresh display list.",
            browser_no_displays: "The approved host session has no displays available to share.",
            browser_bootstrap_http_error: "The host refused browser session bootstrap.",
            browser_bootstrap_too_large: "The host bootstrap response exceeded the browser limit.",
            browser_bootstrap_unreadable: "The host bootstrap response could not be read.",
            browser_control_too_large: "The host control record exceeded the browser limit.",
            browser_media_record_invalid: "The desktop media stream was refused. Reconnect to start a fresh session.",
            browser_media_cleanup_incomplete: "The browser has not confirmed decoder cleanup. Reload this page before reconnecting.",
            browser_video_too_large: "The host media record exceeded the negotiated limit.",
            browser_transport_congested: "The browser control connection could not remain responsive.",
            browser_canvas_unavailable: "The desktop canvas is unavailable.",
        };
        const error = new Error(descriptions[code] || "Browser session refused (" + code + ").");
        error.code = code;
        return error;
    }

    function cancelled() {
        return failure("browser_connection_cancelled");
    }

    class FrankenWssClient {
        constructor(callbacks, options) {
            this.callbacks = callbacks || {};
            const config = options || {};
            this.createAdapter = config.createAdapter || ((signal) => global.FrankenRust.createAdapter(signal));
            this.nowMs = config.nowMs || (() => global.performance.now());
            this.controlSocket = null;
            this.videoSocket = null;
            this.retiredAttempt = null;
            this.canvas = config.canvas || null;
            this.createPlayer = config.createPlayer || ((canvas, callbacks, media) =>
                new global.FrankenHevc.WebCodecsHevcPlayer(canvas, callbacks, media, { nowMs: this.nowMs }));
            this.closed = true;
            this.connectionAttempt = null;
            this.visible = true;
            this.focused = true;
        }

        _owns(attempt) {
            return attempt !== null && this.connectionAttempt === attempt && !this.closed;
        }

        // Native imports and response readers may settle after abort. Settle the
        // caller promptly, and check ownership again before using late results.
        _wait(attempt, operation) {
            return new Promise((resolve, reject) => {
                const signal = attempt.abort.signal;
                const aborted = () => reject(cancelled());
                if (signal.aborted) {
                    reject(cancelled());
                    Promise.resolve(operation).catch(() => {});
                    return;
                }
                signal.addEventListener("abort", aborted, { once: true });
                Promise.resolve(operation).then((value) => {
                    signal.removeEventListener("abort", aborted);
                    if (this._owns(attempt)) resolve(value);
                    else reject(cancelled());
                }, (error) => {
                    signal.removeEventListener("abort", aborted);
                    reject(this._owns(attempt) ? error : cancelled());
                });
            });
        }

        _state(attempt) {
            if (!this._owns(attempt) || !attempt.binding) return;
            const snapshot = JSON.parse(attempt.binding.core.snapshot());
            if (typeof this.callbacks.onState === "function") {
                // Every action closes over this original transport owner. A
                // detached old picker must never target a replacement adapter.
                this.callbacks.onState(snapshot, {
                    selectDisplay: (index) => this._selectDisplay(attempt, index),
                });
            }
        }

        _selectDisplay(attempt, index) {
            if (!this._owns(attempt) || !attempt.binding) return false;
            try {
                attempt.binding.core.selectDisplay(attempt.token, index, this.nowMs());
                this._pump(attempt);
                this._state(attempt);
                return this._owns(attempt);
            } catch (error) {
                this._end(attempt, failure(error, "browser_display_selection_invalid"), true);
                return false;
            }
        }

        async _bootstrapBody(response, attempt) {
            const maximum = attempt.binding.maxBootstrapBytes;
            const length = response.headers && response.headers.get("content-length");
            if (length !== null && length !== undefined
                && (!/^[0-9]+$/.test(length) || Number(length) > maximum)) {
                throw failure("browser_bootstrap_too_large");
            }
            if (!response.body || typeof response.body.getReader !== "function") {
                throw failure("browser_bootstrap_unreadable");
            }
            const reader = response.body.getReader();
            attempt.reader = reader;
            const output = new Uint8Array(maximum);
            let used = 0;
            try {
                while (true) {
                    const chunk = await this._wait(attempt, reader.read());
                    if (chunk.done) break;
                    if (!(chunk.value instanceof Uint8Array) || chunk.value.byteLength > maximum - used) {
                        throw failure("browser_bootstrap_too_large");
                    }
                    output.set(chunk.value, used);
                    used += chunk.value.byteLength;
                }
                return new TextDecoder("utf-8", { fatal: true }).decode(output.subarray(0, used));
            } finally {
                if (attempt.reader === reader) attempt.reader = null;
                try { Promise.resolve(reader.cancel()).catch(() => {}); } catch (_) {}
                try { reader.releaseLock(); } catch (_) {}
            }
        }

        async connect(role) {
            this.disconnect();
            if (this.retiredAttempt) this._cleanupAttempt(this.retiredAttempt);
            if (this.retiredAttempt) throw failure("browser_media_cleanup_incomplete");
            const attempt = {
                abort: new global.AbortController(),
                binding: null,
                token: null,
                reader: null,
                timer: null,
                loadingTimer: null,
                rejectOpen: null,
                failure: null,
                startedMs: this.nowMs(),
                player: null, configuring: false, pumping: false, cleaning: false, videoStarted: false,
            };
            this.connectionAttempt = attempt;
            this.closed = false;
            try {
                if (global.location.protocol !== "https:") throw failure("browser_https_required");
                attempt.loadingTimer = global.setTimeout(() => {
                    this._end(attempt, failure("browser_client_load_timeout"), true);
                }, CLIENT_LOAD_TIMEOUT_MS);
                const binding = await this._wait(attempt, Promise.resolve().then(() => this.createAdapter(attempt.abort.signal)).then((loaded) => {
                    if (!this._owns(attempt)) {
                        loaded.core.free();
                        throw cancelled();
                    }
                    attempt.binding = loaded;
                    return loaded;
                }));
                global.clearTimeout(attempt.loadingTimer);
                attempt.loadingTimer = null;
                attempt.token = binding.core.begin(role || "observer", attempt.startedMs);
                binding.core.lifecycle(attempt.token, this.visible ? "visible" : "hidden", this.nowMs());
                binding.core.lifecycle(attempt.token, this.focused ? "focus_gained" : "focus_lost", this.nowMs());
                this._state(attempt);
                if (!this._owns(attempt)) throw cancelled();
                attempt.timer = global.setInterval(() => {
                    if (!this._owns(attempt)) return;
                    try {
                        binding.core.tick(attempt.token, this.nowMs());
                        this._pump(attempt);
                        this._state(attempt);
                    } catch (error) {
                        this._end(attempt, failure(error, "browser_handshake_expired"), true);
                    }
                }, 100);
                const response = await this._wait(attempt, global.fetch("/api/session/bootstrap", {
                    method: "POST",
                    credentials: "same-origin",
                    cache: "no-store",
                    signal: attempt.abort.signal,
                    headers: { "Content-Type": "application/json" },
                    body: binding.core.bootstrapRequest(attempt.token),
                }));
                if (!response.ok) throw failure("browser_bootstrap_http_error");
                const body = await this._bootstrapBody(response, attempt);
                if (!this._owns(attempt)) throw cancelled();
                binding.core.acceptBootstrap(attempt.token, body, this.nowMs());
                this._state(attempt);
                await this._openControl(attempt);
            } catch (error) {
                const current = this._owns(attempt);
                if (current) this.disconnect();
                throw current ? failure(error, "browser_bootstrap_unreadable") : attempt.failure || cancelled();
            }
        }

        _openControl(attempt) {
            if (!this._owns(attempt)) return Promise.reject(cancelled());
            return new Promise((resolve, reject) => {
                const socket = new global.WebSocket("wss://" + global.location.host + "/ws/control");
                socket.binaryType = "arraybuffer";
                this.controlSocket = socket;
                let settled = false;
                const ownsSocket = () => this._owns(attempt) && this.controlSocket === socket;
                attempt.rejectOpen = (error) => {
                    if (settled) return;
                    settled = true;
                    reject(error);
                };
                socket.onopen = () => {
                    if (!ownsSocket()) return;
                    try {
                        const hello = attempt.binding.core.clientHello(attempt.token, this.nowMs());
                        this._send(socket, hello);
                        this._state(attempt);
                    } catch (error) {
                        this._end(attempt, failure(error, "browser_handshake_failed"), true);
                        return;
                    }
                    if (!ownsSocket()) return;
                    settled = true;
                    attempt.rejectOpen = null;
                    resolve();
                };
                socket.onmessage = (event) => {
                    if (!ownsSocket()) return;
                    if (!(event.data instanceof ArrayBuffer)
                        || event.data.byteLength > attempt.binding.maxControlRecordBytes) {
                        this._end(attempt, failure("browser_control_too_large"), true);
                        return;
                    }
                    try {
                        const reason = attempt.binding.core.receiveControl(
                            attempt.token, new Uint8Array(event.data), this.nowMs()
                        );
                        this._state(attempt);
                        if (reason) this._end(attempt, failure(reason, "browser_host_refused"), true);
                        else this._pump(attempt);
                    } catch (error) {
                        this._end(attempt, failure(error, "browser_control_invalid"), true);
                    }
                };
                socket.onerror = () => {
                    if (ownsSocket()) this._end(attempt, failure("browser_transport_failed"), false);
                };
                socket.onclose = () => {
                    if (ownsSocket()) this._end(attempt, failure("browser_transport_closed"), false);
                };
            });
        }

        _send(socket, bytes) {
            // This is only local transport admission, never receiver credit.
            // Cumulative credit comes exclusively from Rust after original
            // receive/native resource disposal.
            if (!socket || socket.readyState !== 1
                || !(bytes instanceof Uint8Array) || bytes.byteLength > 4096
                || Number(socket.bufferedAmount || 0) + bytes.byteLength > 32768) {
                throw failure("browser_transport_congested");
            }
            socket.send(bytes);
        }

        _pump(attempt) {
            if (!this._owns(attempt) || !attempt.binding || attempt.pumping) return;
            attempt.pumping = true;
            try {
                const core = attempt.binding.core;
                if (this.controlSocket && this.controlSocket.readyState === 1) {
                    for (let i = 0; i < 8 && this._owns(attempt); ++i) {
                        const bytes = core.takeControl(attempt.token, this.nowMs());
                        if (!bytes) break;
                        this._send(this.controlSocket, bytes);
                        core.controlQueued(attempt.token, this.nowMs());
                    }
                }
                if (!this._owns(attempt)) return;
                if (core.needsVideo() && !attempt.videoStarted) this._openVideo(attempt);
                if (core.configurationReady() && !attempt.configuring && !attempt.player) this._configurePlayer(attempt);
                if (this.videoSocket && this.videoSocket.readyState === 1) {
                    for (let i = 0; i < 2 && this._owns(attempt); ++i) {
                        const bytes = core.takeVideoFeedback(attempt.token, this.nowMs());
                        if (!bytes) break;
                        this._send(this.videoSocket, bytes);
                    }
                }
                if (this._owns(attempt) && attempt.player && attempt.player.configured && !attempt.configuring) {
                    const chunk = core.takeVideoChunk(attempt.token, this.nowMs());
                    if (chunk) {
                        const result = attempt.player.submitPreparedChunk(chunk);
                        if (!result.admitted) throw failure(result.reason, "browser_media_record_invalid");
                    }
                }
            } catch (error) {
                this._end(attempt, failure(error, "browser_media_record_invalid"), true);
            } finally {
                attempt.pumping = false;
            }
        }

        _openVideo(attempt) {
            if (!this._owns(attempt) || attempt.videoStarted) return;
            attempt.videoStarted = true;
            const socket = new global.WebSocket("wss://" + global.location.host + "/ws/video");
            socket.binaryType = "arraybuffer";
            this.videoSocket = socket;
            const owns = () => this._owns(attempt) && this.videoSocket === socket;
            socket.onopen = () => {
                if (!owns()) return;
                try {
                    const bytes = attempt.binding.core.videoAttach(attempt.token, this.nowMs());
                    this._send(socket, bytes);
                } catch (error) { this._end(attempt, failure(error, "browser_media_record_invalid"), true); }
            };
            socket.onmessage = (event) => {
                if (!owns()) return;
                if (!(event.data instanceof ArrayBuffer)
                    || event.data.byteLength > attempt.binding.maxVideoRecordBytes) {
                    this._end(attempt, failure("browser_video_too_large"), true);
                    return;
                }
                try {
                    const action = attempt.binding.core.receiveVideo(attempt.token,
                        new Uint8Array(event.data), this.nowMs());
                    if (action === "configuration") this._configurePlayer(attempt);
                    this._pump(attempt);
                    this._state(attempt);
                } catch (error) { this._end(attempt, failure(error, "browser_media_record_invalid"), true); }
            };
            socket.onerror = () => { if (owns()) this._end(attempt, failure("browser_transport_failed"), false); };
            socket.onclose = () => { if (owns()) this._end(attempt, failure("browser_transport_closed"), false); };
        }

        _configurePlayer(attempt) {
            if (!this._owns(attempt) || attempt.configuring || attempt.player || !this.canvas) {
                throw failure("browser_canvas_unavailable");
            }
            const core = attempt.binding.core;
            // The original Rust session is also the media owner. The local HEVC
            // probe's independent MediaAdapter is never used for remote frames.
            const media = {
                maxConfigurationBytes: () => attempt.binding.maxVideoRecordBytes,
                prepareConfiguration: () => core.prepareDesktopConfiguration(),
                decoderConfigured: (decoder) => core.desktopConfigured(decoder, this.nowMs()),
                output: (decoder, callback) => core.desktopOutput(decoder, callback, this.nowMs()),
                presented: (decoder, callback, now, visible) => core.desktopPresented(decoder, callback, now, visible),
                frameClosed: (decoder, callback) => core.desktopFrameClosed(decoder, callback, this.nowMs()),
                decoderClosed: (decoder) => core.desktopDecoderClosed(decoder),
                reclaim: (decoder) => core.desktopReclaim(decoder, this.nowMs()),
                retire: () => core.desktopRetire(),
                quarantine: () => core.desktopQuarantine(),
                snapshot: () => core.desktopSnapshot(),
            };
            const progress = () => {
                if (this._owns(attempt)) {
                    this._pump(attempt);
                    this._state(attempt);
                } else this._cleanupAttempt(attempt);
            };
            attempt.player = this.createPlayer(this.canvas, {
                onConfigured: progress,
                onFirstFrameDecoded: progress,
                onFramePresented: progress,
                onFrameClosed: progress,
                onDecoderReclaimed: (code) => {
                    if (this._owns(attempt)) this._end(attempt, failure(code, "browser_media_record_invalid"), true);
                },
            }, media);
            attempt.player.setVisible(this.visible);
            const parameters = JSON.parse(core.configurationParameters());
            parameters.hvcc = Uint8Array.from(parameters.hvcc);
            attempt.configuring = true;
            // A native isConfigSupported promise cannot be aborted. Its actual
            // decoder owner stays retained until this original promise settles.
            const player = attempt.player;
            Promise.resolve().then(() => {
                if (!this._owns(attempt)) return { supported: false, cancelled: true };
                return player.configure(parameters);
            }).then((result) => {
                if (!this._owns(attempt)) return;
                if (!result.supported) {
                    this._end(attempt, failure(result.code, "browser_media_record_invalid"), true);
                }
            }, (error) => {
                if (this._owns(attempt)) this._end(attempt, failure(error, "browser_media_record_invalid"), true);
            }).finally(() => {
                attempt.configuring = false;
                if (this._owns(attempt)) {
                    this._pump(attempt);
                    this._state(attempt);
                } else this._cleanupAttempt(attempt);
            });
        }

        _cleanupAttempt(attempt) {
            if (!attempt || !attempt.binding || attempt.cleaning || this._owns(attempt)) return;
            attempt.cleaning = true;
            let clean = false;
            try {
                const nativeClosed = !attempt.player || attempt.player.reset();
                const snapshot = JSON.parse(attempt.binding.core.desktopSnapshot());
                clean = nativeClosed && !attempt.configuring && !snapshot.quarantined
                    && snapshot.retainedDecoders === 0 && snapshot.inflightFrames === 0;
                if (clean) {
                    attempt.binding.core.free();
                    attempt.binding = null;
                    attempt.player = null;
                }
            } catch (_) {}
            finally { attempt.cleaning = false; }
            if (clean && this.retiredAttempt === attempt) this.retiredAttempt = null;
            else if (!clean) this.retiredAttempt = attempt;
        }

        _end(attempt, error, refusal) {
            if (!this._owns(attempt)) return;
            attempt.failure = error;
            if (attempt.rejectOpen) attempt.rejectOpen(error);
            this.disconnect();
            if (refusal && typeof this.callbacks.onRefused === "function") {
                this.callbacks.onRefused(error);
            } else if (typeof this.callbacks.onClosed === "function") {
                this.callbacks.onClosed(error.code);
            }
        }

        lifecycle(event) {
            if (event === "hidden") this.visible = false;
            if (event === "visible") this.visible = true;
            if (event === "focus_lost") this.focused = false;
            if (event === "focus_gained") this.focused = true;
            const attempt = this.connectionAttempt;
            if (attempt && attempt.player) attempt.player.setVisible(this.visible);
            const terminal = event === "pagehide" || event === "bfcache_restore";
            if (!this._owns(attempt)) return;
            if (!attempt.binding || attempt.token === null) {
                if (terminal) this._end(attempt, cancelled(), false);
                return;
            }
            try {
                attempt.binding.core.lifecycle(attempt.token, event, this.nowMs());
                this._state(attempt);
                if (terminal) this._end(attempt, cancelled(), false);
            } catch (error) {
                this._end(attempt, failure(error, "browser_lifecycle_closed"), false);
            }
        }

        disconnect() {
            const attempt = this.connectionAttempt;
            const sockets = [this.controlSocket, this.videoSocket];
            this.connectionAttempt = null;
            this.controlSocket = null;
            this.videoSocket = null;
            this.closed = true;
            if (attempt) {
                attempt.abort.abort();
                if (attempt.timer !== null) global.clearInterval(attempt.timer);
                if (attempt.loadingTimer !== null) global.clearTimeout(attempt.loadingTimer);
                if (attempt.rejectOpen) attempt.rejectOpen(cancelled());
                if (attempt.reader) {
                    try { Promise.resolve(attempt.reader.cancel()).catch(() => {}); } catch (_) {}
                }
                if (attempt.binding) {
                    try {
                        if (attempt.token !== null) attempt.binding.core.disconnect(attempt.token);
                    } catch (_) {}
                    this.retiredAttempt = attempt;
                    this._cleanupAttempt(attempt);
                }
            }
            for (const socket of sockets) {
                if (!socket) continue;
                socket.onopen = socket.onmessage = socket.onerror = socket.onclose = null;
                try { socket.close(1000, "connection_retired"); } catch (_) {}
            }
        }
    }

    global.FrankenWss = { FrankenWssClient };
})(window);
