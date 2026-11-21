"use strict";

// Explicit integration suite: real generated Rust/WASM protocol and session
// exports through production wss_client.js, with simulated browser I/O/DOM.
// Run with FR_WEB_WASM_PACKAGE pointing at wasm-bindgen's --target web output:
//   node --test web/tests/wasm_transport.cjs
// Positive desktop availability below is synthetic. These tests do not qualify
// a live browser, tailnet listener, desktop media, or input-control capability.
const assert = require("node:assert/strict");
const fs = require("node:fs/promises");
const path = require("node:path");
const { pathToFileURL } = require("node:url");
const { before, afterEach, test } = require("node:test");
const { available, deferred, nextTurn, ioFixture, uiFixture } = require("./browser_fixture.cjs");

let bindings;
const cleanups = [];

before(async () => {
    const directory = process.env.FR_WEB_WASM_PACKAGE;
    assert.ok(directory, "FR_WEB_WASM_PACKAGE must name an actual generated web package");
    bindings = await import(pathToFileURL(path.resolve(directory, "fr_web.js")).href);
    const bytes = await fs.readFile(path.join(directory, "fr_web_bg.wasm"));
    assert.equal(WebAssembly.validate(bytes), true);
    const exports = await bindings.default({ module_or_path: bytes });
    assert.ok(exports.memory instanceof WebAssembly.Memory);
    assert.equal(typeof exports.browseradapter_new, "function");
    assert.equal(bindings.browserAbiVersion(), 1);
    assert.equal(bindings.maxBootstrapBytes(), 4096);
    assert.equal(bindings.maxControlRecordBytes(), 4096);
});

afterEach(() => {
    for (const cleanup of cleanups.splice(0)) cleanup();
});

// Independent protocol goldens, shared with wasm_adapter.cjs. The offer-role
// byte at offset 33 is the only change for a controller request. No production
// encoder or simulated protocol implementation constructs these expectations.
const OBSERVER_HELLO = Buffer.from((
    "46524430 0000 0001 00000000 00000050 00000000 00000000" +
    "00000001 0000 03 0000 00" +
    "00010000 00100000 01000000 00002000 0000000001000000" +
    "0c 0000000002000000 00000000 01" +
    "a5".repeat(32)
).replace(/\s/g, ""), "hex");
const REQUIRED_CAPABILITY = Buffer.from(
    "46524430000000040000000000000004000000000000000000040000", "hex");

function noAuthority(snapshot) {
    assert.equal(snapshot.view_fresh, false);
    assert.equal(snapshot.can_request_control, false);
    assert.equal(snapshot.is_controlling, false);
    return snapshot;
}

function wireBuffer(bytes) {
    // Node Buffer may share a larger backing slab. Deliver exactly this record.
    return Uint8Array.from(bytes).buffer;
}

function callCount(owner, name) {
    return owner.calls.filter((call) => call.name === name).length;
}

// Instrument ownership without implementing or replacing any Rust decision.
// Every method forwards to the actual wasm-bindgen BrowserAdapter instance.
function rustFixture(response = available(), options = {}, fixture = ioFixture) {
    const owners = [];
    const { load, ...ioOptions } = options;
    const createBinding = () => {
        const adapter = new bindings.BrowserAdapter();
        const owner = { adapter, calls: [], errors: [], freed: 0, final: null };
        owners.push(owner);
        const core = {};
        for (const name of [
            "begin", "bootstrapRequest", "acceptBootstrap", "clientHello",
            "receiveControl", "lifecycle", "tick", "disconnect", "snapshot",
        ]) {
            core[name] = (...args) => {
                owner.calls.push({ name, args });
                try { return adapter[name](...args); }
                catch (error) {
                    owner.errors.push({ name, error });
                    throw error;
                }
            };
        }
        core.free = () => {
            owner.freed += 1;
            assert.equal(owner.freed, 1, "each actual Rust adapter is freed exactly once");
            owner.final = noAuthority(JSON.parse(adapter.snapshot()));
            adapter.free();
        };
        return {
            core,
            maxBootstrapBytes: bindings.maxBootstrapBytes(),
            maxControlRecordBytes: bindings.maxControlRecordBytes(),
        };
    };
    const f = fixture(response, {
        ...ioOptions,
        createAdapter: () => load ? load(createBinding) : createBinding(),
    });
    if (fixture === uiFixture) cleanups.push(() => f.click("btn-disconnect"));
    return {
        ...f,
        owners,
        makeClient(callbacks) {
            const client = f.makeClient(callbacks);
            cleanups.push(() => client.disconnect());
            return client;
        },
    };
}

function disposed(f) {
    assert.equal(f.cores.length, 0, "the default fixture Rust stub was never constructed");
    assert.equal(f.timers.size, 0);
    for (const owner of f.owners) {
        assert.equal(owner.freed, 1);
        noAuthority(owner.final);
    }
}

async function open(client, f, role = "observer", index = f.sockets.length) {
    const pending = client.connect(role);
    await nextTurn();
    assert.ok(f.sockets[index], "bootstrap must produce the expected control socket");
    f.sockets[index].open();
    await pending;
    return f.sockets[index];
}

test("production transport sends the actual Rust hello for both requested roles", async () => {
    for (const role of ["observer", "controller"]) {
        const f = rustFixture(available({ role }));
        const states = [];
        const client = f.makeClient({ onState: (snapshot) => states.push(noAuthority(snapshot)) });
        const socket = await open(client, f, role);
        const expected = Buffer.from(OBSERVER_HELLO);
        if (role === "controller") expected[33] = 1;
        assert.equal(expected.length, 104);
        assert.equal(socket.sent.length, 1);
        assert.deepEqual(Buffer.from(socket.sent[0]), expected);
        assert.equal(f.requests[0].url, "/api/session/bootstrap");
        assert.equal(f.requests[0].options.body, JSON.stringify({ role }));
        assert.equal(f.requests[0].options.credentials, "same-origin");
        assert.equal(f.requests[0].options.cache, "no-store");
        assert.equal(socket.url, "wss://host.fixture.ts.net:8443/ws/control");
        assert.equal(states.at(-1).state_label, "awaiting_host");
        assert.equal(callCount(f.owners[0], "clientHello"), 1);
        assert.equal(f.owners[0].adapter instanceof bindings.BrowserAdapter, true);
        client.disconnect();
        disposed(f);
    }
});

test("actual Rust capability refusal stops transport and keeps bearer text out of errors", async () => {
    for (const response of [
        { desktop_available: false, nonce_hex: "private-bearer", refusal_code: "private-peer-text" },
        { nonce_hex: "private-bearer" },
    ]) {
        const f = rustFixture(response);
        const client = f.makeClient();
        await assert.rejects(client.connect("observer"), (error) => {
            assert.equal(error.code, "browser_session_unavailable");
            assert.equal(error.message.includes("private"), false);
            return true;
        });
        assert.equal(f.sockets.length, 0);
        assert.equal(callCount(f.owners[0], "clientHello"), 0);
        assert.equal(f.owners[0].final.refusal_code, "browser_session_unavailable");
        assert.equal(JSON.stringify(f.owners[0].final).includes("private"), false);
        disposed(f);
    }
});

test("canonical host refusal retires the real Rust token before JS safely frees and closes", async () => {
    const f = rustFixture();
    const refusals = [], states = [];
    const client = f.makeClient({
        onState: (snapshot) => states.push(noAuthority(snapshot)),
        onRefused: (error) => refusals.push(error),
    });
    const socket = await open(client, f);
    socket.message(wireBuffer(REQUIRED_CAPABILITY));
    assert.equal(client.closed, true);
    assert.equal(socket.closeCalls.length, 1);
    assert.equal(refusals.length, 1);
    assert.equal(refusals[0].code, "host_required_capability_missing");
    assert.equal(states.at(-1).state_label, "refused");
    assert.equal(states.at(-1).attempt, null);
    assert.equal(f.owners[0].final.refusal_code, "host_required_capability_missing");
    assert.ok(f.owners[0].errors.some((entry) =>
        entry.name === "disconnect" && entry.error === "browser_connection_cancelled"));
    socket.open();
    socket.message(wireBuffer(REQUIRED_CAPABILITY));
    socket.peerClose();
    assert.equal(callCount(f.owners[0], "receiveControl"), 1);
    assert.equal(refusals.length, 1);
    disposed(f);
});

test("malformed or unsupported control records are refused by actual Rust", async () => {
    const wrongBinding = Buffer.from(REQUIRED_CAPABILITY);
    wrongBinding[19] = 7;
    const cases = [
        [REQUIRED_CAPABILITY.subarray(0, 27), "browser_control_record_invalid"],
        [Buffer.concat([REQUIRED_CAPABILITY, Buffer.from([0])]), "browser_control_record_invalid"],
        [wrongBinding, "browser_control_record_invalid"],
        [Buffer.from("FRBA"), "browser_control_record_invalid"],
        [OBSERVER_HELLO, "browser_control_record_unsupported"],
    ];
    for (const [bytes, code] of cases) {
        const f = rustFixture();
        const refusals = [];
        const client = f.makeClient({ onRefused: (error) => refusals.push(error.code) });
        const socket = await open(client, f);
        socket.message(wireBuffer(bytes));
        assert.deepEqual(refusals, [code]);
        assert.equal(callCount(f.owners[0], "receiveControl"), 1);
        assert.equal(f.owners[0].final.refusal_code, code);
        assert.equal(client.closed, true);
        disposed(f);
    }
});

test("bootstrap metadata is validated by Rust instead of becoming JS session authority", async () => {
    const cases = [
        [available({ profile_id: 1 }), {}, "browser_transport_profile_unsupported"],
        [available({ role: "controller" }), {}, "browser_bootstrap_invalid"],
        [available({ session_id_hex: "ff".repeat(16) }), {}, "browser_bootstrap_invalid"],
        [available(), { rawBody: new TextEncoder().encode('{"desktop_available":false,"desktop_available":true}') }, "browser_bootstrap_invalid"],
    ];
    for (const [response, options, code] of cases) {
        const f = rustFixture(response, options);
        const client = f.makeClient();
        await assert.rejects(client.connect("observer"), { code });
        assert.equal(callCount(f.owners[0], "acceptBootstrap"), 1);
        assert.equal(f.sockets.length, 0);
        assert.equal(f.owners[0].final.refusal_code, code);
        disposed(f);
    }
});

test("oversized body and control input are rejected before copying into actual WASM", async () => {
    const bodyFixture = rustFixture(available(), { rawBody: new Uint8Array(4097) });
    const bodyClient = bodyFixture.makeClient();
    await assert.rejects(bodyClient.connect("observer"), { code: "browser_bootstrap_too_large" });
    assert.equal(callCount(bodyFixture.owners[0], "acceptBootstrap"), 0);
    assert.equal(bodyFixture.sockets.length, 0);
    disposed(bodyFixture);

    const f = rustFixture();
    const refusals = [];
    const client = f.makeClient({ onRefused: (error) => refusals.push(error.code) });
    const socket = await open(client, f);
    socket.message(new ArrayBuffer(4097));
    assert.deepEqual(refusals, ["browser_control_too_large"]);
    assert.equal(callCount(f.owners[0], "receiveControl"), 0);
    assert.equal(socket.closeCalls.length, 1);
    disposed(f);
});

test("cancelled body completion cannot reach or overwrite a replacement Rust adapter", { timeout: 3000 }, async () => {
    const body = deferred();
    const f = rustFixture((index) => index === 0 ? body.promise : available());
    const refusals = [];
    const client = f.makeClient({ onRefused: (error) => refusals.push(error.code) });
    const old = client.connect("observer");
    const cancelled = assert.rejects(old, { code: "browser_connection_cancelled" });
    await nextTurn();
    const replacement = await open(client, f);
    await cancelled;
    assert.equal(f.owners[0].freed, 1);
    assert.equal(f.requests[0].options.signal.aborted, true);
    assert.equal(callCount(f.owners[0], "acceptBootstrap"), 0);
    const before = f.owners[1].adapter.snapshot();
    assert.equal(JSON.parse(before).attempt, 1, "separate Rust adapters may reuse their local serial");
    body.resolve({ desktop_available: false, nonce_hex: "private-old-bearer" });
    await nextTurn();
    assert.equal(f.owners[1].adapter.snapshot(), before);
    assert.equal(client.controlSocket, replacement);
    assert.equal(client.closed, false);
    assert.equal(f.sockets.length, 1);
    assert.deepEqual(refusals, []);
    client.disconnect();
    disposed(f);
});

test("retired socket callbacks cannot mutate a replacement Rust owner", async () => {
    const f = rustFixture();
    const client = f.makeClient();
    const old = await open(client, f);
    const current = await open(client, f);
    const expected = f.owners[1].adapter.snapshot();
    old.open();
    old.message(wireBuffer(REQUIRED_CAPABILITY));
    old.message(wireBuffer(OBSERVER_HELLO));
    old.peerClose();
    if (old.onerror) old.onerror();
    assert.equal(client.controlSocket, current);
    assert.equal(client.closed, false);
    assert.equal(f.owners[1].adapter.snapshot(), expected);
    assert.equal(callCount(f.owners[1], "receiveControl"), 0);
    assert.equal(old.sent.length, 1);
    assert.equal(f.owners[0].freed, 1);
    assert.equal(f.owners[1].freed, 0);
    client.disconnect();
    disposed(f);
});

test("the real Rust handshake deadline settles an indefinitely pending response body", { timeout: 3000 }, async () => {
    const body = deferred();
    const f = rustFixture(() => body.promise);
    const refusals = [];
    const client = f.makeClient({ onRefused: (error) => refusals.push(error.code) });
    const pending = client.connect("observer");
    const expired = assert.rejects(pending, { code: "browser_handshake_expired" });
    await nextTurn();
    f.tick(10099);
    assert.equal(client.closed, false);
    f.tick(10100);
    await expired;
    assert.deepEqual(refusals, ["browser_handshake_expired"]);
    assert.equal(f.requests[0].options.signal.aborted, true);
    assert.equal(f.owners[0].final.refusal_code, "browser_handshake_expired");
    assert.equal(f.sockets.length, 0);
    disposed(f);
    body.resolve(available());
    await nextTurn();
    assert.equal(callCount(f.owners[0], "acceptBootstrap"), 0);
});

test("bootstrap expiry shortens Rust's original budget while the socket is still opening", { timeout: 3000 }, async () => {
    const f = rustFixture(available({ expires_in_ms: 1000 }));
    const client = f.makeClient();
    const pending = client.connect("observer");
    const expired = assert.rejects(pending, { code: "browser_handshake_expired" });
    await nextTurn();
    assert.equal(f.sockets.length, 1);
    f.tick(1099);
    assert.equal(client.closed, false);
    f.tick(1100);
    await expired;
    assert.equal(f.sockets[0].closeCalls.length, 1);
    f.sockets[0].open();
    assert.equal(f.sockets[0].sent.length, 0);
    assert.equal(callCount(f.owners[0], "clientHello"), 0);
    assert.equal(f.owners[0].final.refusal_code, "browser_handshake_expired");
    disposed(f);
});

test("actual Rust includes package loading time in its original connection deadline", { timeout: 3000 }, async () => {
    const loading = deferred();
    const f = rustFixture(available(), { load: (create) => loading.promise.then(create) });
    const client = f.makeClient();
    const pending = client.connect("observer");
    const expired = assert.rejects(pending, { code: "browser_handshake_expired" });
    await nextTurn();
    f.tick(10100);
    assert.equal(f.owners.length, 0);
    assert.equal(client.closed, false, "the independent package watchdog has not elapsed");
    loading.resolve();
    await expired;
    assert.equal(f.owners.length, 1);
    assert.equal(f.owners[0].calls.find((call) => call.name === "begin").args[1], 100);
    assert.equal(f.owners[0].final.refusal_code, "browser_handshake_expired");
    assert.equal(f.requests.length, 0);
    assert.equal(f.sockets.length, 0);
    disposed(f);
});

test("cancelling package loading frees a late actual adapter without beginning a Rust attempt", { timeout: 3000 }, async () => {
    const loading = deferred();
    const f = rustFixture(available(), { load: (create) => loading.promise.then(create) });
    const client = f.makeClient();
    const pending = client.connect("observer");
    const cancelled = assert.rejects(pending, { code: "browser_connection_cancelled" });
    await nextTurn();
    client.disconnect();
    await cancelled;
    assert.equal(f.owners.length, 0);
    loading.resolve();
    await nextTurn();
    assert.equal(f.owners.length, 1);
    assert.equal(callCount(f.owners[0], "begin"), 0);
    assert.equal(f.owners[0].final.state_label, "disconnected");
    assert.equal(f.requests.length, 0);
    disposed(f);
});

test("visibility and terminal lifecycle events reach actual Rust and retire transport immediately", async () => {
    for (const event of ["pagehide", "bfcache_restore"]) {
        const f = rustFixture();
        const states = [];
        const client = f.makeClient({ onState: (snapshot) => states.push(noAuthority(snapshot)) });
        client.lifecycle("hidden");
        client.lifecycle("focus_lost");
        const socket = await open(client, f);
        assert.equal(states.at(-1).visible, false);
        assert.equal(states.at(-1).focused, false);
        client.lifecycle("visible");
        client.lifecycle("focus_gained");
        assert.equal(states.at(-1).visible, true);
        assert.equal(states.at(-1).focused, true);
        client.lifecycle(event);
        assert.equal(client.closed, true);
        assert.equal(states.at(-1).state_label, "closed");
        assert.equal(states.at(-1).visible, false);
        assert.equal(states.at(-1).attempt, null);
        assert.equal(socket.closeCalls.length, 1);
        assert.equal(f.owners[0].final.state_label, "closed");
        socket.message(wireBuffer(REQUIRED_CAPABILITY));
        assert.equal(callCount(f.owners[0], "receiveControl"), 0);
        disposed(f);
    }
});

test("a reentrant real-Rust refusal projection cannot tear down its replacement", async () => {
    const f = rustFixture();
    const refusals = [];
    let client, replacement;
    client = f.makeClient({
        onState(snapshot) {
            noAuthority(snapshot);
            if (snapshot.state_label === "refused") replacement = client.connect("observer");
        },
        onRefused: (error) => refusals.push(error.code),
    });
    const old = await open(client, f);
    old.message(wireBuffer(REQUIRED_CAPABILITY));
    await nextTurn();
    assert.ok(replacement);
    assert.equal(f.sockets.length, 2);
    f.sockets[1].open();
    await replacement;
    assert.equal(client.controlSocket, f.sockets[1]);
    assert.equal(client.closed, false);
    assert.equal(f.owners[0].freed, 1);
    assert.equal(f.owners[1].freed, 0);
    assert.equal(JSON.parse(f.owners[1].adapter.snapshot()).state_label, "awaiting_host");
    assert.deepEqual(refusals, []);
    client.disconnect();
    disposed(f);
});

test("production DOM stays view-only through actual Rust startup and typed host refusal", async () => {
    const f = rustFixture(available(), {}, uiFixture);
    const pending = f.click("btn-connect");
    await nextTurn();
    assert.equal(f.sockets.length, 1);
    f.sockets[0].open();
    await pending;
    assert.equal(f.element("connection-state").textContent, "Awaiting session authorization");
    assert.equal(f.element("freshness-badge").textContent, "View: Unverified");
    assert.equal(f.element("authority-badge").textContent, "Role: View-Only");
    for (const id of ["btn-request-control", "btn-release-control", "btn-fullscreen"]) {
        assert.equal(f.element(id).disabled, true);
    }
    f.sockets[0].message(wireBuffer(REQUIRED_CAPABILITY));
    assert.equal(f.element("connection-state").textContent, "Refused");
    assert.equal(f.element("freshness-badge").textContent, "View: Unverified");
    assert.equal(f.element("btn-request-control").disabled, true);
    assert.equal(f.owners[0].final.refusal_code, "host_required_capability_missing");
    disposed(f);
});
