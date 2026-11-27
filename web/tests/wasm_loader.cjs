"use strict";

// Loader I/O/resource tests. Synthetic modules model package availability only;
// actual protocol behavior is covered separately by wasm_transport.cjs.
// Node's VM dynamic-import hook requires:
//   node --experimental-vm-modules --test web/tests/wasm_loader.cjs
const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const vm = require("node:vm");
const { getEventListeners } = require("node:events");
const test = require("node:test");
const { deferred, nextTurn, ioFixture } = require("./browser_fixture.cjs");
const source = fs.readFileSync(path.join(__dirname, "..", "wasm_client.js"), "utf8");

function fixture(options = {}) {
    const imports = [], initializations = [], adapters = [], media = [];
    const window = { location: { protocol: options.protocol || "https:" } };
    const context = vm.createContext({ window });
    const script = new vm.Script(source, {
        filename: "wasm_client.js",
        importModuleDynamically: async (specifier) => {
            const index = imports.length;
            imports.push(specifier);
            if (options.importHook) await options.importHook(index);
            const exports = {
                default: async (input) => {
                    const call = initializations.length;
                    initializations.push(input.module_or_path);
                    if (options.initHook) await options.initHook(call);
                },
                BrowserAdapter: class {
                    constructor() {
                        if (options.constructorFailure) throw new Error("private constructor detail");
                        this.freed = 0;
                        adapters.push(this);
                    }
                    free() { this.freed += 1; assert.equal(this.freed, 1); }
                },
                BrowserMediaAdapter: class {
                    constructor(frames, bytes) {
                        this.frames = frames; this.bytes = bytes;
                        this.retired = 0; this.freed = 0; media.push(this);
                        if (options.mediaConstructor) options.mediaConstructor();
                    }
                    retire() { this.retired++; }
                    free() { this.freed++; assert.equal(this.freed, 1); }
                },
                browserMediaAbiVersion: () => options.mediaAbi === undefined ? 1 : options.mediaAbi,
                browserAbiVersion: () => options.abi === undefined ? 2 : options.abi,
                maxBootstrapBytes: () => options.bootstrapLimit === undefined ? 4096 : options.bootstrapLimit,
                maxControlRecordBytes: () => options.controlLimit === undefined ? 4096 : options.controlLimit,
                maxVideoRecordBytes: () => options.videoLimit === undefined ? 65536 : options.videoLimit,
            };
            const module = new vm.SyntheticModule(Object.keys(exports), function () {
                for (const [name, value] of Object.entries(exports)) this.setExport(name, value);
            }, { context });
            await module.link(() => { throw new Error("unexpected package dependency"); });
            await module.evaluate();
            return module;
        },
    });
    script.runInContext(context);
    return { api: window.FrankenRust, imports, initializations, adapters, media, options };
}

function noListener(controller) {
    assert.equal(getEventListeners(controller.signal, "abort").length, 0);
}

test("plaintext and already-aborted callers never start package loading", async () => {
    const insecure = fixture({ protocol: "http:" });
    await assert.rejects(insecure.api.createAdapter(), { code: "browser_https_required" });
    assert.equal(insecure.imports.length, 0);
    const f = fixture();
    const controller = new AbortController();
    controller.abort();
    await assert.rejects(f.api.createAdapter(controller.signal), { code: "browser_connection_cancelled" });
    assert.equal(f.imports.length, 0);
    assert.equal(f.adapters.length, 0);
    noListener(controller);
});

test("one package load serves active subscribers while cancellation removes retired callers", { timeout: 3000 }, async () => {
    const gate = deferred();
    const f = fixture({ importHook: () => gate.promise });
    const retired = new AbortController(), active = new AbortController();
    const old = f.api.createAdapter(retired.signal);
    const cancelled = assert.rejects(old, { code: "browser_connection_cancelled" });
    const current = f.api.createAdapter(active.signal);
    retired.abort();
    await cancelled;
    noListener(retired);
    assert.equal(f.adapters.length, 0);
    gate.resolve();
    const binding = await current;
    assert.deepEqual(f.imports, ["/pkg/fr_web.js"]);
    assert.deepEqual(f.initializations, ["/pkg/fr_web_bg.wasm"]);
    assert.equal(f.adapters.length, 1, "a cancelled subscriber never constructs a Rust owner");
    assert.equal(binding.maxBootstrapBytes, 4096);
    assert.equal(binding.maxControlRecordBytes, 4096);
    noListener(active);
    binding.core.free();
});

test("repeated WSS cancellation and watchdog expiry do not accumulate module subscribers", { timeout: 5000 }, async () => {
    const gate = deferred();
    const loader = fixture({ importHook: () => gate.promise });
    const f = ioFixture();
    f.window.FrankenRust = loader.api;
    const client = f.makeClient();
    for (let index = 0; index < 40; index += 1) {
        const timedOut = index % 5 === 0;
        const pending = client.connect("observer");
        const rejected = assert.rejects(pending, {
            code: timedOut ? "browser_client_load_timeout" : "browser_connection_cancelled",
        });
        await nextTurn();
        const signal = client.connectionAttempt.abort.signal;
        if (timedOut) f.tick(f.window.performance.now() + 15000);
        else client.disconnect();
        await rejected;
        assert.equal(signal.aborted, true);
        assert.equal(getEventListeners(signal, "abort").length, 0);
        assert.equal(f.timers.size, 0);
        assert.equal(loader.adapters.length, 0);
    }
    assert.deepEqual(loader.imports, ["/pkg/fr_web.js"]);
    gate.resolve();
    await nextTurn();
    await nextTurn();
    assert.equal(loader.adapters.length, 0, "module completion cannot resurrect retired attempts");
    assert.equal(f.requests.length, 0);
    assert.equal(f.sockets.length, 0);
    const current = await loader.api.createAdapter();
    assert.equal(loader.adapters.length, 1);
    assert.equal(loader.imports.length, 1);
    current.core.free();
});

test("pending subscriber capacity is bounded and cancellation immediately releases a slot", { timeout: 3000 }, async () => {
    const gate = deferred();
    const f = fixture({ importHook: () => gate.promise });
    const controllers = Array.from({ length: 16 }, () => new AbortController());
    const pending = controllers.map((controller) => f.api.createAdapter(controller.signal));
    await assert.rejects(f.api.createAdapter(), { code: "browser_client_busy" });
    const cancelled = assert.rejects(pending[0], { code: "browser_connection_cancelled" });
    controllers[0].abort();
    await cancelled;
    const replacementController = new AbortController();
    const replacement = f.api.createAdapter(replacementController.signal);
    gate.resolve();
    const bindings = await Promise.all([...pending.slice(1), replacement]);
    assert.equal(bindings.length, 16);
    assert.equal(f.adapters.length, 16);
    assert.equal(f.imports.length, 1);
    for (const controller of [...controllers, replacementController]) noListener(controller);
    for (const binding of bindings) binding.core.free();
});

test("failed shared loading retires before retry subscribers are notified", { timeout: 3000 }, async () => {
    const gate = deferred();
    const f = fixture({
        importHook: async (index) => {
            if (index === 0) {
                await gate.promise;
                throw new Error("private credential in import failure");
            }
        },
    });
    const retried = [0, 1].map(() => f.api.createAdapter().catch((error) => {
        assert.equal(error.code, "browser_client_unavailable");
        assert.equal(error.message.includes("private"), false);
        return f.api.createAdapter();
    }));
    gate.resolve();
    const bindings = await Promise.all(retried);
    assert.equal(f.imports.length, 2);
    assert.equal(f.initializations.length, 1);
    assert.equal(f.adapters.length, 2);
    for (const binding of bindings) binding.core.free();
});

test("ABI or byte-limit mismatches refuse without constructing adapters and allow retry", async () => {
    for (const key of ["abi", "bootstrapLimit", "controlLimit", "videoLimit"]) {
        const options = { [key]: 99 };
        const f = fixture(options);
        await assert.rejects(f.api.createAdapter(), { code: "browser_client_version_mismatch" });
        assert.equal(f.adapters.length, 0);
        delete options[key];
        const binding = await f.api.createAdapter();
        assert.equal(f.imports.length, 2);
        assert.equal(f.adapters.length, 1);
        binding.core.free();
    }
});

test("an initialization failure after all subscribers cancel is consumed and permits retry", { timeout: 3000 }, async () => {
    const initialization = deferred();
    const f = fixture({
        initHook: (index) => index === 0 ? initialization.promise : undefined,
    });
    const controller = new AbortController();
    const pending = f.api.createAdapter(controller.signal);
    const cancelled = assert.rejects(pending, { code: "browser_connection_cancelled" });
    await nextTurn();
    assert.equal(f.initializations.length, 1);
    controller.abort();
    await cancelled;
    noListener(controller);
    initialization.reject(new Error("private initialization failure"));
    await nextTurn();
    const binding = await f.api.createAdapter();
    assert.equal(f.imports.length, 2);
    assert.equal(f.adapters.length, 1);
    binding.core.free();
});

test("a cached package still checks cancellation and sanitizes constructor failures", async () => {
    const options = {};
    const f = fixture(options);
    const first = await f.api.createAdapter();
    first.core.free();
    const controller = new AbortController();
    controller.abort();
    await assert.rejects(f.api.createAdapter(controller.signal), { code: "browser_connection_cancelled" });
    options.constructorFailure = true;
    await assert.rejects(f.api.createAdapter(), (error) => {
        assert.equal(error.code, "browser_client_unavailable");
        assert.equal(error.message.includes("private"), false);
        return true;
    });
    options.constructorFailure = false;
    const second = await f.api.createAdapter();
    assert.equal(f.imports.length, 1);
    assert.equal(f.adapters.length, 2);
    second.core.free();
});


test("media and startup subscribers share one bounded load without constructing cancelled owners", async () => {
    const gate = deferred();
    const f = fixture({ importHook: () => gate.promise });
    const cancelled = new AbortController();
    const old = f.api.createMediaAdapter(cancelled.signal);
    const rejected = assert.rejects(old, { code: "browser_connection_cancelled" });
    const startup = f.api.createAdapter();
    const active = f.api.createMediaAdapter(undefined, { frames: 2, bytes: 1234n });
    cancelled.abort();
    await rejected;
    gate.resolve();
    const [connection, media] = await Promise.all([startup, active]);
    assert.equal(f.imports.length, 1);
    assert.equal(f.adapters.length, 1);
    assert.equal(f.media.length, 1);
    assert.equal(media.frames, 2);
    assert.equal(media.bytes, 1234n);
    connection.core.free();
    media.free();
});

test("a just-created media owner is retired and freed when construction reenters cancellation", async () => {
    const controller = new AbortController();
    const f = fixture({ mediaConstructor: () => controller.abort() });
    await assert.rejects(f.api.createMediaAdapter(controller.signal), { code: "browser_connection_cancelled" });
    assert.equal(f.media.length, 1);
    assert.equal(f.media[0].retired, 1);
    assert.equal(f.media[0].freed, 1);
});

test("cached media creation checks cancellation before allocation and refuses invalid limits", async () => {
    const f = fixture();
    (await f.api.createAdapter()).core.free();
    const controller = new AbortController();
    const pending = f.api.createMediaAdapter(controller.signal);
    controller.abort();
    await assert.rejects(pending, { code: "browser_connection_cancelled" });
    assert.equal(f.media.length, 0);
    for (const limits of [{ frames: 5, bytes: 1n }, { frames: 1, bytes: -1n }, { frames: 1, bytes: 33554433n }]) {
        await assert.rejects(f.api.createMediaAdapter(undefined, limits), { code: "browser_client_version_mismatch" });
    }
    const wrong = fixture({ mediaAbi: 2 });
    await assert.rejects(wrong.api.createMediaAdapter(), { code: "browser_client_version_mismatch" });
    assert.equal(wrong.media.length, 0);
});
