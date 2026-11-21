"use strict";
// I/O and DOM ownership with an instrumented adapter. wasm_transport.cjs runs
// this same I/O implementation against the real compiled Rust/WASM owner.
const assert = require("node:assert/strict");
const test = require("node:test");
const { available, deferred, nextTurn, ioFixture, uiFixture } = require("./browser_fixture.cjs");

async function open(client, fixture, index = 0) {
    const pending = client.connect("observer");
    await nextTurn();
    fixture.sockets[index].open();
    await pending;
}

test("an adapter bootstrap refusal prevents every socket and disposes its original owner", async () => {
    for (const availability of [false, undefined, "true", 1]) {
        const f = ioFixture(available({ desktop_available: availability }));
        const client = f.makeClient();
        await assert.rejects(client.connect("observer"), { code: "browser_session_unavailable" });
        assert.equal(f.sockets.length, 0);
        assert.equal(client.closed, true);
        assert.equal(f.cores[0].freed, 1);
        assert.equal(f.timers.size, 0);
        assert.equal(f.requests[0].url, "/api/session/bootstrap");
        assert.equal(f.requests[0].options.credentials, "same-origin");
        assert.equal(f.requests[0].options.cache, "no-store");
    }
});

test("plaintext bootstrap is refused before adapter creation or network traffic", async () => {
    const f = ioFixture(available(), { protocol: "http:" });
    await assert.rejects(f.makeClient().connect(), { code: "browser_https_required" });
    assert.equal(f.requests.length, 0);
    assert.equal(f.cores.length, 0);
    assert.equal(f.sockets.length, 0);
});

test("control socket sends exactly the opaque hello returned by the Rust boundary", async () => {
    const hello = Uint8Array.of(19, 2, 141, 9, 17);
    const f = ioFixture(available(), { hello });
    const client = f.makeClient();
    await open(client, f);
    assert.deepEqual(f.sockets[0].sent, [Array.from(hello)]);
    assert.equal(f.sockets[0].url, "wss://host.fixture.ts.net:8443/ws/control");
    assert.equal(f.cores[0].calls.filter((x) => x[0] === "clientHello").length, 1);
    assert.equal(f.requests[0].options.body, '{"role":"observer"}');
    assert.equal(typeof client.attachVideoChannel, "undefined");
    assert.equal(typeof client.sendReceiverPressure, "undefined");
    client.disconnect();
});

test("disconnect settles a pending body read without waiting for its late response", async () => {
    const body = deferred();
    const f = ioFixture(() => body.promise);
    const client = f.makeClient();
    const pending = client.connect();
    const rejection = assert.rejects(pending, { code: "browser_connection_cancelled" });
    await nextTurn();
    client.disconnect();
    await rejection;
    assert.equal(f.requests[0].options.signal.aborted, true);
    assert.equal(f.cores[0].freed, 1);
    body.resolve(available());
    await nextTurn();
    assert.equal(f.sockets.length, 0);
    assert.equal(f.cores[0].calls.some((x) => x[0] === "acceptBootstrap"), false);
    assert.equal(f.timers.size, 0);
});

test("a superseded bootstrap cannot overwrite a replacement owner even with reused local token", async () => {
    const body = deferred();
    const f = ioFixture((index) => index === 0 ? body.promise : available());
    const client = f.makeClient();
    const first = client.connect();
    const rejection = assert.rejects(first, { code: "browser_connection_cancelled" });
    await nextTurn();
    await open(client, f);
    const current = client.controlSocket;
    body.resolve(available());
    await rejection;
    await nextTurn();
    assert.equal(client.controlSocket, current);
    assert.equal(client.closed, false);
    assert.equal(f.sockets.length, 1);
    assert.equal(f.cores[0].freed, 1);
    assert.equal(f.cores[1].freed, 0);
    client.disconnect();
});

test("close before socket open settles connection and retires late socket callbacks", async () => {
    const closed = [];
    const f = ioFixture();
    const client = f.makeClient({ onClosed: (code) => closed.push(code) });
    const pending = client.connect();
    const rejection = assert.rejects(pending, { code: "browser_transport_closed" });
    await nextTurn();
    const socket = f.sockets[0];
    socket.peerClose();
    await rejection;
    socket.open();
    socket.message(new ArrayBuffer(1));
    socket.peerClose();
    assert.equal(client.closed, true);
    assert.equal(client.controlSocket, null);
    assert.equal(socket.sent.length, 0);
    assert.deepEqual(closed, ["browser_transport_closed"]);
    assert.equal(f.timers.size, 0);
});

test("retired socket messages and failures never reach the replacement Rust owner", async () => {
    const f = ioFixture();
    const client = f.makeClient();
    await open(client, f);
    const oldSocket = f.sockets[0];
    await open(client, f, 1);
    const current = f.sockets[1];
    oldSocket.open();
    oldSocket.message(new ArrayBuffer(1));
    oldSocket.peerClose();
    assert.equal(client.controlSocket, current);
    assert.equal(f.cores[1].calls.some((x) => x[0] === "receiveControl"), false);
    assert.equal(current.closeCalls.length, 0);
    assert.equal(oldSocket.sent.length, 1);
    client.disconnect();
});

test("control bytes go to Rust and a typed refusal closes only that connection", async () => {
    const refusals = [];
    const f = ioFixture(available(), { receiveControl: () => "browser_host_required_capability" });
    const client = f.makeClient({ onRefused: (error) => refusals.push(error) });
    await open(client, f);
    f.sockets[0].message(Uint8Array.of(91, 14, 5).buffer);
    assert.equal(client.closed, true);
    assert.deepEqual(f.cores[0].calls.find((x) => x[0] === "receiveControl")[2], [91, 14, 5]);
    assert.equal(refusals.length, 1);
    assert.equal(refusals[0].code, "browser_host_required_capability");
    assert.equal(f.cores[0].freed, 1);
    assert.equal(f.timers.size, 0);
});

test("bootstrap byte bounds are enforced before copying a body into WASM", async () => {
    for (const options of [{ contentLength: "5000" }, { rawBody: new Uint8Array(4097) }]) {
        const f = ioFixture(available(), options);
        await assert.rejects(f.makeClient().connect(), { code: "browser_bootstrap_too_large" });
        assert.equal(f.sockets.length, 0);
        assert.equal(f.cores[0].calls.some((x) => x[0] === "acceptBootstrap"), false);
        assert.equal(f.cores[0].freed, 1);
    }
});

test("control byte bounds are enforced before copying a record into WASM", async () => {
    const refused = [];
    const f = ioFixture();
    const client = f.makeClient({ onRefused: (error) => refused.push(error.code) });
    await open(client, f);
    f.sockets[0].message(new ArrayBuffer(4097));
    assert.equal(f.cores[0].calls.some((x) => x[0] === "receiveControl"), false);
    assert.deepEqual(refused, ["browser_control_too_large"]);
    assert.equal(client.closed, true);
});

test("the Rust deadline retires a silent bootstrap while its response remains pending", async () => {
    const body = deferred();
    const f = ioFixture(() => body.promise, { tick: () => { throw "browser_handshake_expired"; } });
    const client = f.makeClient();
    const pending = client.connect();
    const rejection = assert.rejects(pending, { code: "browser_handshake_expired" });
    await nextTurn();
    f.tick(200);
    await rejection;
    assert.equal(f.requests[0].options.signal.aborted, true);
    assert.equal(f.cores[0].freed, 1);
    assert.equal(f.timers.size, 0);
    body.resolve(available());
});

test("terminal lifecycle retires transport immediately and pre-connect visibility reaches Rust", async () => {
    const f = ioFixture();
    const client = f.makeClient();
    client.lifecycle("hidden");
    client.lifecycle("focus_lost");
    await open(client, f);
    assert.equal(f.cores[0].visible, false);
    assert.equal(f.cores[0].focused, false);
    client.lifecycle("pagehide");
    assert.equal(client.closed, true);
    assert.equal(f.sockets[0].closeCalls.length, 1);
    assert.equal(f.cores[0].freed, 1);
    assert.equal(f.timers.size, 0);
});

test("disconnect while WASM is loading settles promptly and frees a late adapter", async () => {
    const loaded = deferred();
    let freed = 0;
    const f = ioFixture(available(), { createAdapter: () => loaded.promise });
    const client = f.makeClient();
    const pending = client.connect();
    const rejection = assert.rejects(pending, { code: "browser_connection_cancelled" });
    await nextTurn();
    client.disconnect();
    await rejection;
    loaded.resolve({ core: { free() { freed += 1; } }, maxBootstrapBytes: 4096, maxControlRecordBytes: 4096 });
    await nextTurn();
    assert.equal(freed, 1);
    assert.equal(f.requests.length, 0);
    assert.equal(f.sockets.length, 0);
});

test("a reentrant state callback can retire startup before fetch or timer creation", async () => {
    const f = ioFixture();
    let client;
    client = f.makeClient({ onState() { client.disconnect(); } });
    await assert.rejects(client.connect(), { code: "browser_connection_cancelled" });
    assert.equal(f.requests.length, 0);
    assert.equal(f.sockets.length, 0);
    assert.equal(f.timers.size, 0);
    assert.equal(f.cores[0].freed, 1);
});

test("an old bootstrap refusal cannot overwrite a replacement Connecting UI", async () => {
    const firstBody = deferred(), secondBody = deferred();
    const ui = uiFixture((index) => index === 0 ? firstBody.promise : secondBody.promise);
    const first = ui.click("btn-connect");
    await nextTurn();
    ui.click("btn-disconnect");
    const second = ui.click("btn-connect");
    await nextTurn();
    firstBody.resolve(available({ desktop_available: false }));
    await first;
    assert.equal(ui.element("connection-state").textContent, "Connecting...");
    assert.equal(ui.element("refusal-overlay").classList.contains("hidden"), true);
    secondBody.resolve(available({ desktop_available: false }));
    await second;
    assert.equal(ui.element("connection-state").textContent, "Refused");
    assert.equal(ui.element("btn-connect").disabled, false);
    assert.equal(ui.sockets.length, 0);
});

test("pagehide and bfcache restoration cancel pending UI work and allow a fresh attempt", async () => {
    const body = deferred();
    const ui = uiFixture((index) => index === 0 ? body.promise : available({ desktop_available: false }));
    const pending = ui.click("btn-connect");
    await nextTurn();
    ui.event("pagehide");
    ui.event("pageshow", { persisted: true });
    await pending;
    assert.equal(ui.requests[0].options.signal.aborted, true);
    assert.equal(ui.element("btn-connect").disabled, false);
    assert.equal(ui.element("btn-disconnect").disabled, true);
    body.resolve(available({ desktop_available: false }));
    await nextTurn();
    assert.equal(ui.element("connection-state").textContent, "Disconnected");
    assert.equal(ui.element("refusal-overlay").classList.contains("hidden"), true);
    await ui.click("btn-connect");
    assert.equal(ui.element("connection-state").textContent, "Refused");
});

test("socket open and arbitrary media bytes cannot enable observation or control", async () => {
    const ui = uiFixture(available());
    const pending = ui.click("btn-connect");
    await nextTurn();
    ui.sockets[0].open();
    await pending;
    assert.equal(ui.element("connection-state").textContent, "Awaiting session authorization");
    for (const button of ["btn-request-control", "btn-release-control", "btn-fullscreen"]) {
        assert.equal(ui.element(button).disabled, true);
    }
    // No JS FRTK/media parser may infer a usable session from unvalidated bytes.
    ui.sockets[0].message(Uint8Array.of(70, 82, 84, 75, 1).buffer);
    assert.equal(ui.element("connection-state").textContent, "Refused");
    assert.equal(ui.element("freshness-badge").textContent, "View: Unverified");
    assert.equal(ui.element("btn-request-control").disabled, true);
});


test("a stalled browser package load expires before a Rust adapter exists", async () => {
    const loading = deferred();
    const f = ioFixture(available(), { createAdapter: () => loading.promise });
    const refusals = [];
    const client = f.makeClient({ onRefused: (error) => refusals.push(error.code) });
    const pending = client.connect("observer");
    const expired = assert.rejects(pending, { code: "browser_client_load_timeout" });
    await nextTurn();
    assert.equal(f.timers.size, 1);
    f.tick(15099);
    assert.equal(client.closed, false);
    f.tick(15100);
    await expired;
    assert.equal(client.closed, true);
    assert.equal(f.timers.size, 0);
    assert.equal(f.requests.length, 0);
    assert.equal(f.sockets.length, 0);
    assert.deepEqual(refusals, ["browser_client_load_timeout"]);

    let freed = 0;
    loading.resolve({ core: { free() { freed += 1; } }, maxBootstrapBytes: 4096, maxControlRecordBytes: 4096 });
    await nextTurn();
    assert.equal(freed, 1, "a late package result disposes only its original adapter");
    assert.equal(f.requests.length, 0);
});

test("a retired loading watchdog cannot close its replacement connection", async () => {
    const loading = deferred();
    const f = ioFixture();
    const createAdapter = f.window.FrankenRust.createAdapter;
    let attempts = 0;
    f.window.FrankenRust.createAdapter = () => attempts++ === 0 ? loading.promise : createAdapter();
    const refusals = [];
    const client = f.makeClient({ onRefused: (error) => refusals.push(error.code) });
    const old = client.connect("observer");
    const cancelled = assert.rejects(old, { code: "browser_connection_cancelled" });
    await nextTurn();
    const staleWatchdog = Array.from(f.timers.values())[0];

    f.tick(15000);
    await open(client, f);
    await cancelled;
    const replacement = client.controlSocket;
    f.tick(15100);
    staleWatchdog();
    assert.equal(client.controlSocket, replacement);
    assert.equal(client.closed, false);
    assert.deepEqual(refusals, []);

    let freed = 0;
    loading.resolve({ core: { free() { freed += 1; } }, maxBootstrapBytes: 4096, maxControlRecordBytes: 4096 });
    await nextTurn();
    assert.equal(freed, 1);
    assert.equal(f.cores[0].freed, 0);
    assert.equal(f.timers.size, 1, "only the replacement Rust deadline remains");
    client.disconnect();
    assert.equal(f.timers.size, 0);
});

test("loading the browser package never resets the original Rust start time", async () => {
    const loading = deferred();
    const f = ioFixture();
    const createAdapter = f.window.FrankenRust.createAdapter;
    f.window.FrankenRust.createAdapter = () => loading.promise;
    const client = f.makeClient();
    const pending = client.connect("observer");
    await nextTurn();
    f.tick(5000);
    loading.resolve(await createAdapter());
    await nextTurn();
    assert.deepEqual(f.cores[0].calls.find((call) => call[0] === "begin"), ["begin", "observer", 100]);
    assert.equal(f.cores[0].calls.find((call) => call[0] === "acceptBootstrap")[3], 5000);
    assert.equal(f.timers.size, 1, "successful loading replaces the I/O watchdog with Rust ticking");
    f.sockets[0].open();
    await pending;
    assert.equal(f.cores[0].calls.find((call) => call[0] === "clientHello")[2], 5000);
    client.disconnect();
    assert.equal(f.timers.size, 0);
});
