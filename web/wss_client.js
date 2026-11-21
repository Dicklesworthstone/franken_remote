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
            browser_bootstrap_http_error: "The host refused browser session bootstrap.",
            browser_bootstrap_too_large: "The host bootstrap response exceeded the browser limit.",
            browser_bootstrap_unreadable: "The host bootstrap response could not be read.",
            browser_control_too_large: "The host control record exceeded the browser limit.",
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
            if (typeof this.callbacks.onState === "function") this.callbacks.onState(snapshot);
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
                        socket.send(hello);
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
            const socket = this.controlSocket;
            this.connectionAttempt = null;
            this.controlSocket = null;
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
                    try { attempt.binding.core.free(); } catch (_) {}
                    attempt.binding = null;
                }
            }
            if (socket) {
                try { socket.close(1000, "connection_retired"); } catch (_) {}
            }
        }
    }

    global.FrankenWss = { FrankenWssClient };
})(window);
