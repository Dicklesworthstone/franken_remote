"use strict";

// Real Rust/WASM configuration owner with simulated asynchronous WebCodecs.
// No native/browser/tailnet qualification is implied.
const assert = require("node:assert/strict");
const { before, test } = require("node:test");
const { initialize, fixture, parameters, finishProbe, configure, HVCC } = require("./media_fixture.cjs");
before(initialize);

function assertCancelled(result) {
    assert.equal(result.supported, false);
    assert.equal(result.cancelled, true);
    assert.equal(result.code, "decoder_configuration_cancelled");
}

for (const outcome of ["supported", "unsupported", "rejected"]) {
    test("reset fences a pending " + outcome + " probe before canvas, decoder or callback mutation", async () => {
        const f = fixture();
        const pending = f.player.configure(parameters());
        assert.equal(f.player.reset(), false, "the uncancellable probe retains its original slot");
        finishProbe(f.probes[0], outcome);
        assertCancelled(await pending);
        assert.equal(f.player.decoder, null);
        assert.equal(f.player.configured, false);
        assert.equal(f.decoders.length, 0);
        assert.equal(f.configured.length, 0);
        assert.equal(f.canvasWrites.length, 0);
        assert.equal(f.player.snapshot().retainedDecoders, 0);
    });

    test("late " + outcome + " probe cannot replace a newer decoder of the same wire generation", async () => {
        const f = fixture();
        const first = f.player.configure(parameters());
        const second = f.player.configure(parameters());
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
        assert.equal(f.configured[0].generation, "0");
        assert.equal(f.configured[0].decoder, "2");
        assert.equal(f.canvas.width, 320);
        assert.equal(f.canvas.height, 240);
        assert.equal(f.canvasWrites.length, 2);
    });
}

test("an earlier probe completing first stays cancelled while its replacement is pending", async () => {
    const f = fixture();
    const first = f.player.configure(parameters());
    const second = f.player.configure(parameters(1));
    finishProbe(f.probes[0], "supported");
    assertCancelled(await first);
    assert.equal(f.decoders.length, 0);
    assert.equal(f.canvasWrites.length, 0);
    finishProbe(f.probes[1], "supported");
    assert.equal((await second).supported, true);
    assert.equal(f.player.activeGeneration, "1");
    assert.deepEqual(f.configured.map((info) => info.generation), ["1"]);
});

test("a newer refusal cannot be undone by an older successful probe", async () => {
    const f = fixture();
    const first = f.player.configure(parameters());
    const second = f.player.configure(parameters(1));
    finishProbe(f.probes[1], "unsupported");
    assert.equal((await second).code, "no_supported_hevc_decoder");
    finishProbe(f.probes[0], "supported");
    assertCancelled(await first);
    assert.equal(f.player.decoder, null);
    assert.equal(f.decoders.length, 0);
    assert.equal(f.configured.length, 0);
    assert.equal(f.canvasWrites.length, 0);
    assert.equal(f.player.snapshot().retainedDecoders, 0);
});

test("Rust validates and captures negotiated metadata and an offset hvcC view before awaiting", async () => {
    const f = fixture();
    const request = parameters();
    const storage = new Uint8Array(HVCC.length + 10);
    storage.set(request.hvcc, 5);
    request.hvcc = storage.subarray(5, 5 + HVCC.length);
    const original = Array.from(request.hvcc);
    const pending = f.player.configure(request);
    request.generation = 99;
    request.codec = "mutated-codec";
    request.codedWidth = 1;
    request.codedHeight = 2;
    storage.fill(255);
    f.probes[0].resolve({ supported: true, config: { codec: "different-codec" } });
    assert.equal((await pending).supported, true);
    const decoder = f.player.decoder;
    assert.equal(decoder.config.codec, "hvc1.1.6.L60.90");
    assert.equal(decoder.config.codedWidth, 320);
    assert.equal(decoder.config.codedHeight, 240);
    assert.deepEqual(Array.from(decoder.config.description), original);
    assert.equal(f.player.activeGeneration, "0");
    assert.deepEqual(f.configured, [{ decoder: "1", generation: "0", codec: "hvc1.1.6.L60.90" }]);
});

test("reset invalidates the configuration before actual tracked frame-close callbacks run", async () => {
    let callbackOwner = "not-called", f;
    f = fixture({ onFrameClosed() { callbackOwner = f.player.configurationOwner; } });
    await configure(f);
    assert.equal(f.submit().admitted, true);
    const frame = f.output();
    assert.equal(f.player.reset(), true);
    assert.equal(callbackOwner, null);
    assert.equal(frame.closeCalls, 1);
    assert.equal(f.player.snapshot().inflightFrames, 0);
});

test("cleanup reentrancy cannot install a retired configuration over its successor", async () => {
    let successor, f;
    f = fixture({ onFrameClosed() { successor = f.player.configure(parameters(3)); } });
    const oldDecoder = await configure(f, 1);
    assert.equal(f.submit().admitted, true);
    f.output();
    const pending = f.player.configure(parameters(2));
    assertCancelled(await pending);
    assert.equal(oldDecoder.closeCalls, 1);
    assert.equal(f.decoders.length, 1);
    assert.equal(f.player.decoder, null);
    finishProbe(f.probes[1], "supported");
    assert.equal((await successor).supported, true);
    assert.equal(f.player.activeGeneration, "3");
    assert.deepEqual(f.configured.map((info) => info.generation), ["1", "3"]);
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
    assert.equal(f.player.snapshot().retainedDecoders, 0);
});

test("invalid hvcC and metadata refuse before constructing or probing a native decoder", async () => {
    for (const patch of [
        { hvcc: new Uint8Array(23) }, { codedWidth: 8193 }, { cropWidth: 321 },
        { decodedPictures: 12 }, { codec: "hev1.1.6.L99.90" },
        { generation: 9007199254740992 },
    ]) {
        const f = fixture();
        const result = await f.player.configure({ ...parameters(), ...patch });
        assert.equal(result.supported, false);
        assert.equal(f.probes.length, 0);
        assert.equal(f.decoders.length, 0);
        assert.equal(f.player.snapshot().retainedDecoders, 0);
    }
});

test("configure failure retains partial native custody when close throws", async () => {
    const f = fixture({ configureThrows: "private-config-error", closeThrows: true });
    const pending = f.player.configure(parameters());
    finishProbe(f.probes[0], "supported");
    const result = await pending;
    assert.equal(result.code, "hevc_decoder_configuration_failed");
    assert.equal(result.reason.includes("private-"), false);
    assert.equal(f.decoders.length, 1);
    assert.equal(f.decoders[0].closeCalls, 1);
    assert.equal(f.player.decoder, null);
    assert.equal(f.player.configured, false);
    assert.equal(f.player.snapshot().retainedDecoders, 1);
    assert.equal(f.player.reset(), false);
    f.decoders[0].closeThrows = false;
    assert.equal(f.player.reset(), true);
    assert.equal(f.player.snapshot().retainedDecoders, 0);
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
        assert.equal(f.player.snapshot().retainedDecoders, 0);
    }
});
