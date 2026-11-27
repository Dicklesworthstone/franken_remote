"use strict";
// Browser I/O fixture. The default adapter is an instrumented ownership stub;
// wasm_transport.cjs supplies the actual generated Rust/WASM implementation.
const fs = require("node:fs");
const path = require("node:path");
const vm = require("node:vm");
const assert = require("node:assert/strict");
const source = fs.readFileSync(path.join(__dirname, "..", "wss_client.js"), "utf8");
const appSource = fs.readFileSync(path.join(__dirname, "..", "app.js"), "utf8");

function deferred() {
    let resolve, reject;
    const promise = new Promise((yes, no) => { resolve = yes; reject = no; });
    return { promise, resolve, reject };
}
function nextTurn() { return new Promise((resolve) => setImmediate(resolve)); }
function available(extra = {}) {
    return {
        nonce_hex: "a5".repeat(32),
        desktop_available: true,
        profile_id: 3,
        profile_version: 0,
        transport_profile: "wss_degraded_tcp_hol_dedicated",
        expires_in_ms: 15000,
        role: "observer",
        ...extra,
    };
}

function ioFixture(response = available(), options = {}) {
    const requests = [], sockets = [], cores = [], timers = new Map();
    let time = 100;
    let nextTimer = 1;
    class AdapterStub {
        constructor() {
            this.calls = [];
            this.phase = "disconnected";
            this.token = null;
            this.visible = true;
            this.focused = true;
            this.freed = 0;
            cores.push(this);
        }
        begin(role, now) { this.calls.push(["begin", role, now]); this.token = 1; this.phase = "bootstrapping"; return 1; }
        bootstrapRequest(token) { this.calls.push(["bootstrapRequest", token]); return '{"role":"observer"}'; }
        acceptBootstrap(token, body, now) {
            this.calls.push(["acceptBootstrap", token, body, now]);
            if (options.acceptBootstrap) options.acceptBootstrap(body);
            else if (JSON.parse(body).desktop_available !== true) throw "browser_session_unavailable";
            this.phase = "awaiting_transport";
        }
        clientHello(token, now) {
            this.calls.push(["clientHello", token, now]);
            this.phase = "awaiting_host";
            return options.hello || Uint8Array.of(7, 8, 9);
        }
        receiveControl(token, bytes, now) {
            this.calls.push(["receiveControl", token, Array.from(bytes), now]);
            if (options.receiveControl) return options.receiveControl(bytes);
            throw "browser_control_record_unsupported";
        }
        lifecycle(token, event) {
            this.calls.push(["lifecycle", token, event]);
            if (event === "hidden") this.visible = false;
            if (event === "visible") this.visible = true;
            if (event === "focus_lost") this.focused = false;
            if (event === "focus_gained") this.focused = true;
            if (event === "pagehide" || event === "bfcache_restore") this.phase = "closed";
        }
        tick(token, now) {
            this.calls.push(["tick", token, now]);
            if (options.tick) options.tick(now);
        }
        disconnect(token) { this.calls.push(["disconnect", token]); this.token = null; this.phase = "closed"; }
        free() { this.freed += 1; assert.equal(this.freed, 1, "original adapter freed exactly once"); }
        takeControl() { return undefined; }
        controlQueued() {}
        needsVideo() { return false; }
        configurationReady() { return false; }
        desktopSnapshot() {
            return JSON.stringify({ retainedDecoders: 0, inflightFrames: 0, quarantined: false });
        }
        snapshot() {
            return JSON.stringify({
                attempt: this.token, state_label: this.phase,
                transport_label: "wss_degraded_tcp_hol_dedicated",
                visible: this.visible, focused: this.focused,
                view_fresh: false, can_request_control: false, is_controlling: false, refusal_code: null,
            });
        }
    }
    const createAdapter = options.createAdapter || (async () => ({
        core: new AdapterStub(), maxBootstrapBytes: 4096, maxControlRecordBytes: 4096, maxVideoRecordBytes: 65536,
    }));
    const window = {
        location: { protocol: options.protocol || "https:", host: "host.fixture.ts.net:8443" },
        performance: { now: () => time },
        AbortController,
        FrankenRust: { createAdapter },
        setInterval(callback) { const id = nextTimer++; timers.set(id, callback); return id; },
        clearInterval(id) { timers.delete(id); },
        setTimeout(callback, delay) {
            const id = nextTimer++, due = time + delay;
            timers.set(id, () => {
                if (time < due) return;
                timers.delete(id);
                callback();
            });
            return id;
        },
        clearTimeout(id) { timers.delete(id); },
        fetch: async (url, requestOptions) => {
            const index = requests.length;
            const request = { url, options: requestOptions, cancelled: 0 };
            requests.push(request);
            let consumed = false;
            return {
                ok: options.httpOk !== false,
                headers: { get: () => options.contentLength === undefined ? null : options.contentLength },
                body: { getReader: () => ({
                    async read() {
                        if (consumed) return { done: true };
                        consumed = true;
                        const body = typeof response === "function" ? await response(index) : response;
                        return { done: false, value: options.rawBody || new TextEncoder().encode(JSON.stringify(body)) };
                    },
                    cancel() { request.cancelled += 1; return Promise.resolve(); },
                    releaseLock() {},
                }) },
            };
        },
        WebSocket: class {
            static OPEN = 1;
            constructor(url) {
                this.url = url;
                this.readyState = 0;
                this.sent = [];
                this.closeCalls = [];
                sockets.push(this);
            }
            send(bytes) { assert.equal(this.readyState, 1); this.sent.push(Array.from(bytes)); }
            close(code, reason) { this.readyState = 2; this.closeCalls.push({ code, reason }); }
            open() { this.readyState = 1; if (this.onopen) this.onopen(); }
            peerClose() { this.readyState = 3; if (this.onclose) this.onclose({ reason: "untrusted-peer-text" }); }
            message(data) { if (this.onmessage) this.onmessage({ data }); }
        },
    };
    const context = { window, ArrayBuffer, Uint8Array, DataView, TextDecoder };
    vm.runInNewContext(source, context);
    return {
        window, context, requests, sockets, cores, timers,
        makeClient: (callbacks) => new window.FrankenWss.FrankenWssClient(callbacks),
        tick(now) { time = now; for (const callback of Array.from(timers.values())) callback(); },
    };
}

function uiFixture(response, options = {}) {
    const f = ioFixture(response, options);
    const elements = new Map(), events = new Map(), documentEvents = new Map();
    const document = {
        hidden: false,
        hasFocus: () => true,
        getElementById(id) {
            if (!elements.has(id)) {
                const classes = new Set();
                elements.set(id, {
                    disabled: false, textContent: "", className: "",
                    listeners: new Map(),
                    classList: {
                        add: (name) => classes.add(name),
                        remove: (name) => classes.delete(name),
                        contains: (name) => classes.has(name),
                    },
                    addEventListener(name, callback) { this.listeners.set(name, callback); },
                });
            }
            return elements.get(id);
        },
        addEventListener: (name, callback) => documentEvents.set(name, callback),
    };
    f.window.addEventListener = (name, callback) => events.set(name, callback);
    vm.runInNewContext(appSource, { ...f.context, document });
    return {
        ...f, document,
        element: (id) => document.getElementById(id),
        click: (id) => document.getElementById(id).listeners.get("click")(),
        event: (name, data = {}) => events.get(name)(data),
        documentEvent: (name) => documentEvents.get(name)(),
    };
}
module.exports = { available, deferred, nextTurn, ioFixture, uiFixture };
