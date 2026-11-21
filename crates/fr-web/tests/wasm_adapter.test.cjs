"use strict";

// Executes the real compiled Rust exports through wasm-bindgen's --target web
// glue. Browser/tailnet/decoder I/O is not supplied or qualified by this test.
// Run after building the package, with FR_WEB_WASM_PACKAGE set to its directory.
const assert = require("node:assert/strict");
const fs = require("node:fs/promises");
const path = require("node:path");
const { pathToFileURL } = require("node:url");
const { before, test } = require("node:test");

let bindings;
let rawExports;

before(async () => {
    const directory = process.env.FR_WEB_WASM_PACKAGE;
    assert.ok(directory, "FR_WEB_WASM_PACKAGE must name an actual generated web package");
    bindings = await import(pathToFileURL(path.resolve(directory, "fr_web.js")).href);
    const bytes = await fs.readFile(path.join(directory, "fr_web_bg.wasm"));
    assert.equal(WebAssembly.validate(bytes), true);
    rawExports = await bindings.default({ module_or_path: bytes });
});

function withAdapter(run) {
    const adapter = new bindings.BrowserAdapter();
    try { return run(adapter); }
    finally { adapter.free(); }
}

function bootstrap(role = "observer", overrides = {}) {
    return JSON.stringify({
        nonce_hex: "a5".repeat(32),
        expires_in_ms: 15_000,
        transport_profile: "wss_degraded_tcp_hol_dedicated",
        profile_id: 3,
        profile_version: 0,
        role,
        desktop_available: true, // Synthetic positive capability, not the current host.
        ...overrides,
    });
}

function awaitingHost(adapter, now = 10) {
    const attempt = adapter.begin("observer", now);
    adapter.acceptBootstrap(attempt, bootstrap(), now + 1);
    adapter.clientHello(attempt, now + 2);
    return attempt;
}

function fails(code, run) {
    assert.throws(run, (error) => {
        assert.equal(typeof error, "string");
        assert.equal(error, code);
        assert.ok(error.length < 64);
        return true;
    });
}

function noAuthority(adapter) {
    const snapshot = JSON.parse(adapter.snapshot());
    assert.equal(snapshot.view_fresh, false);
    assert.equal(snapshot.can_request_control, false);
    assert.equal(snapshot.is_controlling, false);
    return snapshot;
}

// Independently written from PROTOCOL_BROWSER_BOOTSTRAP.md / the shared limits,
// not an invocation of the encoder under test. The only role-dependent byte is
// the fixed offer-role field at offset 33. These are test fixtures, not JS wire
// implementation for the application.
const OBSERVER_HELLO = Buffer.from((`
    46524430 0000 0001 00000000 00000050 00000000 00000000
    00000001 0000 03 0000 00
    00010000 00100000 01000000 00002000 0000000001000000
    0c 0000000002000000 00000000 01
` + "a5".repeat(32)).replace(/\s/g, ""), "hex");

// PROTOCOL_REFUSAL.md's 28-byte, zero-bound record, required capability reason 4.
const REQUIRED_CAPABILITY = Buffer.from(
    "46524430000000040000000000000004000000000000000000040000", "hex");

test("the actual module exposes versioned Rust bindings and linear memory", () => {
    assert.ok(rawExports.memory instanceof WebAssembly.Memory);
    assert.equal(typeof rawExports.browseradapter_new, "function");
    assert.equal(bindings.browserAbiVersion(), 1);
    assert.equal(bindings.maxBootstrapBytes(), 4096);
    assert.equal(bindings.maxControlRecordBytes(), 4096);
    withAdapter((adapter) => {
        assert.equal(JSON.parse(adapter.snapshot()).state_label, "disconnected");
        noAuthority(adapter);
    });
});

test("real WASM emits exact canonical hello bytes for both requested roles", () => {
    for (const role of ["observer", "controller"]) {
        withAdapter((adapter) => {
            const attempt = adapter.begin(role, 10);
            assert.equal(attempt, 1);
            assert.equal(adapter.bootstrapRequest(attempt), JSON.stringify({ role }));
            adapter.acceptBootstrap(attempt, bootstrap(role), 11);
            assert.equal(JSON.parse(adapter.snapshot()).state_label, "awaiting_transport");
            const hello = adapter.clientHello(attempt, 12);
            assert.ok(hello instanceof Uint8Array);
            const expected = Buffer.from(OBSERVER_HELLO);
            if (role === "controller") expected[33] = 1;
            assert.equal(expected.length, 104);
            assert.deepEqual(Buffer.from(hello), expected);
            assert.equal(noAuthority(adapter).state_label, "awaiting_host");
            fails("browser_connection_state_invalid", () => adapter.clientHello(attempt, 13));
            assert.equal(noAuthority(adapter).attempt, null);
        });
    }
});

test("unavailable or absent desktop capability refuses before hello and omits bearer data", () => {
    for (const response of [
        { desktop_available: false, nonce_hex: "private-bearer", refusal_code: "private-peer-text" },
        { nonce_hex: "private-bearer" },
    ]) {
        withAdapter((adapter) => {
            const attempt = adapter.begin("observer", 10);
            fails("browser_session_unavailable", () => adapter.acceptBootstrap(attempt, JSON.stringify(response), 11));
            const snapshot = noAuthority(adapter);
            assert.equal(snapshot.attempt, null);
            assert.equal(snapshot.refusal_code, "browser_session_unavailable");
            assert.equal(adapter.snapshot().includes("private"), false);
            assert.equal(adapter.snapshot().includes("nonce"), false);
            fails("browser_connection_cancelled", () => adapter.clientHello(attempt, 12));
        });
    }
});

test("real WASM decodes the canonical typed host refusal and closes its attempt", () => {
    withAdapter((adapter) => {
        const attempt = awaitingHost(adapter);
        assert.equal(adapter.receiveControl(attempt, REQUIRED_CAPABILITY, 13), "host_required_capability_missing");
        const snapshot = noAuthority(adapter);
        assert.equal(snapshot.state_label, "refused");
        assert.equal(snapshot.refusal_code, "host_required_capability_missing");
        assert.equal(snapshot.attempt, null);
        fails("browser_connection_cancelled", () => adapter.receiveControl(attempt, REQUIRED_CAPABILITY, 14));
    });
});

test("stale attempts cannot mutate replacements through bytes, lifecycle or invalid clocks", () => {
    withAdapter((adapter) => {
        const old = awaitingHost(adapter);
        const current = adapter.begin("observer", 20);
        assert.ok(current > old);
        const expected = adapter.snapshot();
        fails("browser_connection_cancelled", () => adapter.acceptBootstrap(old, "invalid", 0));
        fails("browser_connection_cancelled", () => adapter.receiveControl(old, REQUIRED_CAPABILITY, NaN));
        fails("browser_connection_cancelled", () => adapter.lifecycle(old, "pagehide", 0));
        fails("browser_connection_cancelled", () => adapter.disconnect(old));
        assert.equal(adapter.snapshot(), expected);
        adapter.acceptBootstrap(current, bootstrap(), 21);
        adapter.clientHello(current, 22);
        assert.equal(noAuthority(adapter).state_label, "awaiting_host");
    });
});

test("lifecycle events reach the Rust core and pagehide or bfcache retires pending work", () => {
    for (const closeEvent of ["pagehide", "bfcache_restore"]) {
        withAdapter((adapter) => {
            const attempt = awaitingHost(adapter);
            adapter.lifecycle(attempt, "hidden", 13);
            adapter.lifecycle(attempt, "focus_lost", 14);
            assert.equal(noAuthority(adapter).visible, false);
            assert.equal(noAuthority(adapter).focused, false);
            adapter.lifecycle(attempt, "visible", 15);
            adapter.lifecycle(attempt, "focus_gained", 16);
            assert.equal(noAuthority(adapter).visible, true);
            assert.equal(noAuthority(adapter).focused, true);
            adapter.lifecycle(attempt, closeEvent, 17);
            const closed = noAuthority(adapter);
            assert.equal(closed.attempt, null);
            assert.equal(closed.state_label, "closed");
            assert.equal(closed.visible, false);
            fails("browser_connection_cancelled", () => adapter.receiveControl(attempt, REQUIRED_CAPABILITY, 18));
        });
    }
});

test("handshake progress and delayed bootstrap cannot restart the original expiry", () => {
    withAdapter((adapter) => {
        const attempt = awaitingHost(adapter);
        adapter.tick(attempt, 9000);
        fails("browser_handshake_expired", () => adapter.tick(attempt, 10010));
        assert.equal(noAuthority(adapter).refusal_code, "browser_handshake_expired");
    });
    withAdapter((adapter) => {
        const attempt = adapter.begin("observer", 10);
        fails("browser_handshake_expired", () => adapter.acceptBootstrap(attempt, bootstrap("observer", { expires_in_ms: 1000 }), 1010));
        assert.equal(noAuthority(adapter).attempt, null);
    });
});

test("invalid browser times are typed terminal failures instead of saturating conversions", () => {
    for (const now of [NaN, Infinity, -1, Number.MAX_VALUE]) {
        withAdapter((adapter) => {
            const attempt = awaitingHost(adapter);
            fails("browser_clock_invalid", () => adapter.tick(attempt, now));
            assert.equal(noAuthority(adapter).attempt, null);
            assert.equal(noAuthority(adapter).refusal_code, "browser_clock_invalid");
        });
    }
    withAdapter((adapter) => {
        const attempt = awaitingHost(adapter);
        fails("browser_clock_invalid", () => adapter.tick(attempt, 11));
        assert.equal(noAuthority(adapter).attempt, null);
    });
});

test("bounded control input refuses wrong bindings, truncation, trailing data and legacy magic", () => {
    const wrongBinding = Buffer.from(REQUIRED_CAPABILITY);
    wrongBinding[19] = 7;
    for (const bytes of [
        REQUIRED_CAPABILITY.subarray(0, 27),
        Buffer.concat([REQUIRED_CAPABILITY, Buffer.from([0])]),
        wrongBinding,
        Buffer.from("FRBA"),
        Buffer.from("FRAT"),
    ]) {
        withAdapter((adapter) => {
            const attempt = awaitingHost(adapter);
            fails("browser_control_record_invalid", () => adapter.receiveControl(attempt, bytes, 13));
            assert.equal(noAuthority(adapter).attempt, null);
        });
    }
    withAdapter((adapter) => {
        const attempt = awaitingHost(adapter);
        fails("browser_message_too_large", () => adapter.receiveControl(attempt, new Uint8Array(4097), 13));
        noAuthority(adapter);
    });
});

test("bootstrap rejects profile skew, role mismatch, duplicate fields and invented session authority", () => {
    const cases = [
        [bootstrap("observer", { profile_id: 1 }), "browser_transport_profile_unsupported"],
        [bootstrap("observer", { profile_version: 99 }), "browser_transport_profile_unsupported"],
        [bootstrap("controller"), "browser_bootstrap_invalid"],
        [bootstrap("observer", { nonce_hex: "0g".repeat(32) }), "browser_bootstrap_invalid"],
        [bootstrap("observer", { session_id_hex: "ff".repeat(16) }), "browser_bootstrap_invalid"],
        ['{"desktop_available":false,"desktop_available":true}', "browser_bootstrap_invalid"],
        [" ".repeat(4097), "browser_message_too_large"],
    ];
    for (const [body, code] of cases) {
        withAdapter((adapter) => {
            const attempt = adapter.begin("observer", 10);
            fails(code, () => adapter.acceptBootstrap(attempt, body, 11));
            assert.equal(noAuthority(adapter).attempt, null);
        });
    }
    withAdapter((adapter) => {
        fails("browser_role_invalid", () => adapter.begin("administrator", 10));
        assert.equal(noAuthority(adapter).state_label, "disconnected");
    });
});

test("a non-refusal canonical record cannot fabricate a desktop session", () => {
    withAdapter((adapter) => {
        const attempt = awaitingHost(adapter);
        fails("browser_control_record_unsupported", () => adapter.receiveControl(attempt, OBSERVER_HELLO, 13));
        assert.equal(noAuthority(adapter).state_label, "refused");
    });
});
