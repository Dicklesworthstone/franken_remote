"use strict";

// The production probe, packaged x265 samples and actual generated Rust/WASM.
// Native WebCodecs and canvas callbacks are controlled explicitly here. Browser
// and physical output qualification are separate execution lanes.
const assert = require("node:assert/strict");
const fs = require("node:fs");
const { createHash } = require("node:crypto");
const path = require("node:path");
const vm = require("node:vm");
const { before, test } = require("node:test");
const { initialize, mediaAdapter, fixture, finishProbe } = require("./media_fixture.cjs");
before(initialize);
const nextTurn = () => new Promise((resolve) => setImmediate(resolve));
const script = (name) => fs.readFileSync(path.join(__dirname, "..", name), "utf8");

function probeFixture(options = {}) {
    const f = fixture(options);
    // Reuse the explicit browser boundary; only the probe's own media adapter
    // participates in this test, so retire the unused fixture adapter now.
    f.media.free();
    const timers = new Map(), states = [], media = [];
    let now = 1000, serial = 0, drawn = null;
    f.canvas.isConnected = true;
    f.canvas.getBoundingClientRect = () => ({ width: 320, height: 240 });
    f.canvas.getContext = () => ({
        drawImage(frame) { drawn = frame; },
        getImageData() {
            if (options.readThrows) throw new Error("private native error");
            const bytes = new Uint8Array(320 * 240 * 4);
            const value = options.unchanged ? 31 : drawn?.pattern ?? 31;
            for (let i = 0; i < bytes.length; i += 4) {
                bytes[i] = options.blank ? 0 : (i / 4 + value) & 255;
                bytes[i + 1] = options.blank ? 0 : value;
                bytes[i + 2] = options.blank ? 0 : 2 * value & 255;
                bytes[i + 3] = 255;
            }
            return { data: bytes };
        },
    });
    f.window.performance = { now: () => now };
    f.window.AbortController = AbortController;
    f.window.atob = atob;
    f.window.setTimeout = (callback, delay) => {
        const id = ++serial;
        timers.set(id, { callback, at: now + delay });
        return id;
    };
    f.window.clearTimeout = (id) => timers.delete(id);
    f.window.FrankenRust = {
        async createMediaAdapter(signal, limits) {
            if (options.loading) await options.loading;
            if (signal.aborted) throw Object.assign(new Error(), { code: "browser_connection_cancelled" });
            const core = mediaAdapter(limits.frames, limits.bytes);
            let freed = false;
            const free = core.free.bind(core);
            core.free = () => { assert.equal(freed, false); freed = true; free(); };
            media.push({ core, freed: () => freed });
            return core;
        },
    };
    const context = { window: f.window, document: f.document, Uint8Array, ArrayBuffer };
    vm.runInNewContext(script("hevc_probe_sample.js"), context);
    vm.runInNewContext(script("hevc_qualification.js"), context);
    const probe = new f.window.FrankenHevcProbe.Probe(f.canvas, (state) => states.push(state));
    return {
        ...f, probe, states, media, timers,
        async advance(delta) {
            now += delta;
            for (const [id, timer] of Array.from(timers)) {
                if (timer.at <= now && timers.delete(id)) timer.callback();
            }
            await nextTurn();
        },
        setClock(value) { now = value; },
        async configured(outcome = "supported") {
            const pending = f.probes.at(-1);
            assert.ok(pending);
            finishProbe(pending, outcome);
            await nextTurn();
        },
        async draw() {
            const decoder = f.decoders.at(-1);
            assert.ok(decoder);
            decoder.outputCount ||= 0;
            while (decoder.outputCount < decoder.chunks.length) {
                const chunk = decoder.chunks[decoder.outputCount++];
                // Deterministic native-pixel fixture keyed to actual admitted bytes.
                let pattern = 19;
                for (const byte of chunk.data) pattern = (pattern * 31 + byte) & 255;
                const frame = {
                    timestamp: chunk.timestamp, pattern,
                    closed: false,
                    close() { assert.equal(this.closed, false); this.closed = true; },
                };
                decoder.callbacks.output(frame);
            }
            while (f.raf.length) f.raf.shift()();
            await nextTurn();
        },
    };
}

async function untilFirstPicture(f) {
    const completion = f.probe.start();
    await nextTurn();
    await f.configured();
    return { completion };
}

async function completeSequence(f) {
    const { completion } = await untilFirstPicture(f);
    await f.draw(); // Isolated IDR, no flush.
    await f.advance(1000);
    await f.draw(); // First P after idle.
    await f.draw(); // Burst final picture.
    await f.advance(250);
    await f.draw(); // Delayed final P.
    await f.configured(); // New decoder, same public sequence.
    await f.draw();
    await f.draw();
    return completion;
}

test("the public sample's exact hvcC and six complete access units retain their independent provenance", () => {
    const f = probeFixture();
    const sample = f.window.FrankenHevcProbeSample;
    const digest = createHash("sha256").update(sample.parameters("1").hvcc);
    for (let index = 0; index < 6; index++) digest.update(sample.picture(index));
    assert.equal(digest.digest("hex"), "c7ba68e2ca69d08df7116a26573d1036d7b3ed599095e7520b700812c92c8d87");
});

test("the fixed sample passes real Rust admission through idle, burst, reset and positive cleanup", async () => {
    const f = probeFixture();
    const result = await completeSequence(f);
    assert.equal(result.status, "passed");
    assert.equal(result.code, "browser_probe_passed");
    assert.equal(result.submitted, 8);
    assert.equal(result.presented, 6);
    assert.equal(result.released, 8);
    assert.equal(result.cleanupComplete, true);
    assert.equal(f.probe.busy, false);
    assert.equal(f.media.length, 1);
    assert.ok(f.media[0].freed());
    assert.equal(f.decoders.length, 2);
    assert.ok(f.decoders.every((decoder) => decoder.state === "closed"));
    assert.equal(f.timers.size, 0);
});

test("positive configuration without native output times out and releases its original decoder", async () => {
    const f = probeFixture();
    const { completion } = await untilFirstPicture(f);
    await f.advance(2000);
    const result = await completion;
    assert.equal(result.code, "browser_probe_frame_timeout");
    assert.equal(result.presented, 0);
    assert.equal(f.probe.busy, false);
    assert.ok(f.media[0].freed());
    assert.equal(f.decoders[0].state, "closed");
});

test("hung native capability probe has one retained owner until its late completion", async () => {
    const f = probeFixture();
    const completion = f.probe.start();
    await nextTurn();
    await f.advance(12000);
    assert.equal((await completion).code, "browser_probe_timeout");
    assert.equal(f.probe.busy, true);
    assert.equal(f.media[0].core.snapshot().includes('"retainedDecoders":1'), true);
    assert.equal((await f.probe.start()).code, "browser_probe_busy");
    assert.equal(f.media.length, 1);
    assert.equal(f.media[0].freed(), false);
    await f.configured();
    assert.equal(f.probe.busy, false);
    assert.equal(f.media[0].freed(), true);
    assert.equal(f.decoders.length, 0);
    assert.equal(f.states.at(-1).code, "browser_probe_timeout");
});

test("cancelled module loading remains bounded and creates no native owner", async () => {
    let resume;
    const loading = new Promise((resolve) => { resume = resolve; });
    const f = probeFixture({ loading });
    const completion = f.probe.start();
    f.probe.cancel();
    assert.equal((await completion).code, "browser_probe_cancelled");
    assert.equal((await f.probe.start()).code, "browser_probe_busy");
    resume();
    await nextTurn();
    assert.equal(f.media.length, 0);
    assert.equal(f.probe.busy, false);
});

test("native unsupported result cannot become a successful configuration-only result", async () => {
    const f = probeFixture();
    const completion = f.probe.start();
    await nextTurn();
    await f.configured("unsupported");
    assert.equal((await completion).code, "no_supported_hevc_decoder");
    assert.equal(f.decoders.length, 0);
    assert.equal(f.probe.busy, false);
});

for (const [options, reason] of [
    [{ blank: true }, "browser_probe_blank_canvas"],
    [{ unchanged: true }, "browser_probe_pixels_unchanged"],
    [{ readThrows: true }, "browser_probe_canvas_unavailable"],
]) {
    test("actual presentation/readback is required for " + reason, async () => {
        const f = probeFixture(options);
        const { completion } = await untilFirstPicture(f);
        await f.draw();
        if (options.unchanged) {
            await f.advance(1000);
            await f.draw();
        }
        assert.equal((await completion).code, reason);
        assert.equal(f.probe.busy, false);
        assert.ok(f.media[0].freed());
        assert.ok(!JSON.stringify(f.states).includes("private native"));
    });
}

for (const [event, expected] of [
    ["hidden", "browser_probe_hidden"],
    ["clock", "browser_probe_clock_invalid"],
    ["deadline", "browser_probe_timeout"],
]) {
    test("late native output cannot qualify a " + event + " attempt", async () => {
        const f = probeFixture();
        const { completion } = await untilFirstPicture(f);
        if (event === "hidden") f.document.hidden = true;
        else f.setClock(event === "clock" ? 999 : 14000);
        await f.draw();
        assert.equal((await completion).code, expected);
        assert.ok(f.states.every((state) => state.status !== "passed"));
        assert.equal(f.probe.busy, false);
        assert.ok(f.media[0].freed());
    });
}

test("failed native close prevents a pass and prevents allocating a replacement owner", async () => {
    const f = probeFixture({ closeThrows: true });
    const { completion } = await untilFirstPicture(f);
    f.probe.cancel();
    assert.equal((await completion).code, "browser_probe_cancelled");
    await nextTurn();
    assert.equal(f.probe.busy, true);
    assert.equal((await f.probe.start()).code, "browser_probe_busy");
    assert.equal(f.media.length, 1);
    assert.equal(f.media[0].freed(), false);
    f.decoders[0].closeThrows = false;
    f.probe.cancel(); // Retry cleanup under the same retained owner.
    assert.equal(f.probe.busy, false);
    assert.equal(f.media[0].freed(), true);
});

test("native cleanup returning after the total deadline cannot report a pass", async () => {
    let closed = 0;
    const f = probeFixture({ onDecoderClose() {
        if (++closed === 2) f.setClock(14000);
    } });
    const result = await completeSequence(f);
    assert.equal(result.status, "failed");
    assert.equal(result.code, "browser_probe_timeout");
    assert.equal(result.cleanupComplete, true);
    assert.ok(f.media[0].freed());
});
