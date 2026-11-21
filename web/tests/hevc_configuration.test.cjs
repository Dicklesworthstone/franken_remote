"use strict";

// Package-independent probe/refusal checks. Full configuration/custody cases
// execute the actual generated Rust/WASM in wasm_media_configuration.cjs and
// wasm_media.cjs through scripts/build-web.cjs.
const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const { test } = require("node:test");
const vm = require("node:vm");
const source = fs.readFileSync(path.join(__dirname, "..", "hevc_probe.js"), "utf8");

function api(probe) {
    const window = { performance: { now: () => 0 }, EncodedVideoChunk: class {},
        VideoDecoder: class { static isConfigSupported(config) { return probe(config); } } };
    vm.runInNewContext(source, { window, document: { hidden: false }, ArrayBuffer, Uint8Array });
    return window.FrankenHevc;
}

test("the player refuses absent Rust ownership before browser capability probing", async () => {
    let calls = 0;
    const hevc = api(() => { calls++; return { supported: true }; });
    const player = new hevc.WebCodecsHevcPlayer({ getContext: () => ({}) });
    const result = await player.configure({});
    assert.equal(result.code, "browser_media_owner_unavailable");
    assert.equal(result.supported, false);
    assert.equal(calls, 0);
    assert.equal(player.configured, false);
});

test("the narrow capability probe copies only the supplied hvcC view before awaiting", async () => {
    let resolve, config;
    const hevc = api((request) => { config = request; return new Promise((yes) => { resolve = yes; }); });
    const bytes = new Uint8Array(40).fill(17);
    const pending = hevc.probeHevcWebCodecs("hvc1.1.6.L60.90", bytes.subarray(5, 28), 320, 240);
    bytes.fill(255);
    resolve({ supported: true, config: { codec: "substituted" } });
    const result = await pending;
    assert.equal(result.supported, true);
    assert.equal(result.config, config);
    assert.equal(config.codec, "hvc1.1.6.L60.90");
    assert.deepEqual(Array.from(config.description), Array(23).fill(17));
});

for (const asynchronous of [false, true]) {
    test((asynchronous ? "asynchronous" : "synchronous") + " capability failure omits platform details", async () => {
        const hevc = api(() => {
            if (asynchronous) return Promise.reject(new Error("private platform detail"));
            throw new Error("private platform detail");
        });
        const result = await hevc.probeHevcWebCodecs("hvc1.1.6.L60.90", new Uint8Array(23), 320, 240);
        assert.equal(result.supported, false);
        assert.equal(result.code, "hevc_probe_failed");
        assert.equal(result.reason.includes("private"), false);
        assert.ok(result.reason.length < 160);
    });
}

test("unsupported capability is a refusal, never a second codec fallback", async () => {
    const hevc = api(() => ({ supported: false }));
    const result = await hevc.probeHevcWebCodecs("hvc1.1.6.L60.90", new Uint8Array(23), 320, 240);
    assert.equal(result.supported, false);
    assert.equal(result.code, "no_supported_hevc_decoder");
});
