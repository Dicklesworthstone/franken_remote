#!/usr/bin/env node
"use strict";

// Build the real browser protocol owner and its matching self-hosted glue.
// Generated assets stay outside the source tree and build.rs never downloads.
const fs = require("node:fs");
const path = require("node:path");
const { spawnSync } = require("node:child_process");

const root = path.resolve(__dirname, "..");
const expectedCompiler = "rustc 1.100.0-nightly (908501772 2026-08-30)";

function run(command, args, options = {}) {
    const result = spawnSync(command, args, {
        cwd: root,
        env: process.env,
        stdio: options.capture ? "pipe" : "inherit",
        encoding: "utf8",
        ...options,
    });
    if (result.error || result.status !== 0) {
        throw new Error(`${command} failed; browser assets were not qualified`);
    }
    return options.capture ? result.stdout.trim() : undefined;
}

function main() {
    if (process.argv.length !== 3 || process.argv[2] === "--help") {
        console.log("Usage: node scripts/build-web.cjs NEW_OUTPUT_DIRECTORY");
        console.log("Requires the repository-pinned compiler, wasm32-unknown-unknown, Node, and matching wasm-bindgen CLI.");
        process.exitCode = process.argv[2] === "--help" ? 0 : 2;
        return;
    }
    const output = path.resolve(process.argv[2]);
    if (fs.existsSync(output)) {
        throw new Error("Choose a new output directory; existing browser packages are never overwritten");
    }
    if (run("rustc", ["--version"], { capture: true }) !== expectedCompiler) {
        throw new Error("The exact compiler in rust-toolchain.toml is required");
    }
    const lock = fs.readFileSync(path.join(root, "Cargo.lock"), "utf8");
    const match = lock.match(/\[\[package\]\]\nname = "wasm-bindgen"\nversion = "([^"]+)"/);
    if (!match || run("wasm-bindgen", ["--version"], { capture: true }) !== `wasm-bindgen ${match[1]}`) {
        throw new Error("wasm-bindgen CLI must exactly match the wasm-bindgen version in Cargo.lock");
    }
    const target = path.resolve(root, process.env.CARGO_TARGET_DIR || "target");
    run("cargo", [
        "build", "-p", "fr-web", "--release", "--locked",
        "--target", "wasm32-unknown-unknown", "--target-dir", target,
    ]);
    fs.mkdirSync(path.dirname(output), { recursive: true });
    fs.mkdirSync(output);
    run("wasm-bindgen", [
        path.join(target, "wasm32-unknown-unknown", "release", "fr_web.wasm"),
        "--target", "web", "--no-typescript", "--out-name", "fr_web", "--out-dir", output,
    ]);
    fs.writeFileSync(path.join(output, "package.json"), '{"type":"module","private":true}\n', { flag: "wx" });
    run(process.execPath, [
        "--experimental-vm-modules", "--test", "crates/fr-web/tests/wasm_adapter.test.cjs",
        "web/tests/wasm_transport.cjs", "web/tests/wasm_loader.cjs",
        "web/tests/hevc_configuration.test.cjs", "web/tests/wasm_media_configuration.cjs",
        "web/tests/opus_probe.test.cjs",
        "web/tests/wasm_media.cjs", "web/tests/wasm_playback_probe.cjs",
    ], { env: { ...process.env, FR_WEB_WASM_PACKAGE: output } });
    console.log(`Browser package: ${output}`);
    console.log("Set FR_WEB_ASSET_DIR to this directory when building frd to embed both generated assets.");
}

try { main(); }
catch (error) {
    console.error(error.message);
    process.exitCode = 1;
}
