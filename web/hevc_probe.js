"use strict";
// FrankenRemote WebCodecs HEVC capability probe and bounded decoder pipeline.
//
// Constitutional invariants (AGENTS.md §§3.3, 5; Plan §§11, 16.3):
// - HEVC only (Main, 8-bit, 4:2:0 baseline). Never silently fall back to H.264/VP9.
// - Never hard-code an hvc1 codec string for playback; always configure from the
//   exact negotiated hvcC description and SPS-derived codec identifier supplied
//   by the Rust HevcGuard / host DecoderConfiguration record.
// - Zero per-frame flush(): flush stalls hardware decoders and destroys freshness.
// - Strict bounded decode queue and renderer retention: VideoFrame objects are
//   closed immediately after presentation or replacement, and receive credit is
//   returned ONLY after VideoFrame.close() completes.

(function (global) {
    const MAX_DECODE_QUEUE_SIZE = 4;

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

    function closeDecoder(decoder) {
        if (decoder) {
            try {
                if (decoder.state !== "closed") decoder.close();
            } catch (_) {
                // Teardown cannot turn a typed configuration refusal into a rejection.
            }
        }
    }

    async function probeHevcWebCodecs(codecString, hvccBytes, width, height) {
        if (typeof global.VideoDecoder !== "function" || typeof global.EncodedVideoChunk !== "function") {
            return {
                supported: false,
                code: "webcodecs_unavailable",
                reason: "Browser does not expose WebCodecs VideoDecoder; HEVC playback cannot proceed without fallback.",
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

    class WebCodecsHevcPlayer {
        constructor(canvas, callbacks) {
            this.canvas = canvas;
            this.ctx = canvas.getContext("2d", { alpha: false, desynchronized: true });
            this.callbacks = callbacks || {};
            this.decoder = null;
            this.configured = false;
            this.awaitingKeyFrame = true;
            this.visible = !document.hidden;
            this.pendingFrame = null;
            this.rafScheduled = false;
            this.inFlightByTimestamp = new Map();
            this.decodedCount = 0;
            this.presentedCount = 0;
            this.closedFrames = 0;
            this.closedBytes = 0;
            this.activeGeneration = 0;
            this.configurationOwner = null;
        }

        setVisible(isVisible) {
            this.visible = Boolean(isVisible);
            if (!this.visible && this.pendingFrame) {
                this._closePendingFrame();
            }
        }

        async configure(params) {
            // A wire generation can be retried: only this local object identifies
            // one probe/configure operation, including same-generation recovery.
            const owner = {};
            this.configurationOwner = owner;
            let requested;
            try {
                requested = {
                    generation: params.generation >>> 0,
                    codec: String(params.codec),
                    codedWidth: Number(params.codedWidth),
                    codedHeight: Number(params.codedHeight),
                    hvcc: params.hvcc,
                };
            } catch (_) {
                return this._refuseConfiguration(owner, failedConfiguration());
            }
            const probe = await probeHevcWebCodecs(
                requested.codec,
                requested.hvcc,
                requested.codedWidth,
                requested.codedHeight
            );
            if (this.configurationOwner !== owner) return cancelledConfiguration();
            if (!probe.supported) {
                return this._refuseConfiguration(owner, probe);
            }

            this._resetDecoder();
            // Closing a pending frame can invoke application callbacks that reset
            // the player or start a replacement configuration synchronously.
            if (this.configurationOwner !== owner) return cancelledConfiguration();
            let decoder = null;
            try {
                decoder = new global.VideoDecoder({
                    output: (frame) => this._onDecodedFrame(frame),
                    error: (err) => this._onDecoderError(err),
                });
                decoder.configure(probe.config);
            } catch (_) {
                closeDecoder(decoder);
                return this._refuseConfiguration(owner, failedConfiguration());
            }
            if (this.configurationOwner !== owner) {
                closeDecoder(decoder);
                return cancelledConfiguration();
            }

            this.decoder = decoder;
            this.activeGeneration = requested.generation;
            this.canvas.width = requested.codedWidth;
            this.canvas.height = requested.codedHeight;
            this.configured = true;
            this.awaitingKeyFrame = true;

            if (typeof this.callbacks.onConfigured === "function") {
                this.callbacks.onConfigured({
                    generation: this.activeGeneration,
                    codec: requested.codec,
                });
            }
            return probe;
        }

        _refuseConfiguration(owner, refusal) {
            if (this.configurationOwner !== owner) return cancelledConfiguration();
            this._resetDecoder();
            // A replacement started by a cleanup callback also owns the result:
            // an old refusal must not make the application disconnect it.
            if (this.configurationOwner !== owner) return cancelledConfiguration();
            this.configurationOwner = null;
            return refusal;
        }

        submitAccessUnit(unit) {
            if (!this.configured || !this.decoder || this.decoder.state !== "configured") {
                return { admitted: false, reason: "decoder_not_configured" };
            }
            if ((unit.generation >>> 0) !== this.activeGeneration) {
                return { admitted: false, reason: "stale_codec_generation" };
            }
            if (this.awaitingKeyFrame && !unit.isKey) {
                // Never feed broken dependencies or delta frames before an IDR.
                return { admitted: false, reason: "awaiting_idr" };
            }
            if (this.decoder.decodeQueueSize >= MAX_DECODE_QUEUE_SIZE) {
                // Bounded decode queue: refuse over-submission without corrupting reference chain.
                return { admitted: false, reason: "decode_queue_full" };
            }

            const timestampUs = unit.timestampUs;
            this.inFlightByTimestamp.set(timestampUs, {
                frameId: unit.frameId,
                chargedBytes: unit.chargedBytes,
                sourceTimestampUs: unit.sourceTimestampUs,
            });

            const chunk = new global.EncodedVideoChunk({
                type: unit.isKey ? "key" : "delta",
                timestamp: timestampUs,
                data: unit.data,
            });
            try {
                // Deliberately NO decoder.flush(): flush stalls hardware pipelines.
                this.decoder.decode(chunk);
                if (unit.isKey) {
                    this.awaitingKeyFrame = false;
                }
                return { admitted: true };
            } catch (err) {
                this.inFlightByTimestamp.delete(timestampUs);
                this._onDecoderError(err);
                return { admitted: false, reason: "decode_submit_failed" };
            }
        }

        _onDecodedFrame(videoFrame) {
            const meta = this.inFlightByTimestamp.get(videoFrame.timestamp) || {
                frameId: 0,
                chargedBytes: 0,
                sourceTimestampUs: videoFrame.timestamp,
            };
            this.inFlightByTimestamp.delete(videoFrame.timestamp);
            this.decodedCount += 1;

            if (this.decodedCount === 1 && typeof this.callbacks.onFirstFrameDecoded === "function") {
                this.callbacks.onFirstFrameDecoded({
                    generation: this.activeGeneration,
                    frameId: meta.frameId,
                });
            }

            // Hidden/background tabs MUST NOT present frames or claim fresh presentation,
            // and MUST close decoded VideoFrames promptly to release GPU surfaces and credit.
            if (!this.visible) {
                this._closeAndRelease(videoFrame, meta, false);
                return;
            }

            // Newest ready frame wins: replace any unpresented frame and close it immediately.
            if (this.pendingFrame) {
                this._closePendingFrame();
            }
            this.pendingFrame = { frame: videoFrame, meta: meta };

            if (!this.rafScheduled) {
                this.rafScheduled = true;
                global.requestAnimationFrame(() => this._presentLatest());
            }
        }

        _presentLatest() {
            this.rafScheduled = false;
            const entry = this.pendingFrame;
            this.pendingFrame = null;
            if (!entry) {
                return;
            }

            let presented = false;
            if (this.visible && this.ctx) {
                this.ctx.drawImage(entry.frame, 0, 0, this.canvas.width, this.canvas.height);
                this.presentedCount += 1;
                presented = true;
            }
            this._closeAndRelease(entry.frame, entry.meta, presented);
        }

        _closePendingFrame() {
            const entry = this.pendingFrame;
            this.pendingFrame = null;
            if (entry) {
                this._closeAndRelease(entry.frame, entry.meta, false);
            }
        }

        _closeAndRelease(videoFrame, meta, presented) {
            try {
                videoFrame.close();
            } catch (_) {
                // Ensure credit accounting only runs after explicit close attempt.
            }
            this.closedFrames += 1;
            this.closedBytes += meta.chargedBytes >>> 0;

            if (presented && typeof this.callbacks.onFramePresented === "function") {
                this.callbacks.onFramePresented({
                    generation: this.activeGeneration,
                    frameId: meta.frameId,
                    sourceTimestampUs: meta.sourceTimestampUs,
                    presentedCount: this.presentedCount,
                });
            }
            // Credit is returned ONLY AFTER VideoFrame.close() frees receive/GPU budget.
            if (meta.chargedBytes > 0 && typeof this.callbacks.onFrameClosed === "function") {
                this.callbacks.onFrameClosed({
                    releasedBytes: this.closedBytes,
                    releasedFrames: this.closedFrames,
                    decodeQueueSize: this.decoder ? this.decoder.decodeQueueSize : 0,
                });
            }
        }

        _onDecoderError(err) {
            this.awaitingKeyFrame = true;
            this.configured = false;
            this._closePendingFrame();
            if (typeof this.callbacks.onDecoderReclaimed === "function") {
                this.callbacks.onDecoderReclaimed(err ? String(err) : "decoder_error");
            }
        }

        reset() {
            // Invalidate the async owner before cleanup can invoke any callback.
            this.configurationOwner = null;
            this._resetDecoder();
        }

        _resetDecoder() {
            // Detach original resources before callbacks can install a successor.
            const decoder = this.decoder;
            const pending = this.pendingFrame;
            this.decoder = null;
            this.pendingFrame = null;
            this.inFlightByTimestamp.clear();
            this.configured = false;
            this.awaitingKeyFrame = true;
            this.decodedCount = 0;
            this.presentedCount = 0;
            if (pending) this._closeAndRelease(pending.frame, pending.meta, false);
            closeDecoder(decoder);
        }
    }

    global.FrankenHevc = {
        probeHevcWebCodecs: probeHevcWebCodecs,
        WebCodecsHevcPlayer: WebCodecsHevcPlayer,
    };
})(window);
