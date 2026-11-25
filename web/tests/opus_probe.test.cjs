"use strict";
const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const { test } = require("node:test");
const vm = require("node:vm");
const { deferred, nextTurn } = require("./browser_fixture.cjs");
const script = fs.readFileSync(path.join(__dirname, "..", "opus_probe.js"), "utf8");

function fixture(options = {}) {
    const reports = [], frames = [], submissions = [], contexts = [];
    let ended, output, now = 100, timer;
    const window = {
        isSecureContext: true, atob,
        performance: { now: () => now },
        setTimeout(cb) { timer = cb; return 1; }, clearTimeout() { timer = null; },
        EncodedAudioChunk: class { constructor(fields) { Object.assign(this, fields); } },
        AudioContext: class {
            constructor() { contexts.push(this); this.state = "running"; }
            resume() { this.resumed = true; return options.resume || Promise.resolve(); }
            createBuffer(channels, count, rate) {
                assert.equal(channels, 1); assert.equal(rate, 48000); assert.equal(count, 5760);
                const data = new Float32Array(count);
                return { getChannelData: () => data };
            }
            createBufferSource() {
                return { connect() {}, disconnect() {}, start() { ended = this.onended; } };
            }
            async close() {
                if (options.close) await options.close();
                this.state = "closed";
            }
        },
        AudioDecoder: class {
            constructor(callbacks) { output = callbacks.output; this.state = "unconfigured"; }
            configure(c) {
                assert.equal(c.codec, "opus"); assert.equal(c.numberOfChannels, 1);
                assert.equal(c.sampleRate, 48000); this.state = "configured";
            }
            decode(chunk) { submissions.push(chunk); }
            flush() {
                for (const chunk of submissions.slice(-6)) {
                    const frame = {
                        timestamp: chunk.timestamp, numberOfFrames: 960, numberOfChannels: 1, sampleRate: 48000,
                        copyTo(dest) {
                            const offset = chunk.timestamp * 48000 / 1e6;
                            for (let i = 0; i < dest.length; i++) dest[i] = 0.05 * Math.sin(2 * Math.PI * 440 * (offset + i) / 48000);
                        },
                        close() { this.closed = (this.closed || 0) + 1; },
                        ...options.frame,
                    };
                    frames.push(frame); output(frame);
                }
                return options.flush || Promise.resolve();
            }
            close() { this.state = "closed"; }
        },
    };
    vm.runInNewContext(script, { window });
    const probe = new window.FrankenOpusProbe((state) => reports.push(state));
    return {
        window, reports, frames, submissions, contexts, probe, ended: () => ended?.(),
        lateFrame: (frame) => output(frame), time(value) { now = value; },
        timeout: () => timer?.(), get started() { return !!ended; },
    };
}

test("resume in the gesture, decode six packets and await graph completion", async () => {
    const f = fixture();
    f.probe.start();
    assert.equal(f.contexts[0].resumed, true);
    f.probe.start();
    await nextTurn();
    assert.equal(f.contexts.length, 1);
    assert.deepEqual(f.submissions.map((c) => c.timestamp), [0, 20000, 40000, 60000, 80000, 100000]);
    assert.ok(f.submissions.every((c) => c.type === "key" && c.duration === 20000 && c.data.length <= 1275));
    assert.equal(f.frames.length, 6);
    assert.ok(f.frames.every((d) => d.closed === 1));
    assert.deepEqual(f.reports, ["running"]);
    assert.ok(f.started);
    f.ended();
    await nextTurn();
    assert.equal(f.reports.at(-1), "passed");
    assert.equal(f.contexts[0].state, "closed");
    assert.equal(f.probe.busy, false);
});

for (const [name, frame] of Object.entries({
    length: { numberOfFrames: 1920 }, timestamp: { timestamp: 500 },
    rate: { sampleRate: 44100 }, channels: { numberOfChannels: 2 },
    silence: { copyTo(dest) { dest.fill(0); } }, infinity: { copyTo(dest) { dest.fill(Infinity); } },
    dc: { copyTo(dest) { dest.fill(0.05); } },
    loud: { copyTo(dest) { dest.fill(0.5); } },
})) {
    test("invalid decoder output is released without playback: " + name, async () => {
        const f = fixture({ frame });
        f.probe.start();
        await nextTurn();
        assert.equal(f.reports.at(-1), "invalid_output");
        assert.ok(f.frames.every((d) => d.closed === 1));
        assert.equal(f.started, false);
    });
}

test("pending resume times out and cannot open a decoder later", async () => {
    const resume = deferred();
    const f = fixture({ resume: resume.promise });
    f.probe.start();
    f.timeout();
    resume.resolve();
    await nextTurn();
    assert.equal(f.submissions.length, 0);
    assert.equal(f.reports.at(-1), "timeout");
});
test("overdue decoder completion cannot schedule playback before a delayed timer fires", async () => {
    const flush = deferred();
    const f = fixture({ flush: flush.promise });
    f.probe.start();
    await nextTurn();
    f.time(3100);
    flush.resolve();
    await nextTurn();
    assert.equal(f.started, false);
    assert.equal(f.reports.at(-1), "timeout");
});
test("cancel fences delayed output and waits for original context disposal", async () => {
    const close = deferred(), flush = deferred();
    const f = fixture({ flush: flush.promise, close: () => close.promise });
    f.probe.start();
    await nextTurn();
    f.probe.cancel();
    f.probe.start();
    assert.equal(f.contexts.length, 1);
    let released = 0;
    f.lateFrame({ close() { released++; } });
    assert.equal(released, 1);
    flush.resolve();
    await nextTurn();
    assert.equal(f.started, false);
    assert.equal(f.probe.busy, true);
    close.resolve();
    await nextTurn();
    assert.equal(f.probe.busy, false);
    assert.equal(f.reports.at(-1), "cancelled");
});
test("failed cleanup blocks a replacement until a successful retry", async () => {
    let calls = 0;
    const f = fixture({ close: () => { if (++calls === 1) throw Error(); } });
    f.probe.start();
    await nextTurn();
    f.ended();
    await nextTurn();
    assert.equal(f.reports.at(-1), "cleanup_failed");
    f.probe.start();
    assert.equal(f.contexts.length, 1);
    f.probe.cancel();
    await nextTurn();
    assert.equal(f.reports.at(-1), "passed");
    assert.equal(f.probe.busy, false);
});
test("an unsupported browser allocates no audio resources", () => {
    const f = fixture();
    f.window.AudioDecoder = undefined;
    f.probe.start();
    assert.deepEqual(f.reports, ["unavailable"]);
    assert.equal(f.contexts.length, 0);
});
