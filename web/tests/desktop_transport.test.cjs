"use strict";
// I/O ownership tests, not browser/HEVC/host qualification. Rust canonical
// negotiation and original resource receipts are exercised by separate tests.
const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const vm = require("node:vm");
const test = require("node:test");
const clientSource = fs.readFileSync(path.join(__dirname, "../wss_client.js"), "utf8");
const playerSource = fs.readFileSync(path.join(__dirname, "../hevc_probe.js"), "utf8");

class Signal {
    constructor() { this.aborted = false; this.listeners = new Set(); }
    addEventListener(_, callback) { this.listeners.add(callback); }
    removeEventListener(_, callback) { this.listeners.delete(callback); }
}
class Abort {
    constructor() { this.signal = new Signal(); }
    abort() {
        if (this.signal.aborted) return;
        this.signal.aborted = true;
        for (const callback of Array.from(this.signal.listeners)) callback();
    }
}
async function flush() { for (let i = 0; i < 40; ++i) await Promise.resolve(); }
function harness() {
    const sockets = [];
    const core = {
        freed: 0, disconnected: 0, retained: 0, needVideo: false, receivedVideo: 0,
        begin() { return 1; }, lifecycle() {}, tick() {},
        bootstrapRequest() { return '{"role":"observer"}'; },
        acceptBootstrap() {}, clientHello() { return Uint8Array.of(1); },
        receiveControl() { return ""; },
        snapshot() { return '{"state_label":"awaiting_host"}'; },
        takeControl() { return undefined; }, controlQueued() {},
        needsVideo() { return this.needVideo; },
        configurationReady() { return false; },
        videoAttach() { this.needVideo = false; return Uint8Array.of(2); },
        receiveVideo() { this.receivedVideo += 1; return "configuration"; },
        takeVideoFeedback() { return undefined; }, takeVideoChunk() { return undefined; },
        configurationParameters() { return '{"hvcc":[]}'; },
        desktopSnapshot() {
            return JSON.stringify({ retainedDecoders: this.retained, inflightFrames: 0, quarantined: false });
        },
        disconnect() { this.disconnected += 1; },
        free() { this.freed += 1; },
    };
    class Socket {
        constructor(url) { this.url = url; this.readyState = 0; this.bufferedAmount = 0; this.sent = []; sockets.push(this); }
        open() { this.readyState = 1; this.onopen(); }
        send(bytes) { this.sent.push(bytes); }
        message(bytes) { this.onmessage({ data: bytes }); }
        close() { this.readyState = 3; }
    }
    const global = {
        location: { protocol: "https:", host: "host.example" },
        performance: { now: () => 10 },
        AbortController: Abort, WebSocket: Socket,
        setTimeout: () => 1, clearTimeout() {}, setInterval: () => 2, clearInterval() {},
        fetch: async () => {
            let delivered = false;
            return {
                ok: true, headers: { get: () => null },
                body: { getReader: () => ({
                    read: async () => delivered ? { done: true } : (delivered = true, { value: Uint8Array.of(123, 125), done: false }),
                    cancel: async () => {}, releaseLock() {},
                }) },
            };
        },
    };
    class Decoder { decode(bytes) { return String.fromCharCode(...bytes); } }
    vm.runInNewContext(clientSource, {
        window: global, TextDecoder: Decoder, Uint8Array, ArrayBuffer, Set, Promise, Error,
    });
    let allocated = 0, configured = 0, finish = null, pending = false;
    const player = {
        configured: false,
        setVisible() {},
        configure() {
            configured += 1; pending = true; core.retained = 1;
            return new Promise((resolve) => { finish = () => {
                pending = false; core.retained = 0; resolve({ supported: false, cancelled: true });
            }; });
        },
        reset() { return !pending; },
    };
    const client = new global.FrankenWss.FrankenWssClient({}, {
        canvas: {}, nowMs: () => 10,
        createAdapter: async () => { allocated += 1; return {
            core, maxBootstrapBytes: 4096, maxControlRecordBytes: 4096, maxVideoRecordBytes: 65536,
        }; },
        createPlayer: () => player,
    });
    async function connected() {
        const connecting = client.connect("observer");
        await flush();
        assert.equal(sockets.length, 1);
        sockets[0].open();
        await connecting;
    }
    function video() {
        core.needVideo = true;
        client._pump(client.connectionAttempt);
        assert.equal(sockets.length, 2);
        sockets[1].open();
        return sockets[1];
    }
    return { core, client, connected, video, sockets,
        allocated: () => allocated, configured: () => configured, finish: () => finish() };
}

test("disconnect retains the original WASM owner through an unabortable configuration", async () => {
    const h = harness();
    await h.connected();
    h.video().message(new ArrayBuffer(1));
    await flush();
    assert.equal(h.configured(), 1);
    h.client.disconnect();
    assert.equal(h.core.disconnected, 1);
    assert.equal(h.core.freed, 0);
    await assert.rejects(() => h.client.connect(), { code: "browser_media_cleanup_incomplete" });
    assert.equal(h.allocated(), 1);
    h.finish();
    await flush();
    assert.equal(h.core.freed, 1);
    assert.equal(h.client.retiredAttempt, null);
});

test("retiring before the configuration microtask starts does not create a decoder", async () => {
    const h = harness();
    await h.connected();
    h.video().message(new ArrayBuffer(1));
    h.client.disconnect();
    assert.equal(h.core.freed, 0);
    await flush();
    assert.equal(h.configured(), 0);
    assert.equal(h.core.freed, 1);
});

test("oversized media is refused before copying it to the Rust ABI", async () => {
    const h = harness();
    await h.connected();
    h.video().message(new ArrayBuffer(65537));
    assert.equal(h.core.receivedVideo, 0);
    assert.equal(h.core.freed, 1);
    assert.equal(h.client.closed, true);
});

test("prepared chunks retain original decoder identity and use no JavaScript HEVC path", () => {
    let decoded = 0, freed = 0;
    const global = { performance: { now: () => 10 }, EncodedVideoChunk: class { constructor(config) { this.config = config; } } };
    vm.runInNewContext(playerSource, {
        window: global, document: { hidden: false }, Uint8Array, ArrayBuffer, BigInt, Set, Promise, Error,
    });
    const player = new global.FrankenHevc.WebCodecsHevcPlayer({ getContext: () => ({}) }, {}, {
        prepareChunk() { throw new Error("session chunks must already belong to Rust"); },
    });
    player.current = {
        id: 1n, generation: "2", retired: false, nativeBusy: 0,
        decoder: { state: "configured", decodeQueueSize: 0, decode() { decoded += 1; } },
    };
    player.configured = true;
    const chunk = (decoder, generation) => ({
        decoder: () => decoder, generation: () => generation,
        isKey: () => true, timestamp: () => 1n, takeBytes: () => Uint8Array.of(1),
        free: () => { freed += 1; },
    });
    assert.equal(player.submitPreparedChunk(chunk(9n, 2n)).admitted, false);
    assert.equal(player.submitPreparedChunk(chunk(1n, 3n)).admitted, false);
    assert.equal(decoded, 0);
    assert.equal(freed, 2);
    assert.equal(player.submitPreparedChunk(chunk(1n, 2n)).admitted, true);
    assert.equal(decoded, 1);
    assert.equal(freed, 3);
});
