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
    46524430 0000 0001 00000000 0000009b 00000000 00000000
    00000001 0000 03 0000 00
    00010000 00100000 000aa555 00002000 0000000001000000
    04 0000000000200000 00000003
    00000011 62726f777365722d766964656f2d777373 0001 01
    00000011 636c6f636b2d636f7272656c6174696f6e 0001 01
    00000014 686576632d6465636f6465722d73746172747570 0001 01
    01
` + "a5".repeat(32)).replace(/\s/g, ""), "hex");

// PROTOCOL_REFUSAL.md's 28-byte, zero-bound record, required capability reason 4.
const REQUIRED_CAPABILITY = Buffer.from(
    "46524430000000040000000000000004000000000000000000040000", "hex");

// Independent admitted-host fixtures. These exercise the actual Rust wire,
// receiver, HEVC guard, clock and native-custody owners; the explicit callbacks
// below simulate native decoder/compositor completion, not browser support.
const { HVCC, PICTURE } = require("../../../web/tests/media_fixture.cjs");
const scalar = (value, size) => {
    const bytes = Buffer.alloc(size);
    if (size === 16) bytes.writeBigUInt64BE(BigInt(value), 8);
    else if (size === 8) bytes.writeBigUInt64BE(BigInt(value));
    else bytes.writeUIntBE(value, 0, size);
    return bytes;
};
const u32 = (value) => scalar(value, 4);
const u64 = (value) => scalar(value, 8);
const identities = Buffer.concat([11, 12, 13].map((value) => scalar(value, 16)));
const mediaTuple = Buffer.concat([identities, scalar(21, 16), Buffer.alloc(32)]);
const mediaDescriptor = Buffer.concat([u32(19), mediaTuple, Buffer.from([3, 1])]);
const grant = Buffer.concat([
    mediaDescriptor, Buffer.alloc(32, 42), u64(500000), u64(2097152), u32(4), u64(8),
]);
function record(kind, binding, payload) {
    const header = Buffer.alloc(24);
    header.write("FRD0");
    header.writeUInt16BE(kind, 6);
    header.writeUInt32BE(payload.length, 12);
    header.writeUInt32BE(binding, 16);
    return Buffer.concat([header, payload]);
}
function kind(bytes) { return Buffer.from(bytes).readUInt16BE(6); }
function openedDesktop(adapter, synchronize = true) {
    const attempt = adapter.begin("observer", 1);
    adapter.acceptBootstrap(attempt, bootstrap(), 2);
    adapter.clientHello(attempt, 3);
    adapter.receiveControl(attempt, record(2, 0, Buffer.concat([OBSERVER_HELLO.subarray(24, -33), Buffer.from([0])])), 4);
    const selected = adapter.takeControl(attempt, 4.1);
    assert.equal(kind(selected), 3);
    adapter.controlQueued(attempt, 4.2);
    adapter.receiveControl(attempt, record(0x11, 0, Buffer.concat([
        identities, u32(7), Buffer.from([0, 0, 0, 0, 0, 0, 1, 0]),
        Buffer.from(selected).subarray(24), u64(500000),
    ])), 5);
    assert.equal(kind(adapter.takeControl(attempt, 5.1)), 0x1c);
    adapter.controlQueued(attempt, 5.2);
    const probe = Buffer.from(adapter.takeControl(attempt, 5.3));
    assert.equal(kind(probe), 0x84);
    adapter.controlQueued(attempt, 5.4);
    const clockReply = record(0x85, 7, Buffer.concat([probe.subarray(24), u64(5400)]));
    if (synchronize) adapter.receiveControl(attempt, clockReply, 5.5);
    adapter.receiveControl(attempt, record(0x1b, 7, mediaDescriptor), 6);
    assert.equal(kind(adapter.takeControl(attempt, 6.1)), 0x1c);
    adapter.controlQueued(attempt, 6.2);
    adapter.receiveControl(attempt, record(0x18, 7, grant), 7);
    assert.equal(adapter.needsVideo(), true);
    assert.equal(kind(adapter.videoAttach(attempt, 8)), 0x19);
    adapter.receiveVideo(attempt, record(0x1a, 19, grant), 9);
    const codec = Buffer.from("hev1.1.6.L60.90");
    adapter.receiveVideo(attempt, record(0x30, 19, Buffer.concat([
        mediaTuple, ...[320, 240, 320, 240].map(u32), scalar(30, 2),
        Buffer.from([1, 1, 1, 0, 4]), u32(codec.length), codec, u32(HVCC.length), HVCC,
    ])), 10);
    return { attempt, clockReply };
}
function configuredDesktop(adapter) {
    const { attempt } = openedDesktop(adapter);
    assert.equal(adapter.configurationReady(), true);
    const prepared = JSON.parse(adapter.prepareDesktopConfiguration());
    assert.equal(prepared.codec, "hvc1.1.6.L60.90");
    const decoder = BigInt(prepared.decoder);
    adapter.desktopConfigured(decoder, 11);
    assert.equal(kind(adapter.takeVideoFeedback(attempt, 11)), 0x31);
    return { attempt, decoder };
}
function descriptor(frame, capture = 20000, reference = null) {
    return Buffer.concat([
        u64(frame), u32(PICTURE.length), u32(65536 - 73), u64(capture),
        Buffer.from([reference === null ? 0 : 1]), ...(reference === null ? [] : [u64(reference)]),
    ]);
}
function progress(frame, capture = 20000, observed = capture, source = 1, reference = null) {
    return record(0x37, 19, Buffer.concat([
        descriptor(frame, capture, reference), u64(observed), Buffer.from([source, 1]),
    ]));
}
function receivePicture(adapter, attempt, frame = 1, now = 20, recovery = true) {
    const capture = now * 1000;
    adapter.receiveVideo(attempt, progress(frame, capture), now);
    const payload = recovery ? [
        u64(frame), u32(PICTURE.length), u32(0), u64(capture), u32(PICTURE.length), PICTURE,
    ] : [
        u64(frame), u32(PICTURE.length), u32(0), u32(0), u32(1), u32(65536 - 73),
        u64(capture), Buffer.from([0]), u32(PICTURE.length), PICTURE,
    ];
    adapter.receiveVideo(attempt, record(recovery ? 0x32 : 0x34, 19, Buffer.concat(payload)), now + 0.1);
}
function takePicture(adapter, attempt, now) {
    const chunk = adapter.takeVideoChunk(attempt, now);
    assert.ok(chunk);
    const callback = BigInt(chunk.timestamp());
    assert.deepEqual(Buffer.from(chunk.takeBytes()), PICTURE.subarray(PICTURE.length - 20));
    chunk.free();
    return callback;
}
function pressure(adapter, attempt, now) {
    const bytes = Buffer.from(adapter.takeControl(attempt, now));
    assert.equal(kind(bytes), 0x81);
    adapter.controlQueued(attempt, now);
    assert.equal(bytes.readUInt32BE(16), 7);
    assert.equal(bytes.readUInt32BE(72), 19);
    assert.equal(bytes.readBigUInt64BE(78), 8n);
    return {
        bytes: bytes.readBigUInt64BE(86), frames: bytes.readBigUInt64BE(94),
        queuedBytes: bytes.readBigUInt64BE(103), queuedFrames: bytes.readUInt32BE(111),
    };
}
const FRAME_CHARGE = BigInt(3 * PICTURE.length + 4096);

test("the desktop waits for actual clock evidence before native configuration", () => withAdapter((adapter) => {
    const { attempt, clockReply } = openedDesktop(adapter, false);
    assert.equal(adapter.configurationReady(), false);
    fails("browser_connection_state_invalid", () => adapter.prepareDesktopConfiguration());
    assert.equal(JSON.parse(adapter.desktopSnapshot()).retainedDecoders, 0);
    adapter.receiveControl(attempt, clockReply, 10.1);
    assert.equal(adapter.configurationReady(), true);
    noAuthority(adapter);
    adapter.disconnect(attempt);
}));

test("late decoded or presented pixels stay unqualified until a newer exact-source observation", () => {
    for (const decodedAt of [21, 90]) withAdapter((adapter) => {
        const { attempt, decoder } = configuredDesktop(adapter);
        receivePicture(adapter, attempt);
        const callback = takePicture(adapter, attempt, 20.2);
        const output = JSON.parse(adapter.desktopOutput(decoder, callback, decodedAt));
        assert.equal(output.canPresent, true, "the newest image can still be shown late");
        assert.equal(output.firstDecoded, true);
        assert.equal(JSON.parse(adapter.snapshot()).view_fresh, false);
        assert.equal(adapter.takeControl(attempt, decodedAt), undefined, "decode is not disposal credit");
        adapter.desktopPresented(decoder, callback, 100, true);
        assert.equal(JSON.parse(adapter.snapshot()).view_fresh, false, "the original 50ms display deadline survives decode and rAF delay");
        adapter.receiveVideo(attempt, progress(1, 20000, 20000, 1), 105);
        assert.equal(JSON.parse(adapter.snapshot()).view_fresh, false, "replaying old capture evidence cannot qualify late pixels");
        adapter.receiveVideo(attempt, progress(1, 20000, 110000, 2), 110.1);
        const shown = JSON.parse(adapter.snapshot());
        assert.equal(shown.view_fresh, true);
        assert.equal(shown.can_request_control, false);
        assert.equal(shown.is_controlling, false);
        const released = JSON.parse(adapter.desktopFrameClosed(decoder, callback, 111));
        assert.equal(BigInt(released.releasedBytes), FRAME_CHARGE);
        assert.deepEqual(pressure(adapter, attempt, 111), {
            bytes: FRAME_CHARGE, frames: 1n, queuedBytes: 0n, queuedFrames: 0,
        });
        adapter.disconnect(attempt);
        adapter.desktopDecoderClosed(decoder);
        assert.equal(JSON.parse(adapter.desktopSnapshot()).retainedDecoders, 0);
    });
});

test("retired escaped output retains its original reservation after decoder closure", () => withAdapter((adapter) => {
    const { attempt, decoder } = configuredDesktop(adapter);
    receivePicture(adapter, attempt);
    const callback = takePicture(adapter, attempt, 20.2);
    adapter.desktopOutput(decoder, callback, 21);
    adapter.disconnect(attempt);
    assert.deepEqual(JSON.parse(adapter.desktopDecoderClosed(decoder)), []);
    assert.equal(JSON.parse(adapter.desktopSnapshot()).inflightFrames, 1);
    fails("browser_connection_state_invalid", () => adapter.begin("observer", 22));
    fails("browser_media_record_invalid", () => adapter.desktopPresented(decoder, callback, 23, true));
    const release = JSON.parse(adapter.desktopFrameClosed(decoder, callback, 24));
    assert.equal(BigInt(release.releasedBytes), FRAME_CHARGE);
    assert.equal(JSON.parse(adapter.desktopSnapshot()).inflightFrames, 0);
    assert.equal(JSON.parse(adapter.desktopSnapshot()).retainedDecoders, 0);
    const next = adapter.begin("observer", 25);
    assert.ok(next > attempt);
    noAuthority(adapter);
}));

test("superseded receive allocations release full credit without a decoder callback", () => withAdapter((adapter) => {
    const { attempt, decoder } = configuredDesktop(adapter);
    receivePicture(adapter, attempt);
    const first = takePicture(adapter, attempt, 20.2);
    adapter.desktopOutput(decoder, first, 21);
    adapter.desktopPresented(decoder, first, 22, true);
    adapter.desktopFrameClosed(decoder, first, 23);
    assert.equal(pressure(adapter, attempt, 23).frames, 1n);
    adapter.receiveVideo(attempt, progress(2, 30000, 30000, 1, 1), 30);
    assert.equal(adapter.takeControl(attempt, 30), undefined, "an announced allocation remains retained");
    receivePicture(adapter, attempt, 3, 31, false);
    assert.deepEqual(pressure(adapter, attempt, 31.2), {
        bytes: 2n * FRAME_CHARGE, frames: 2n, queuedBytes: FRAME_CHARGE, queuedFrames: 1,
    });
    const third = takePicture(adapter, attempt, 31.3);
    assert.ok(third > first);
    adapter.desktopOutput(decoder, third, 32);
    adapter.desktopPresented(decoder, third, 33, true);
    adapter.desktopFrameClosed(decoder, third, 34);
    assert.deepEqual(pressure(adapter, attempt, 34), {
        bytes: 3n * FRAME_CHARGE, frames: 3n, queuedBytes: 0n, queuedFrames: 0,
    });
    adapter.disconnect(attempt);
    adapter.desktopDecoderClosed(decoder);
}));

test("the actual module exposes versioned Rust bindings and linear memory", () => {
    assert.ok(rawExports.memory instanceof WebAssembly.Memory);
    assert.equal(typeof rawExports.browseradapter_new, "function");
    assert.equal(bindings.browserAbiVersion(), 2);
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
            assert.equal(expected.length, 179);
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
