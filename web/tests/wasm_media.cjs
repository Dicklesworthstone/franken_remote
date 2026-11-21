"use strict";

const assert = require("node:assert/strict");
const { before, test } = require("node:test");
const { initialize, fixture, parameters, finishProbe, configure, PICTURE } = require("./media_fixture.cjs");
before(initialize);

test("the exact Rust owner admits hvc1 bytes and returns custody only after native frame close", async () => {
    const f = fixture();
    const decoder = await configure(f);
    assert.equal(f.submit(9, "0", "18446744073709551615").admitted, true);
    assert.equal(decoder.chunks.length, 1);
    assert.equal(decoder.chunks[0].type, "key");
    assert.deepEqual(Array.from(decoder.chunks[0].data), Array.from(PICTURE.subarray(PICTURE.length - 20)));
    assert.equal(f.player.snapshot().inflightBytes, PICTURE.length);
    const frame = f.output();
    assert.equal(f.first.length, 1);
    assert.equal(f.first[0].sourceTimestampUs, "18446744073709551615");
    assert.equal(f.presented.length, 0);
    assert.equal(f.released.length, 0);
    f.raf.shift()();
    assert.equal(frame.closeCalls, 1);
    assert.equal(f.presented.length, 1);
    assert.equal(f.released.length, 1);
    assert.equal(f.released[0].decoder, "1");
    assert.equal(f.released[0].frameId, "9");
    assert.equal(f.released[0].releasedBytes, PICTURE.length);
    assert.equal(f.player.snapshot().inflightFrames, 0);
});

test("Rust count admission covers native outputs even when decodeQueueSize is zero", async () => {
    const f = fixture({ maxFrames: 2 });
    await configure(f);
    assert.equal(f.submit(1).admitted, true);
    f.output();
    assert.equal(f.submit(2).admitted, true);
    assert.equal(f.player.decoder.decodeQueueSize, 0);
    assert.equal(f.submit(3).reason, "browser_media_capacity");
    assert.equal(f.player.snapshot().inflightFrames, 2);
    f.raf.shift()();
    assert.equal(f.submit(3).admitted, true);
    assert.equal(f.player.snapshot().inflightFrames, 2);
});

test("Rust byte admission never expands a small local budget", async () => {
    const f = fixture({ maxBytes: PICTURE.length - 1 });
    await configure(f);
    assert.equal(f.submit().reason, "browser_media_capacity");
    assert.equal(f.player.snapshot().inflightBytes, 0);
    assert.equal(f.player.decoder.chunks.length, 0);
});

test("late same-generation output and error callbacks cannot present or fence a replacement", async () => {
    const f = fixture({ closeThrows: true });
    const old = await configure(f);
    assert.equal(f.submit(1).admitted, true);
    const current = await configure(f);
    current.closeThrows = false;
    assert.notEqual(current, old);
    assert.equal(f.submit(1).admitted, true);
    const stale = f.output(old);
    assert.equal(stale.closeCalls, 1);
    assert.equal(f.presented.length, 0);
    assert.equal(f.first.length, 0);
    assert.equal(f.player.decoder, current);
    assert.equal(f.player.configured, true);
    assert.equal(f.released[0].decoder, "1");
    assert.equal(f.player.snapshot().inflightFrames, 1);
    old.callbacks.error(new Error("private old failure"));
    assert.equal(f.player.decoder, current);
    assert.deepEqual(f.errors, []);
    f.output(current);
    f.raf.shift()();
    assert.equal(f.presented[0].decoder, "2");
    assert.equal(f.released[1].decoder, "2");
});

test("failed stale VideoFrame close remains charged after its original decoder finally closes", async () => {
    const f = fixture({ closeThrows: true });
    const old = await configure(f);
    assert.equal(f.submit().admitted, true);
    const current = await configure(f);
    current.closeThrows = false;
    old.closeThrows = false;
    const stale = f.output(old, 0, { closeThrows: true });
    assert.equal(old.state, "closed");
    assert.equal(f.player.snapshot().inflightFrames, 1);
    assert.equal(f.player.snapshot().retainedDecoders, 2);
    assert.equal(f.released.length, 0);
    assert.equal(f.player.decoder, current);
    stale.closeThrows = false;
    assert.equal(f.player.reset(), true);
    assert.equal(f.player.snapshot().inflightFrames, 0);
    assert.equal(f.released.length, 1);
    assert.equal(f.released[0].decoder, "1");
});

test("failed current output close retains custody and native error details stay private", async () => {
    const f = fixture();
    await configure(f);
    assert.equal(f.submit().admitted, true);
    const frame = f.output(f.player.decoder, 0, { closeThrows: true });
    f.raf.shift()();
    assert.equal(f.player.configured, false);
    assert.equal(f.player.snapshot().inflightFrames, 1);
    assert.equal(f.released.length, 0);
    assert.deepEqual(f.errors, ["browser_hevc_decoder_failed"]);
    frame.closeThrows = false;
    assert.equal(f.player.reset(), true);
    assert.equal(f.released.length, 1);
});

test("reset releases pending inputs only after the original native close returns successfully", async () => {
    const f = fixture({ closeThrows: true });
    const old = await configure(f);
    assert.equal(f.submit(1).admitted, true);
    assert.equal(f.submit(2).admitted, true);
    assert.equal(f.player.reset(), false);
    assert.equal(f.player.snapshot().inflightFrames, 2);
    assert.equal(f.released.length, 0);
    old.closeThrows = false;
    assert.equal(f.player.reset(), true);
    assert.equal(f.player.snapshot().inflightFrames, 0);
    assert.deepEqual(f.released.map((receipt) => receipt.frameId), ["1", "2"]);
    assert.ok(f.released.every((receipt) => receipt.decoder === "1"));
});

test("retained decoder instances exhaust the Rust limit before another native construction", async () => {
    const f = fixture({ closeThrows: true });
    for (let generation = 0; generation < 4; generation++) await configure(f, generation);
    assert.equal(f.player.snapshot().retainedDecoders, 4);
    const refused = await f.player.configure(parameters(4));
    assert.equal(refused.supported, false);
    assert.equal(f.decoders.length, 4);
    assert.equal(f.probes.length, 4);
    assert.equal(f.player.snapshot().retainedDecoders, 4);
    for (const decoder of f.decoders) decoder.closeThrows = false;
    assert.equal(f.player.reset(), true);
});

test("unabortable configuration probes retain bounded original slots until they settle", async () => {
    const f = fixture();
    const pending = [];
    for (let generation = 0; generation < 4; generation++) {
        pending.push(f.player.configure(parameters(generation)));
        assert.equal(f.player.reset(), false);
    }
    assert.equal(f.player.snapshot().retainedDecoders, 4);
    assert.equal((await f.player.configure(parameters(4))).supported, false);
    assert.equal(f.probes.length, 4);
    assert.equal(f.decoders.length, 0);
    for (const probe of f.probes) finishProbe(probe);
    for (const result of await Promise.all(pending)) assert.equal(result.cancelled, true);
    assert.equal(f.player.snapshot().retainedDecoders, 0);
    assert.equal(f.player.reset(), true);
    await configure(f, 4);
    assert.equal(f.player.snapshot().retainedDecoders, 1);
});

test("old animation callbacks never present a new decoder's pending frame", async () => {
    const f = fixture();
    await configure(f);
    assert.equal(f.submit().admitted, true);
    f.output();
    const oldAnimation = f.raf.shift();
    await configure(f, 1);
    assert.equal(f.pendingRaf.size, 0);
    assert.equal(f.submit().admitted, true);
    f.output();
    oldAnimation();
    assert.equal(f.draws.length, 0);
    assert.equal(f.presented.length, 0);
    assert.equal(f.player.snapshot().inflightFrames, 1);
    f.raf.shift()();
    assert.equal(f.presented.length, 1);
    assert.equal(f.presented[0].decoder, "2");
});

test("repeated reset cancels scheduled paint handles while animation is suspended", async () => {
    const f = fixture();
    for (let generation = 0; generation < 24; generation++) {
        await configure(f, generation);
        assert.equal(f.submit().admitted, true);
        f.output();
        assert.equal(f.pendingRaf.size, 1);
        assert.equal(f.player.reset(), true);
        assert.equal(f.pendingRaf.size, 0);
        assert.equal(f.player.owners.size, 0);
    }
    for (const lateCallback of f.raf) lateCallback();
    assert.equal(f.presented.length, 0);
    assert.equal(f.player.snapshot().inflightFrames, 0);
});

test("hidden output closes without presentation and without inventing source freshness", async () => {
    const f = fixture();
    await configure(f);
    f.player.setVisible(false);
    assert.equal(f.submit().admitted, true);
    const frame = f.output();
    assert.equal(frame.closeCalls, 1);
    assert.equal(f.raf.length, 0);
    assert.equal(f.presented.length, 0);
    assert.equal(f.player.snapshot().inflightFrames, 0);
    assert.equal(f.player.snapshot().isControlling, undefined);
    assert.equal(f.player.snapshot().viewFresh, undefined);
});

test("draw failure closes the original output and reports a static error", async () => {
    const f = fixture({ drawThrows: true });
    await configure(f);
    assert.equal(f.submit().admitted, true);
    const frame = f.output();
    f.raf.shift()();
    assert.equal(frame.closeCalls, 1);
    assert.equal(f.presented.length, 0);
    assert.equal(f.player.snapshot().inflightFrames, 0);
    assert.deepEqual(f.errors, ["browser_hevc_decoder_failed"]);
});

test("paint scheduling failure closes the escaped output without exposing native details", async () => {
    const f = fixture({ animationThrows: true });
    await configure(f);
    assert.equal(f.submit().admitted, true);
    const frame = f.output();
    assert.equal(frame.closeCalls, 1);
    assert.equal(f.player.configured, false);
    assert.equal(f.player.snapshot().inflightFrames, 0);
    assert.equal(f.player.snapshot().retainedDecoders, 0);
    assert.equal(f.presented.length, 0);
    assert.deepEqual(f.errors, ["browser_hevc_decoder_failed"]);
});

for (const failure of ["decodeThrows", "chunkThrows"]) {
    test(failure + " retires native custody without a fabricated output receipt", async () => {
        const f = fixture({ [failure]: true });
        const decoder = await configure(f);
        assert.equal(f.submit().reason, "decode_submit_failed");
        assert.equal(decoder.closeCalls, 1);
        assert.equal(f.player.snapshot().inflightFrames, 0);
        assert.equal(f.first.length, 0);
        assert.equal(f.presented.length, 0);
        assert.equal(f.released.length, 1);
        assert.deepEqual(f.errors, ["browser_hevc_decoder_failed"]);
    });
}

test("a release callback replacing the decoder cannot make the obsolete output visible", async () => {
    let successor, f;
    let replaced = false;
    f = fixture({ onFrameClosed() {
        if (!replaced) { replaced = true; successor = f.player.configure(parameters(1)); }
    } });
    const old = await configure(f);
    assert.equal(f.submit(1).admitted, true);
    f.output(old, 0);
    assert.equal(f.submit(2).admitted, true);
    const second = f.output(old, 1);
    assert.equal(second.closeCalls, 1);
    assert.equal(f.player.pendingFrame, null);
    finishProbe(f.probes.at(-1));
    assert.equal((await successor).supported, true);
    for (const paint of f.raf) paint();
    assert.equal(f.presented.length, 0);
    assert.equal(f.player.snapshot().inflightFrames, 0);
    assert.equal(f.player.snapshot().activeDecoder, "2");
});

test("application notification mutation cannot redirect the original frame's release receipt", async () => {
    const f = fixture({
        onFirstFrameDecoded(info) { info.callback = 2; info.frameId = "2"; },
        onFramePresented(info) { info.callback = 999; info.decoder = "999"; },
    });
    const decoder = await configure(f);
    assert.equal(f.submit(1).admitted, true);
    assert.equal(f.submit(2).admitted, true);
    const first = f.output(decoder, 0);
    const second = f.output(decoder, 1);
    assert.equal(first.closeCalls, 1);
    assert.equal(second.closeCalls, 0);
    assert.equal(f.released.length, 1);
    assert.equal(f.released[0].frameId, "1");
    assert.equal(f.player.snapshot().inflightFrames, 1);
    f.raf.shift()();
    assert.equal(second.closeCalls, 1);
    assert.equal(f.released[1].frameId, "2");
    assert.equal(f.player.snapshot().inflightFrames, 0);
    assert.equal(f.player.snapshot().quarantined, false);
});

test("duplicate native outputs cannot free or mutate another current charge", async () => {
    const f = fixture();
    const decoder = await configure(f);
    assert.equal(f.submit(1).admitted, true);
    const first = f.output();
    const duplicate = f.output(decoder);
    assert.equal(duplicate.closeCalls, 1);
    assert.equal(first.closeCalls, 1);
    assert.equal(f.released.length, 1);
    assert.equal(f.presented.length, 0);
    assert.equal(f.player.snapshot().inflightFrames, 0);
});

test("unowned output close failures terminally quarantine bounded metadata without releasing custody", async () => {
    const f = fixture();
    const decoder = await configure(f);
    assert.equal(f.submit(1).admitted, true);
    const known = f.output();
    const orphan = f.output(decoder, 0, { closeThrows: true });
    assert.equal(orphan.closeCalls, 1);
    assert.equal(known.closeCalls, 1);
    for (let index = 0; index < 100; index++) {
        const extra = f.output(decoder, 0, { closeThrows: true });
        assert.equal(extra.closeCalls, 1);
    }
    assert.equal(f.player.quarantineFrame, orphan);
    assert.equal(f.player.owners.size, 1);
    assert.equal([...f.player.owners][0].frames.size, 1);
    assert.equal(f.player.snapshot().quarantined, true);
    assert.equal(f.player.snapshot().unclosedUnownedOutputs, 101);
    assert.equal(f.player.snapshot().inflightFrames, 1);
    assert.equal(f.player.snapshot().inflightBytes, PICTURE.length);
    assert.equal(f.player.snapshot().retainedDecoders, 1);
    assert.equal(f.player.reset(), false);
    assert.equal((await f.player.configure(parameters(1))).code, "browser_media_cleanup_incomplete");
    assert.equal(f.probes.length, 1);
    assert.equal(f.released.length, 0);
    assert.equal(f.presented.length, 0);
    assert.deepEqual(f.errors, ["browser_media_cleanup_incomplete"]);
    assert.throws(() => f.media.resetSession(), (error) => error === "browser_media_cleanup_incomplete");
});

test("native closure proof survives a failed Rust acknowledgment and retries its original owner", async () => {
    const f = fixture();
    const decoder = await configure(f);
    assert.equal(f.submit(1).admitted, true);
    // Fault the JS binding call once, then execute the actual Rust method. No
    // substitute owner computes admission or cleanup in this boundary test.
    const closed = f.media.decoderClosed.bind(f.media);
    let fail = true;
    f.media.decoderClosed = (owner) => {
        if (fail) { fail = false; throw new Error("private binding failure"); }
        return closed(owner);
    };
    assert.equal(f.player.reset(), false);
    const owner = [...f.player.owners][0];
    assert.equal(owner.nativeClosed, true);
    assert.equal(owner.reconciled, false);
    assert.equal(decoder.closeCalls, 1);
    assert.equal(f.player.snapshot().inflightFrames, 1);
    assert.equal(f.released.length, 0);
    assert.equal(f.player.reset(), true);
    assert.equal(decoder.closeCalls, 1);
    assert.equal(f.player.snapshot().inflightFrames, 0);
    assert.equal(f.released.length, 1);
    assert.equal(f.released[0].decoder, "1");
});

test("a failed quarantine binding still blocks both closure acknowledgments and release notifications", async () => {
    const f = fixture();
    const decoder = await configure(f);
    assert.equal(f.submit(1).admitted, true);
    assert.equal(f.submit(2).admitted, true);
    const frame = f.output(decoder, 0);
    // Only quarantine is faulted before reaching Rust. Both closure bindings
    // still delegate to the working actual WASM owner if the player calls them.
    const frameClosed = f.media.frameClosed.bind(f.media);
    const decoderClosed = f.media.decoderClosed.bind(f.media);
    let closureCalls = 0;
    f.media.quarantine = () => { throw new Error("private quarantine binding failure"); };
    f.media.frameClosed = (...args) => { closureCalls++; return frameClosed(...args); };
    f.media.decoderClosed = (...args) => { closureCalls++; return decoderClosed(...args); };
    const orphan = f.output(decoder, 0, { closeThrows: true });
    assert.equal(orphan.closeCalls, 1);
    assert.equal(frame.closeCalls, 1);
    assert.equal(decoder.closeCalls, 1);
    assert.equal(JSON.parse(f.media.snapshot()).quarantined, false);
    assert.equal(f.player.snapshot().quarantined, true);
    assert.equal(f.player.snapshot().stage, "quarantined");
    assert.equal(f.player.snapshot().activeDecoder, null);
    assert.equal(f.player.snapshot().inflightFrames, 2);
    assert.equal(f.player.snapshot().inflightBytes, PICTURE.length * 2);
    assert.equal(f.player.reset(), false);
    assert.equal((await f.player.configure(parameters(1))).code, "browser_media_cleanup_incomplete");
    assert.equal(closureCalls, 0);
    assert.equal(f.released.length, 0);
    assert.deepEqual(f.errors, ["browser_media_cleanup_incomplete"]);
});

test("quarantine during a release callback suppresses the rest of an already returned Rust batch", async () => {
    let f, decoder;
    f = fixture({ onFrameClosed() { f.output(decoder, 0, { closeThrows: true }); } });
    decoder = await configure(f);
    assert.equal(f.submit(1).admitted, true);
    assert.equal(f.submit(2).admitted, true);
    f.media.quarantine = () => { throw new Error("private quarantine binding failure"); };
    assert.equal(f.player.reset(), false);
    // decoderClosed legitimately completed before the first notification. The
    // callback's terminal fence must suppress the second, already-made receipt.
    assert.equal(JSON.parse(f.media.snapshot()).inflightFrames, 0);
    assert.equal(JSON.parse(f.media.snapshot()).quarantined, false);
    assert.equal(f.player.snapshot().quarantined, true);
    assert.equal(f.released.length, 1);
    assert.equal(f.released[0].frameId, "1");
    assert.equal(f.player.snapshot().unclosedUnownedOutputs, 1);
    assert.equal(f.player.reset(), false);
    assert.deepEqual(f.errors, ["browser_media_cleanup_incomplete"]);
});

for (const failure of ["constructorError", "configureError"]) {
    test(failure + " synchronous callback cannot retire a native call still returning its handle", async () => {
        const f = fixture({ [failure]: true });
        const pending = f.player.configure(parameters());
        finishProbe(f.probes[0]);
        assert.equal((await pending).cancelled, true);
        assert.equal(f.decoders.length, 1);
        assert.equal(f.decoders[0].closeCalls, 1);
        assert.equal(f.decoders[0].state, "closed");
        assert.equal(f.player.configured, false);
        assert.equal(f.player.owners.size, 0);
        assert.equal(f.player.snapshot().retainedDecoders, 0);
        assert.deepEqual(f.errors, ["browser_hevc_decoder_failed"]);
    });
}
