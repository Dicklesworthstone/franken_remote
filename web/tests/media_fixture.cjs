"use strict";

// Actual generated Rust/WASM admission/custody with deliberately simulated
// WebCodecs handles. This is boundary execution, not native HEVC qualification.
const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const { pathToFileURL } = require("node:url");
const vm = require("node:vm");
const source = fs.readFileSync(path.join(__dirname, "..", "hevc_probe.js"), "utf8");
let bindings;

async function initialize() {
    const directory = process.env.FR_WEB_WASM_PACKAGE;
    assert.ok(directory, "Build the actual package with scripts/build-web.cjs; set FR_WEB_WASM_PACKAGE");
    bindings = await import(pathToFileURL(path.resolve(directory, "fr_web.js")).href);
    await bindings.default({ module_or_path: fs.readFileSync(path.join(directory, "fr_web_bg.wasm")) });
    assert.equal(bindings.browserMediaAbiVersion(), 1);
}

const NALS = [
    "40010c01ffff01600000030090000003000003003cba0240",
    "42010101600000030090000003000003003ca00a080f165ba4a4c2f016a020202080000003008000000f04",
    "4401c0718112",
    "2801ade06702f86753c11ead2f1f6a69",
].map((hex) => Buffer.from(hex, "hex"));
// Independent canonical hvcC fixture from these real x265 parameter sets.
const HVCC = Buffer.concat([
    Buffer.from("0101600000009000000000003cf000fcfdf8f800000f03", "hex"),
    ...NALS.slice(0, 3).flatMap((nal, index) => {
        const header = Buffer.from([0x80 | (32 + index), 0, 1, 0, 0]);
        header.writeUInt16BE(nal.length, 3);
        return [header, nal];
    }),
]);
const PICTURE = Buffer.concat(NALS.flatMap((nal) => {
    const length = Buffer.alloc(4);
    length.writeUInt32BE(nal.length);
    return [length, nal];
}));

function parameters(generation = 0) {
    return { generation, codec: "hev1.1.6.L60.90", hvcc: new Uint8Array(HVCC),
        codedWidth: 320, codedHeight: 240, cropWidth: 320, cropHeight: 240,
        fps: 30, primaries: 1, transfer: 1, matrix: 1, fullRange: false, decodedPictures: 4 };
}

function deferred() {
    let resolve, reject;
    const promise = new Promise((yes, no) => { resolve = yes; reject = no; });
    return { promise, resolve, reject };
}

function fixture(options = {}) {
    const probes = [], decoders = [], configured = [], released = [], presented = [], first = [], errors = [];
    const raf = [], pendingRaf = new Map(), canvasWrites = [], draws = [];
    let nextRaf = 0;
    let width = 300, height = 150;
    const canvas = {
        getContext: () => ({ drawImage(frame, ...args) {
            if (options.drawThrows) throw new Error("private draw failure");
            draws.push({ frame, args });
        } }),
        get width() { return width; },
        set width(value) { width = value; canvasWrites.push(["width", value]); },
        get height() { return height; },
        set height(value) { height = value; canvasWrites.push(["height", value]); },
    };
    const window = {
        performance: { now: () => 1000 },
        VideoDecoder: class {
            static isConfigSupported(config) {
                const pending = deferred(); probes.push({ ...pending, config });
                if (options.probeThrows) throw new Error(options.probeThrows);
                return pending.promise;
            }
            constructor(callbacks) {
                if (options.constructorThrows) throw new Error(options.constructorThrows);
                this.callbacks = callbacks; this.state = "unconfigured";
                this.decodeQueueSize = 0; this.closeCalls = 0; this.chunks = [];
                this.closeThrows = options.closeThrows;
                decoders.push(this);
                if (options.constructorError) callbacks.error(new Error("private constructor callback"));
            }
            configure(config) {
                this.config = config;
                if (options.configureThrows) throw new Error(options.configureThrows);
                if (options.configureError) this.callbacks.error(new Error("private configure callback"));
                this.state = "configured";
            }
            decode(chunk) {
                if (options.decodeThrows) throw new Error("private decode failure");
                this.chunks.push(chunk);
            }
            close() {
                this.closeCalls += 1;
                if (this.closeThrows) throw new Error("private close failure");
                this.state = "closed";
                if (options.onDecoderClose) options.onDecoderClose(this);
            }
        },
        EncodedVideoChunk: class {
            constructor(config) {
                if (options.chunkThrows) throw new Error("private chunk failure");
                Object.assign(this, config);
            }
        },
        requestAnimationFrame(fn) {
            if (options.animationThrows) throw new Error("private animation failure");
            const handle = ++nextRaf;
            const callback = () => { pendingRaf.delete(handle); fn(); };
            raf.push(callback); pendingRaf.set(handle, callback);
            return handle;
        },
        cancelAnimationFrame(handle) { pendingRaf.delete(handle); },
    };
    vm.runInNewContext(source, { window, document: { hidden: false }, ArrayBuffer, Uint8Array });
    const media = new bindings.BrowserMediaAdapter(options.maxFrames ?? 4, BigInt(options.maxBytes ?? 33554432));
    const player = new window.FrankenHevc.WebCodecsHevcPlayer(canvas, {
        onConfigured(info) { configured.push({ ...info }); if (options.onConfigured) options.onConfigured(info); },
        onFirstFrameDecoded(info) { first.push({ ...info }); if (options.onFirstFrameDecoded) options.onFirstFrameDecoded(info); },
        onFramePresented(info) { presented.push({ ...info }); if (options.onFramePresented) options.onFramePresented(info); },
        onFrameClosed(info) { released.push({ ...info }); if (options.onFrameClosed) options.onFrameClosed(info); },
        onDecoderReclaimed(error) { errors.push(error); if (options.onDecoderReclaimed) options.onDecoderReclaimed(error); },
    }, media, { nowMs: options.nowMs || (() => 1000) });
    function submit(frameId = 1, generation = player.activeGeneration, source = 55) {
        return player.submitAccessUnit({ generation, frameId, sourceTimestampUs: source, isKey: true, data: new Uint8Array(PICTURE) });
    }
    function output(decoder = player.decoder, index = 0, config = {}) {
        const frame = {
            timestamp: decoder.chunks[index].timestamp, closeCalls: 0, closeThrows: config.closeThrows,
            close() { this.closeCalls++; if (this.closeThrows) throw new Error("private frame close"); },
        };
        decoder.callbacks.output(frame);
        return frame;
    }
    return { player, media, probes, decoders, configured, released, presented, first, errors,
        raf, pendingRaf, canvas, canvasWrites, draws, submit, output, api: window.FrankenHevc };
}

function finishProbe(probe, outcome = "supported") {
    if (outcome === "rejected") probe.reject(new Error("private probe failure"));
    else probe.resolve({ supported: outcome === "supported" });
}
async function configure(f, generation = 0) {
    const pending = f.player.configure(parameters(generation));
    assert.ok(f.probes.at(-1), "validated configuration reached real WebCodecs boundary");
    finishProbe(f.probes.at(-1));
    assert.equal((await pending).supported, true);
    return f.player.decoder;
}

module.exports = { initialize, fixture, parameters, finishProbe, configure, HVCC, PICTURE };
