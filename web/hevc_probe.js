"use strict";
// FrankenRemote WebCodecs HEVC capability probe and bounded decoder pipeline.
//
// Constitutional invariants (AGENTS.md §§3.3, 5; Plan §§11, 16.3):
// - HEVC only (Main, 8-bit, 4:2:0 baseline). Never silently fall back to H.264/VP9.
// - Never hard-code an hvc1 codec string for playback; always configure from the
//   exact negotiated hvcC description and SPS-derived codec identifier supplied
//   by the Rust HevcGuard / host DecoderConfiguration record.
// - Zero per-frame flush(): flush stalls hardware decoders and destroys freshness.
// - Rust bounds admitted inputs and retained native output. Original resource
//   receipts follow positive closure; this media boundary grants no authority
//   and cannot generate a session's receiver-credit record itself.

(function (global) {
    const MAX_DECODE_QUEUE_SIZE = 4;
    const MAX_NATIVE_OWNERS = 4;

    function cancelledConfiguration() {
        return {
            supported: false,
            cancelled: true,
            code: "decoder_configuration_cancelled",
        };
    }

    function failedConfiguration() {
        return {
            supported: false,
            code: "hevc_decoder_configuration_failed",
            reason: "Browser could not create or configure the negotiated HEVC decoder.",
        };
    }

    async function probeHevcWebCodecs(codecString, hvccBytes, width, height) {
        if (typeof codecString !== "string" || codecString.length > 64
            || !/^hvc1\.[A-Za-z0-9.]+$/.test(codecString)
            || !Number.isInteger(width) || !Number.isInteger(height)
            || width <= 0 || height <= 0 || width > 8192 || height > 8192
            || width * height > 16777216
            || !(ArrayBuffer.isView(hvccBytes) || hvccBytes instanceof ArrayBuffer)
            || hvccBytes.byteLength < 23 || hvccBytes.byteLength > 12326) {
            return refusal("browser_media_configuration_invalid");
        }
        if (typeof global.VideoDecoder !== "function" || typeof global.EncodedVideoChunk !== "function") {
            return {
                supported: false,
                code: "webcodecs_unavailable",
                reason: "Browser does not expose WebCodecs VideoDecoder; HEVC playback cannot proceed.",
            };
        }

        try {
            const config = {
                codec: codecString,
                codedWidth: width,
                codedHeight: height,
                hardwareAcceleration: "prefer-hardware",
                optimizeForLatency: true,
            };
            if (hvccBytes && hvccBytes.byteLength >= 23) {
                // Keep the exact negotiated description independent of the caller
                // while the browser's asynchronous support probe is outstanding.
                const bytes = ArrayBuffer.isView(hvccBytes)
                    ? new Uint8Array(hvccBytes.buffer, hvccBytes.byteOffset, hvccBytes.byteLength)
                    : new Uint8Array(hvccBytes);
                config.description = bytes.slice();
            }
            const support = await global.VideoDecoder.isConfigSupported(config);
            if (!support || !support.supported) {
                return {
                    supported: false,
                    code: "no_supported_hevc_decoder",
                    reason: "Browser VideoDecoder refused the negotiated HEVC Main 8-bit configuration.",
                };
            }
            return {
                supported: true,
                code: "hevc_webcodecs_ready",
                config: config,
            };
        } catch (_) {
            return {
                supported: false,
                code: "hevc_probe_failed",
                reason: "Browser could not probe the negotiated HEVC configuration.",
            };
        }
    }

    function integer(value) {
        if (typeof value === "number") {
            if (!Number.isSafeInteger(value) || value < 0) throw "browser_media_owner_invalid";
            return BigInt(value);
        }
        if (typeof value === "string" && !/^(0|[1-9][0-9]{0,19})$/.test(value)) {
            throw "browser_media_owner_invalid";
        }
        if (typeof value !== "string" && typeof value !== "bigint") throw "browser_media_owner_invalid";
        const number = BigInt(value);
        if (number < 0n || number > 18446744073709551615n) throw "browser_media_owner_invalid";
        return number;
    }

    function refusal(code) {
        return { supported: false, code, reason: "The browser media operation could not complete safely." };
    }

    // A player receives one actual BrowserMediaAdapter from the generated WASM
    // package. There is no JavaScript protocol, HEVC guard, charge ledger or
    // authority fallback. The owner must outlive its native decoder/frame handles.
    class WebCodecsHevcPlayer {
        constructor(canvas, callbacks, media, options) {
            this.canvas = canvas;
            this.ctx = canvas.getContext("2d", { alpha: false, desynchronized: true });
            this.callbacks = callbacks || {};
            this.media = media;
            this.nowMs = options && options.nowMs || (() => global.performance.now());
            this.visible = !document.hidden;
            this.current = null;
            this.owners = new Set();
            this.pendingFrame = null;
            this.configurationOwner = null;
            this.decoder = null;
            this.configured = false;
            this.activeGeneration = null;
            this.unknownOutput = false;
            this.quarantineFrame = null;
            this.unclosedUnownedOutputs = 0;
        }

        _owns(owner) {
            return this.current === owner && !owner.retired;
        }

        _notify(name, detail) {
            // Application notification failures cannot skip native cleanup.
            // A listener also cannot rewrite the receipt used later to close
            // its original frame. All ABI notification fields are primitives.
            try {
                if (typeof this.callbacks[name] === "function") {
                    this.callbacks[name](detail && typeof detail === "object" ? Object.assign({}, detail) : detail);
                }
            } catch (_) {}
        }

        _release(receipt) {
            // This is one original decoder's resource receipt, NOT a cumulative
            // ReceiverPressure record. Only the authenticated channel owner can
            // turn it into credit on its original channel/epoch.
            // Quarantine is also a JS boundary fence: its Rust binding may
            // throw, or a callback may quarantine midway through a release batch.
            if (!this.unknownOutput) this._notify("onFrameClosed", receipt);
        }

        setVisible(visible) {
            this.visible = Boolean(visible);
            if (!this.visible && this.pendingFrame) this._closeFrame(this.pendingFrame);
        }

        async configure(params) {
            const request = {};
            this.configurationOwner = request;
            if (!this.media || typeof this.media.prepareConfiguration !== "function") {
                return refusal("browser_media_owner_unavailable");
            }
            if (this.unknownOutput) return refusal("browser_media_cleanup_incomplete");
            // Fence before browser cleanup, which may invoke callbacks/reenter.
            this.media.retire();
            this.current = null;
            this.decoder = null;
            this.configured = false;
            for (const owner of Array.from(this.owners)) this._retire(owner);
            if (this.configurationOwner !== request) return cancelledConfiguration();
            // Also bound JS handle metadata if a binding acknowledgment fails
            // after Rust has already applied it. Rust remains the admission owner.
            if (this.owners.size >= MAX_NATIVE_OWNERS) return refusal("browser_decoder_capacity");
            let prepared;
            try {
                if (!params || !(params.hvcc instanceof Uint8Array)
                    || params.hvcc.byteLength > this.media.maxConfigurationBytes()
                    || typeof params.codec !== "string" || params.codec.length > 64
                    || typeof params.fullRange !== "boolean"
                    || ![params.codedWidth, params.codedHeight, params.cropWidth, params.cropHeight,
                        params.fps, params.primaries, params.transfer, params.matrix,
                        params.decodedPictures].every((value) => Number.isSafeInteger(value) && value >= 0)) {
                    return refusal("browser_media_configuration_invalid");
                }
                const metadata = {
                    generation: integer(params.generation).toString(),
                    codedWidth: params.codedWidth, codedHeight: params.codedHeight,
                    cropWidth: params.cropWidth, cropHeight: params.cropHeight,
                    fps: params.fps, primaries: params.primaries, transfer: params.transfer,
                    matrix: params.matrix, fullRange: params.fullRange,
                    decodedPictures: params.decodedPictures, codec: params.codec,
                };
                prepared = JSON.parse(this.media.prepareConfiguration(JSON.stringify(metadata), params.hvcc));
            } catch (_) { return refusal("browser_media_configuration_invalid"); }
            const owner = {
                id: integer(prepared.decoder), generation: prepared.generation,
                decoder: null, retired: false, nativeClosed: false, reconciled: false, probing: true,
                closeFailed: false, closing: false, nativeBusy: 0, frames: new Set(),
                raf: false, rafHandle: null,
                width: prepared.cropWidth, height: prepared.cropHeight,
            };
            this.owners.add(owner);
            this.current = owner;
            const probe = await probeHevcWebCodecs(
                prepared.codec, Uint8Array.from(prepared.hvcc), prepared.codedWidth, prepared.codedHeight
            );
            owner.probing = false;
            if (this.configurationOwner !== request || !this._owns(owner)) {
                this._retire(owner);
                return cancelledConfiguration();
            }
            if (!probe.supported) {
                this._retire(owner);
                return this.configurationOwner === request ? probe : cancelledConfiguration();
            }
            try {
                owner.nativeBusy += 1;
                try {
                    owner.decoder = new global.VideoDecoder({
                        output: (frame) => this._output(owner, frame),
                        error: () => this._error(owner),
                    });
                    if (this.configurationOwner === request && this._owns(owner)) {
                        owner.decoder.configure(probe.config);
                    }
                } finally { owner.nativeBusy -= 1; }
                if (this.configurationOwner !== request || !this._owns(owner)) {
                    this._retire(owner);
                    return cancelledConfiguration();
                }
                this.media.decoderConfigured(owner.id);
            } catch (_) {
                this._retire(owner);
                return this.configurationOwner === request ? failedConfiguration() : cancelledConfiguration();
            }
            this.decoder = owner.decoder;
            this.configured = true;
            this.activeGeneration = owner.generation;
            this.canvas.width = owner.width;
            if (!this._owns(owner)) return cancelledConfiguration();
            this.canvas.height = owner.height;
            if (!this._owns(owner)) return cancelledConfiguration();
            this._notify("onConfigured", {
                decoder: owner.id.toString(), generation: owner.generation, codec: prepared.codec,
            });
            return this._owns(owner) ? probe : cancelledConfiguration();
        }

        submitAccessUnit(unit) {
            const owner = this.current;
            if (!owner || !this._owns(owner) || !this.configured || !owner.decoder
                || owner.decoder.state !== "configured" || this.unknownOutput) {
                return { admitted: false, reason: "decoder_not_configured" };
            }
            let chunk;
            try {
                if (integer(unit.generation).toString() !== owner.generation) {
                    return { admitted: false, reason: "stale_codec_generation" };
                }
                if (!(unit.data instanceof Uint8Array)
                    || unit.data.byteLength > this.media.maxAccessUnitBytes()
                    || typeof unit.isKey !== "boolean") {
                    return { admitted: false, reason: "browser_media_bounds_exceeded" };
                }
                // decodeQueueSize is an additional browser preflight only. The
                // Rust owner bounds all admitted inputs and escaped outputs.
                if (owner.decoder.decodeQueueSize >= MAX_DECODE_QUEUE_SIZE) {
                    return { admitted: false, reason: "decode_queue_full" };
                }
                chunk = this.media.prepareChunk(owner.id, integer(unit.frameId),
                    integer(unit.sourceTimestampUs), unit.isKey, unit.data);
            } catch (error) {
                const reason = typeof error === "string" && /^browser_[a-z_]{1,64}$/.test(error)
                    ? error : "browser_hevc_admission_refused";
                return { admitted: false, reason };
            }
            try {
                owner.nativeBusy += 1;
                // The generated callback timestamp is an exact local identity;
                // source/frame timestamps remain in Rust and cannot be rebound.
                const nativeChunk = new global.EncodedVideoChunk({
                    type: chunk.isKey() ? "key" : "delta",
                    timestamp: Number(chunk.timestamp()), data: chunk.takeBytes(),
                });
                if (!this._owns(owner) || !this.configured || this.unknownOutput) {
                    return { admitted: false, reason: "decoder_not_configured" };
                }
                owner.decoder.decode(nativeChunk);
                return { admitted: true };
            } catch (_) {
                this._error(owner);
                return { admitted: false, reason: "decode_submit_failed" };
            } finally {
                owner.nativeBusy -= 1;
                chunk.free();
                if (owner.retired) this._retire(owner);
            }
        }

        _output(owner, frame) {
            let receipt;
            try { receipt = JSON.parse(this.media.output(owner.id, integer(frame.timestamp))); }
            catch (_) {
                // Unknown/duplicate/native post-close output never borrows a
                // replacement charge or becomes a visible frame.
                try { frame.close(); }
                catch (_) { this._quarantine(frame); }
                this._error(owner);
                return;
            }
            const entry = { owner, frame, receipt, closing: false, nativeClosed: false };
            owner.frames.add(entry);
            if (receipt.firstDecoded && this._owns(owner)) {
                this._notify("onFirstFrameDecoded", receipt);
            }
            if (!receipt.canPresent || !this.visible || !this._owns(owner)) {
                this._closeFrame(entry);
                return;
            }
            if (this.pendingFrame) this._closeFrame(this.pendingFrame);
            // A previous frame's close notification may have reset/replaced us.
            if (!this.visible || !this._owns(owner)) {
                this._closeFrame(entry);
                return;
            }
            this.pendingFrame = entry;
            if (!owner.raf) {
                owner.raf = true;
                try { owner.rafHandle = global.requestAnimationFrame(() => this._present(owner)); }
                catch (_) {
                    owner.raf = false;
                    owner.rafHandle = null;
                    this._error(owner);
                }
            }
        }

        _present(owner) {
            owner.raf = false;
            owner.rafHandle = null;
            const entry = this.pendingFrame;
            if (!entry || entry.owner !== owner) return;
            this.pendingFrame = null;
            try {
                if (this.visible && this.ctx && this._owns(owner)) {
                    this.ctx.drawImage(entry.frame, 0, 0, owner.width, owner.height);
                    if (this.visible && this._owns(owner)) {
                        this.media.presented(owner.id, integer(entry.receipt.callback), this.nowMs(), true);
                        this._notify("onFramePresented", entry.receipt);
                    }
                }
            } catch (_) { this._error(owner); }
            finally { this._closeFrame(entry); }
        }

        _closeFrame(entry) {
            if (entry.closing || !entry.owner.frames.has(entry)) return;
            entry.closing = true;
            if (this.pendingFrame === entry) this.pendingFrame = null;
            try {
                if (!entry.nativeClosed) entry.frame.close();
                entry.nativeClosed = true;
            }
            catch (_) {
                entry.closing = false;
                // Retain both native handle and Rust charge. Decoder closure
                // cannot release an escaped output's charge, even after reset.
                this._error(entry.owner, false);
                return;
            }
            if (this.unknownOutput) {
                entry.closing = false;
                return;
            }
            try {
                const receipt = JSON.parse(this.media.frameClosed(
                    entry.owner.id, integer(entry.receipt.callback)));
                entry.owner.frames.delete(entry);
                this._release(receipt);
            } catch (_) {
                entry.closing = false;
                this._quarantine(null);
            }
            this._forget(entry.owner);
        }

        _quarantine(frame) {
            if (frame) {
                // A broken native implementation can produce arbitrarily many
                // unowned handles. Keep bounded diagnostic custody, never a
                // growing Set or a false zero-resource claim. No later media
                // admission/credit is possible; outer owner teardown is needed.
                if (!this.quarantineFrame) this.quarantineFrame = frame;
                this.unclosedUnownedOutputs = Math.min(65535, this.unclosedUnownedOutputs + 1);
            }
            if (this.unknownOutput) return;
            this.unknownOutput = true;
            this.configurationOwner = null;
            this.current = null;
            this.decoder = null;
            this.configured = false;
            try { this.media.quarantine(); } catch (_) {}
            for (const owner of Array.from(this.owners)) this._retire(owner);
            this._notify("onDecoderReclaimed", "browser_media_cleanup_incomplete");
        }

        _error(owner, closeFrames = true) {
            const current = this._owns(owner);
            if (current) {
                this.current = null;
                this.decoder = null;
                this.configured = false;
                try { this.media.reclaim(owner.id); } catch (_) {}
            }
            this._retire(owner, closeFrames);
            if (current) this._notify("onDecoderReclaimed", "browser_hevc_decoder_failed");
        }

        _retire(owner, closeFrames = true) {
            owner.retired = true;
            if (this.current === owner) {
                this.current = null;
                this.decoder = null;
                this.configured = false;
            }
            try { this.media.reclaim(owner.id); } catch (_) {}
            if (owner.closing) return;
            owner.closing = true;
            if (owner.raf) {
                owner.raf = false;
                const handle = owner.rafHandle;
                owner.rafHandle = null;
                try { global.cancelAnimationFrame(handle); }
                catch (_) { this._quarantine(null); }
            }
            if (closeFrames) {
                for (const entry of Array.from(owner.frames)) this._closeFrame(entry);
            }
            // isConfigSupported cannot be cancelled. Its metadata/native work
            // retains the original Rust decoder slot until the promise settles;
            // repeated cancelled probes cannot accumulate outside the bound.
            if (!owner.nativeClosed && !owner.probing && owner.nativeBusy === 0) {
                try {
                    if (owner.decoder && (owner.decoder.state !== "closed" || owner.closeFailed)) {
                        owner.decoder.close();
                    }
                    owner.nativeClosed = true;
                } catch (_) { owner.closeFailed = true; }
            }
            if (!this.unknownOutput && owner.nativeClosed && !owner.reconciled) {
                try {
                    const releases = JSON.parse(this.media.decoderClosed(owner.id));
                    owner.reconciled = true;
                    for (const receipt of releases) this._release(receipt);
                } catch (_) {
                    // Native close and Rust reconciliation are separate proof
                    // steps. Keep the original owner and retry acknowledgment;
                    // a binding failure cannot make it disappear from cleanup.
                }
            }
            owner.closing = false;
            this._forget(owner);
        }

        _forget(owner) {
            if (owner.nativeClosed && owner.reconciled && owner.frames.size === 0) this.owners.delete(owner);
        }

        reset() {
            this.configurationOwner = null;
            this.current = null;
            this.decoder = null;
            this.configured = false;
            if (this.media) this.media.retire();
            for (const owner of Array.from(this.owners)) this._retire(owner);
            return this.owners.size === 0 && !this.unknownOutput;
        }

        snapshot() {
            const snapshot = JSON.parse(this.media.snapshot());
            if (this.unknownOutput) {
                snapshot.quarantined = true;
                snapshot.activeDecoder = null;
                snapshot.stage = "quarantined";
            }
            return Object.assign(snapshot, {
                unclosedUnownedOutputs: this.unclosedUnownedOutputs,
                unownedOutputCountSaturated: this.unclosedUnownedOutputs === 65535,
            });
        }
    }

    global.FrankenHevc = {
        probeHevcWebCodecs: probeHevcWebCodecs,
        WebCodecsHevcPlayer: WebCodecsHevcPlayer,
    };
})(window);
