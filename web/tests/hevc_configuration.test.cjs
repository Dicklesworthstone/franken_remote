"use strict";

// JavaScript configuration ownership with simulated WebCodecs, not a browser,
// HEVC decoder, or escaped-VideoFrame/receive-credit qualification.
const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const test = require("node:test");
const vm = require("node:vm");

const source = fs.readFileSync(path.join(__dirname, "..", "hevc_probe.js"), "utf8");

function deferred() {
    let resolve;
    let reject;
    const promise = new Promise((yes, no) => { resolve = yes; reject = no; });
    return { promise, resolve, reject };
}

function parameters(generation = 7, width = 640, height = 480) {
    return {
        generation,
        codec: "hvc1.1.6.L93.B0",
        codedWidth: width,
        codedHeight: height,
        hvcc: Uint8Array.from({ length: 23 }, (_, index) => index + 1),
    };
}

function fixture(options = {}) {
    const probes = [];
    const decoders = [];
    const configured = [];
    const canvasWrites = [];
    let width = 300;
    let height = 150;
    const canvas = {
        getContext: () => ({ drawImage() {} }),
        get width() { return width; },
        set width(value) { width = value; canvasWrites.push(["width", value]); },
        get height() { return height; },
        set height(value) { height = value; canvasWrites.push(["height", value]); },
    };
    const window = {
        VideoDecoder: class {
            static isConfigSupported(config) {
                const pending = deferred();
                probes.push({ ...pending, config });
                if (options.probeThrows) throw new Error(options.probeThrows);
                return pending.promise;
            }
            constructor(callbacks) {
                if (options.constructorThrows) throw new Error(options.constructorThrows);
                this.callbacks = callbacks;
                this.state = "unconfigured";
                this.decodeQueueSize = 0;
                this.closeCalls = 0;
                decoders.push(this);
            }
            configure(config) {
                this.config = config;
                if (options.configureThrows) throw new Error(options.configureThrows);
                this.state = "configured";
            }
            close() {
                this.closeCalls += 1;
                this.state = "closed";
                if (options.closeThrows) throw new Error(options.closeThrows);
            }
        },
        EncodedVideoChunk: class {},
        requestAnimationFrame() {},
    };
    vm.runInNewContext(source, { window, document: { hidden: false }, ArrayBuffer, Uint8Array });
    const player = new window.FrankenHevc.WebCodecsHevcPlayer(canvas, {
        onConfigured(info) { configured.push({ ...info }); },
        onFrameClosed(info) {
            if (options.onFrameClosed) options.onFrameClosed(info);
        },
    });
    return { player, probes, decoders, configured, canvasWrites, canvas, api: window.FrankenHevc };
}

function finishProbe(probe, outcome) {
    if (outcome === "rejected") probe.reject(new Error("platform-private-details".repeat(200)));
    else probe.resolve({ supported: outcome === "supported" });
}

function assertCancelled(result) {
    assert.equal(result.supported, false);
    assert.equal(result.cancelled, true);
    assert.equal(result.code, "decoder_configuration_cancelled");
}

for (const outcome of ["supported", "unsupported", "rejected"]) {
    test("reset fences a pending " + outcome + " probe before canvas, decoder or callback mutation", async () => {
        const f = fixture();
        const pending = f.player.configure(parameters());
        f.player.reset();
        finishProbe(f.probes[0], outcome);
        assertCancelled(await pending);
        assert.equal(f.player.decoder, null);
        assert.equal(f.player.configured, false);
        assert.equal(f.decoders.length, 0);
        assert.equal(f.configured.length, 0);
        assert.equal(f.canvasWrites.length, 0);
    });

    test("late " + outcome + " probe cannot replace a newer decoder of the same wire generation", async () => {
        const f = fixture();
        const first = f.player.configure(parameters(7, 640, 480));
        const second = f.player.configure(parameters(7, 1280, 720));
        finishProbe(f.probes[1], "supported");
        assert.equal((await second).supported, true);
        const current = f.player.decoder;
        finishProbe(f.probes[0], outcome);
        assertCancelled(await first);
        assert.equal(f.player.decoder, current);
        assert.equal(f.player.configured, true);
        assert.equal(current.closeCalls, 0);
        assert.equal(f.decoders.length, 1);
        assert.equal(f.configured.length, 1);
        assert.equal(f.configured[0].generation, 7);
        assert.equal(f.canvas.width, 1280);
        assert.equal(f.canvas.height, 720);
        assert.equal(f.canvasWrites.length, 2);
    });
}

test("an earlier probe completing first stays cancelled while its replacement is pending", async () => {
    const f = fixture();
    const first = f.player.configure(parameters());
    const second = f.player.configure(parameters(8));
    finishProbe(f.probes[0], "supported");
    assertCancelled(await first);
    assert.equal(f.decoders.length, 0);
    assert.equal(f.canvasWrites.length, 0);
    finishProbe(f.probes[1], "supported");
    assert.equal((await second).supported, true);
    assert.equal(f.player.activeGeneration, 8);
    assert.deepEqual(f.configured.map((info) => info.generation), [8]);
});

test("a newer refusal cannot be undone by an older successful probe", async () => {
    const f = fixture();
    const first = f.player.configure(parameters());
    const second = f.player.configure(parameters(8));
    finishProbe(f.probes[1], "unsupported");
    assert.equal((await second).code, "no_supported_hevc_decoder");
    finishProbe(f.probes[0], "supported");
    assertCancelled(await first);
    assert.equal(f.player.decoder, null);
    assert.equal(f.decoders.length, 0);
    assert.equal(f.configured.length, 0);
    assert.equal(f.canvasWrites.length, 0);
});

test("configuration captures negotiated metadata and an offset hvcC view before awaiting", async () => {
    const f = fixture();
    const request = parameters();
    const storage = new Uint8Array(40);
    storage.set(request.hvcc, 5);
    request.hvcc = storage.subarray(5, 28);
    const original = Array.from(request.hvcc);
    const pending = f.player.configure(request);
    request.generation = 99;
    request.codec = "mutated-codec";
    request.codedWidth = 1;
    request.codedHeight = 2;
    storage.fill(255);
    // A returned probe dictionary is not permission to substitute a different
    // codec or description for the negotiated snapshot.
    f.probes[0].resolve({ supported: true, config: { codec: "different-codec" } });
    assert.equal((await pending).supported, true);
    const decoder = f.player.decoder;
    assert.equal(decoder.config.codec, "hvc1.1.6.L93.B0");
    assert.equal(decoder.config.codedWidth, 640);
    assert.equal(decoder.config.codedHeight, 480);
    assert.deepEqual(Array.from(decoder.config.description), original);
    assert.equal(f.player.activeGeneration, 7);
    assert.deepEqual(f.configured, [{ generation: 7, codec: "hvc1.1.6.L93.B0" }]);
    assert.equal(f.canvas.width, 640);
    assert.equal(f.canvas.height, 480);
});

test("reset invalidates a pending configuration before frame-close callbacks run", async () => {
    let f;
    let callbackOwner = "not-called";
    f = fixture({ onFrameClosed() { callbackOwner = f.player.configurationOwner; } });
    const pending = f.player.configure(parameters());
    f.player.pendingFrame = {
        frame: { close() {} },
        meta: { chargedBytes: 1 },
    };
    f.player.reset();
    assert.equal(callbackOwner, null);
    finishProbe(f.probes[0], "supported");
    assertCancelled(await pending);
    assert.equal(f.configured.length, 0);
});

test("cleanup reentrancy cannot install a retired configuration over its successor", async () => {
    let successor;
    let f;
    f = fixture({ onFrameClosed() { successor = f.player.configure(parameters(3, 1920, 1080)); } });
    const initial = f.player.configure(parameters(1));
    finishProbe(f.probes[0], "supported");
    await initial;
    const oldDecoder = f.player.decoder;
    const pending = f.player.configure(parameters(2, 1280, 720));
    f.player.pendingFrame = {
        frame: { close() {} },
        meta: { chargedBytes: 1 },
    };
    finishProbe(f.probes[1], "supported");
    assertCancelled(await pending);
    assert.equal(oldDecoder.closeCalls, 1);
    assert.equal(f.decoders.length, 1);
    assert.equal(f.player.decoder, null);
    finishProbe(f.probes[2], "supported");
    assert.equal((await successor).supported, true);
    assert.equal(f.player.activeGeneration, 3);
    assert.deepEqual(f.configured.map((info) => info.generation), [1, 3]);
    assert.equal(f.canvas.width, 1920);
    assert.equal(f.canvas.height, 1080);
});

test("a VideoDecoder constructor failure resolves to a bounded typed refusal", async () => {
    const f = fixture({ constructorThrows: "private-platform-detail".repeat(200) });
    const pending = f.player.configure(parameters());
    finishProbe(f.probes[0], "supported");
    const result = await pending;
    assert.equal(result.supported, false);
    assert.equal(result.code, "hevc_decoder_configuration_failed");
    assert.ok(result.reason.length < 160);
    assert.equal(result.reason.includes("private-platform-detail"), false);
    assert.equal(f.player.decoder, null);
    assert.equal(f.player.configured, false);
    assert.equal(f.configured.length, 0);
    assert.equal(f.canvasWrites.length, 0);
});

test("a refused probe cannot publish failure after cleanup starts a replacement", async () => {
    let successor;
    let f;
    f = fixture({ onFrameClosed() { successor = f.player.configure(parameters(8, 1280, 720)); } });
    const pending = f.player.configure(parameters());
    f.player.pendingFrame = {
        frame: { close() {} },
        meta: { chargedBytes: 1 },
    };
    finishProbe(f.probes[0], "unsupported");
    assertCancelled(await pending);
    finishProbe(f.probes[1], "supported");
    assert.equal((await successor).supported, true);
    assert.equal(f.player.activeGeneration, 8);
    assert.deepEqual(f.configured.map((info) => info.generation), [8]);
    assert.equal(f.canvas.width, 1280);
    assert.equal(f.canvas.height, 720);
});

test("a configure failure attempts partial decoder cleanup and preserves typed refusal if close throws", async () => {
    const f = fixture({ configureThrows: "private-config-error", closeThrows: "private-close-error" });
    const pending = f.player.configure(parameters());
    finishProbe(f.probes[0], "supported");
    const result = await pending;
    assert.equal(result.supported, false);
    assert.equal(result.code, "hevc_decoder_configuration_failed");
    assert.equal(result.reason.includes("private-"), false);
    assert.equal(f.decoders.length, 1);
    assert.equal(f.decoders[0].closeCalls, 1);
    assert.equal(f.player.decoder, null);
    assert.equal(f.player.configured, false);
    assert.equal(f.configured.length, 0);
    assert.equal(f.canvasWrites.length, 0);
});

test("synchronous and asynchronous probe failures have static bounded diagnostics", async () => {
    for (const synchronous of [false, true]) {
        const f = fixture(synchronous ? { probeThrows: "private-platform-detail".repeat(200) } : {});
        const pending = f.player.configure(parameters());
        if (!synchronous) finishProbe(f.probes[0], "rejected");
        const result = await pending;
        assert.equal(result.supported, false);
        assert.equal(result.code, "hevc_probe_failed");
        assert.ok(result.reason.length < 160);
        assert.equal(result.reason.includes("private-platform-detail"), false);
        assert.equal(f.decoders.length, 0);
        assert.equal(f.configured.length, 0);
    }
});
