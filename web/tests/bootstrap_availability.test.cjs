"use strict";

// Transport-shell regressions only; these are not a live browser or HEVC gate.
const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const test = require("node:test");
const vm = require("node:vm");

const source = fs.readFileSync(path.join(__dirname, "..", "wss_client.js"), "utf8");
const appSource = fs.readFileSync(path.join(__dirname, "..", "app.js"), "utf8");

function fixture(response, protocol = "https:", options = {}) {
    const requests = [];
    let sockets = 0;
    const socketInstances = [];
    const window = {
        location: { protocol, host: "host.fixture.ts.net:8443" },
        AbortController,
        fetch: async (url, requestOptions) => {
            const index = requests.length;
            requests.push({ url, options: requestOptions });
            // Deliberately allow a queued body to finish after AbortSignal:
            // cancellation must fence the continuation as well as the fetch.
            return { ok: true, json: async () => typeof response === "function" ? response(index) : response };
        },
        WebSocket: class {
            static OPEN = 1;
            constructor(url) {
                sockets += 1;
                if (!options.allowSockets) throw new Error("No socket is permitted without a desktop capability");
                this.url = url;
                this.readyState = 0;
                this.sent = [];
                this.closeCalls = [];
                socketInstances.push(this);
            }
            send(bytes) {
                assert.equal(this.readyState, 1);
                this.sent.push(Array.from(bytes));
            }
            close(code, reason) {
                this.readyState = 2;
                this.closeCalls.push({ code, reason });
            }
            open() {
                this.readyState = 1;
                if (this.onopen) this.onopen();
            }
            peerClose(reason = "peer_closed") {
                this.readyState = 3;
                if (this.onclose) this.onclose({ reason });
            }
            message(data) {
                if (this.onmessage) this.onmessage({ data });
            }
        },
    };
    const context = { window, ArrayBuffer, Uint8Array, DataView, TextDecoder };
    vm.runInNewContext(source, context);
    return {
        client: new window.FrankenWss.FrankenWssClient(options.callbacks),
        api: window.FrankenWss,
        window,
        context,
        requests,
        socketInstances,
        sockets: () => sockets,
    };
}

function deferred() {
    let resolve;
    const promise = new Promise((done) => { resolve = done; });
    return { promise, resolve };
}

function available(extra = {}) {
    return { nonce_hex: "a5".repeat(32), desktop_available: true, ...extra };
}

function nextTurn() {
    return new Promise((resolve) => setImmediate(resolve));
}

function uiFixture(response) {
    const f = fixture(response, "https:", { allowSockets: true });
    const elements = new Map();
    const events = new Map();
    const document = {
        hidden: false,
        getElementById(id) {
            if (!elements.has(id)) {
                const classes = new Set();
                elements.set(id, {
                    disabled: false,
                    textContent: "",
                    className: "",
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
        addEventListener() {},
    };
    f.window.addEventListener = (name, callback) => events.set(name, callback);
    f.window.FrankenHevc = {
        WebCodecsHevcPlayer: class {
            reset() {}
            setVisible() {}
        },
    };
    vm.runInNewContext(appSource, { ...f.context, document });
    return {
        ...f,
        element: (id) => document.getElementById(id),
        click: (id) => document.getElementById(id).listeners.get("click")(),
        event: (name, detail = {}) => events.get(name)(detail),
    };
}

test("bootstrap availability is required before any socket or observation", async () => {
    for (const availability of [false, undefined, "true", 1]) {
        const nonce = "a5".repeat(32);
        const f = fixture({
            nonce_hex: nonce,
            desktop_available: availability,
            refusal_code: "browser_session_unavailable",
        });
        await assert.rejects(f.client.connect("observer"), (error) => {
            assert.equal(error.code, "browser_session_unavailable");
            assert.ok(!error.message.includes(nonce));
            return true;
        });
        assert.equal(f.sockets(), 0);
        assert.equal(f.client.sessionMeta, null);
        assert.equal(f.requests.length, 1);
        assert.equal(f.requests[0].url, "/api/session/bootstrap");
        assert.equal(f.requests[0].options.method, "POST");
        assert.equal(f.requests[0].options.credentials, "same-origin");
        assert.equal(f.requests[0].options.cache, "no-store");
        assert.ok(!f.requests[0].url.includes(nonce));
    }
});

test("plaintext bootstrap is refused before sending credentials", async () => {
    const f = fixture({ nonce_hex: "a5".repeat(32) }, "http:");
    await assert.rejects(f.client.connect("observer"), {
        code: "browser_https_required",
    });
    assert.equal(f.requests.length, 0);
    assert.equal(f.sockets(), 0);
});

test("hex tokens reject partial parseInt matches and whitespace", () => {
    const { api } = fixture({});
    assert.deepEqual(Array.from(api.hexToBytes("a500FF")), [165, 0, 255]);
    for (const invalid of ["0g", " 1", "1 ", "aa\n", "f", 12, null]) {
        assert.throws(() => api.hexToBytes(invalid));
    }
});

test("disconnect aborts bootstrap and a late body cannot create a socket or metadata", async () => {
    const body = deferred();
    let profiles = 0;
    const f = fixture(() => body.promise, "https:", {
        allowSockets: true,
        callbacks: { onTransportProfile: () => { profiles += 1; } },
    });
    const pending = f.client.connect("observer");
    const rejected = assert.rejects(pending, { code: "browser_connection_cancelled" });
    await nextTurn();
    f.client.disconnect("user_requested");
    assert.equal(f.requests[0].options.signal.aborted, true);
    body.resolve(available());
    await rejected;
    assert.equal(f.sockets(), 0);
    assert.equal(profiles, 0);
    assert.equal(f.client.sessionMeta, null);
    assert.equal(f.client.closed, true);
});

test("a superseded bootstrap cannot overwrite a replacement connection", async () => {
    const body = deferred();
    const f = fixture((index) => index === 0 ? body.promise : available({ session_id_hex: "bb".repeat(16) }),
        "https:", { allowSockets: true });
    const first = f.client.connect("observer");
    const retired = assert.rejects(first, { code: "browser_connection_cancelled" });
    await nextTurn();
    const second = f.client.connect("observer");
    await nextTurn();
    assert.equal(f.sockets(), 1);
    const current = f.socketInstances[0];
    current.open();
    await second;
    body.resolve(available({ session_id_hex: "cc".repeat(16) }));
    await retired;
    assert.equal(f.requests[0].options.signal.aborted, true);
    assert.equal(f.client.controlSocket, current);
    assert.equal(f.client.sessionMeta.sessionIdHex, "bb".repeat(16));
    assert.equal(f.client.closed, false);
    assert.equal(f.sockets(), 1);
    f.client.disconnect();
});

test("close before WebSocket open settles connect and retires its socket", async () => {
    const closed = [];
    const f = fixture(available(), "https:", {
        allowSockets: true,
        callbacks: { onClosed: (reason) => closed.push(reason) },
    });
    const pending = f.client.connect("observer");
    const rejected = assert.rejects(pending, /closed before opening/);
    await nextTurn();
    const socket = f.socketInstances[0];
    socket.peerClose("refused_before_open");
    await rejected;
    assert.equal(f.client.closed, true);
    assert.equal(f.client.controlSocket, null);
    assert.equal(f.client.sessionMeta, null);
    assert.deepEqual(closed, ["refused_before_open"]);
    socket.open();
    socket.peerClose("late_callback");
    assert.equal(socket.sent.length, 0);
    assert.deepEqual(closed, ["refused_before_open"]);
});

test("retired control and video socket callbacks cannot affect current owners", async () => {
    const events = [];
    const f = fixture(available(), "https:", {
        allowSockets: true,
        callbacks: {
            onClosed: () => events.push("closed"),
            onControlBinary: () => events.push("control"),
            onVideoBinary: () => events.push("video"),
            onVideoClosed: () => events.push("video_closed"),
        },
    });
    const first = f.client.connect("observer");
    await nextTurn();
    const oldControl = f.socketInstances[0];
    oldControl.open();
    await first;
    const second = f.client.connect("observer");
    await nextTurn();
    const control = f.socketInstances[1];
    control.open();
    await second;
    oldControl.open();
    oldControl.message(new ArrayBuffer(1));
    oldControl.peerClose();
    assert.equal(oldControl.sent.length, 1);
    assert.deepEqual(events, []);
    assert.equal(f.client.controlSocket, control);

    const session = new Uint8Array(16);
    const ticket = new Uint8Array(32);
    assert.equal(f.client.attachVideoChannel(session, ticket), true);
    const oldVideo = f.socketInstances[2];
    oldVideo.open();
    assert.equal(f.client.attachVideoChannel(session, ticket), true);
    const video = f.socketInstances[3];
    oldVideo.open();
    oldVideo.message(new ArrayBuffer(1));
    oldVideo.peerClose();
    assert.equal(oldVideo.sent.length, 1);
    assert.deepEqual(events, []);
    video.open();
    video.message(new ArrayBuffer(1));
    control.message(new ArrayBuffer(1));
    assert.deepEqual(events, ["video", "control"]);
    f.client.disconnect();
    assert.equal(f.client.attachVideoChannel(session, ticket), false);
    assert.equal(f.sockets(), 4);
});

test("an old bootstrap refusal cannot overwrite Disconnected or a newer Connecting UI", async () => {
    const firstBody = deferred();
    const secondBody = deferred();
    const ui = uiFixture((index) => index === 0 ? firstBody.promise : secondBody.promise);
    const first = ui.click("btn-connect");
    await nextTurn();
    ui.click("btn-disconnect");
    assert.equal(ui.element("connection-state").textContent, "Disconnected");
    const second = ui.click("btn-connect");
    await nextTurn();
    firstBody.resolve({ nonce_hex: "a5".repeat(32), desktop_available: false });
    await first;
    assert.equal(ui.element("connection-state").textContent, "Connecting...");
    assert.equal(ui.element("btn-connect").disabled, true);
    assert.equal(ui.element("btn-disconnect").disabled, false);
    assert.equal(ui.element("refusal-overlay").classList.contains("hidden"), true);
    secondBody.resolve({ nonce_hex: "a5".repeat(32), desktop_available: false });
    await second;
    assert.equal(ui.element("connection-state").textContent, "Refused");
    assert.equal(ui.element("btn-connect").disabled, false);
    assert.equal(ui.sockets(), 0);
});

test("pagehide and bfcache restoration cancel pending UI work and permit a fresh connect", async () => {
    const body = deferred();
    const ui = uiFixture((index) => index === 0 ? body.promise : {
        nonce_hex: "a5".repeat(32), desktop_available: false,
    });
    const pending = ui.click("btn-connect");
    await nextTurn();
    ui.event("pagehide");
    ui.event("pageshow", { persisted: true });
    assert.equal(ui.requests[0].options.signal.aborted, true);
    assert.equal(ui.element("btn-connect").disabled, false);
    assert.equal(ui.element("btn-disconnect").disabled, true);
    body.resolve({ nonce_hex: "a5".repeat(32), desktop_available: false });
    await pending;
    assert.equal(ui.element("connection-state").textContent, "Disconnected (bfcache_restore)");
    assert.equal(ui.element("refusal-overlay").classList.contains("hidden"), true);
    await ui.click("btn-connect");
    assert.equal(ui.requests.length, 2);
    assert.equal(ui.element("connection-state").textContent, "Refused");
    assert.equal(ui.sockets(), 0);
});

test("WebSocket open remains awaiting authorization and cannot enable observation controls", async () => {
    const ui = uiFixture(available());
    const pending = ui.click("btn-connect");
    await nextTurn();
    ui.socketInstances[0].open();
    await pending;
    assert.equal(ui.element("connection-state").textContent, "Awaiting session authorization");
    assert.equal(ui.element("btn-request-control").disabled, true);
    assert.equal(ui.element("btn-fullscreen").disabled, true);
    assert.equal(ui.element("btn-connect").disabled, true);
    assert.equal(ui.element("btn-disconnect").disabled, false);
    ui.socketInstances[0].peerClose();
    assert.equal(ui.element("btn-connect").disabled, false);
    assert.equal(ui.element("btn-disconnect").disabled, true);
});
