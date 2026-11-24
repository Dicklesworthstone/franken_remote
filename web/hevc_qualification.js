"use strict";
// Local, public HEVC qualification through the production Rust media owner.
// No remote session, observation, input authority or receiver credit is created.
(function (global) {
    const MAX_DURATION_MS = 12000;
    const FRAME_TIMEOUT_MS = 2000;

    const ERROR_CODES = new Set([
        "browser_https_required", "browser_client_unavailable",
        "browser_client_version_mismatch", "browser_connection_cancelled",
        "webcodecs_unavailable", "no_supported_hevc_decoder", "hevc_probe_failed",
        "hevc_decoder_configuration_failed", "browser_probe_hidden",
        "browser_probe_clock_invalid", "browser_probe_timeout",
        "browser_probe_frame_timeout", "browser_probe_canvas_unavailable",
        "browser_probe_blank_canvas", "browser_probe_pixels_unchanged",
        "browser_probe_replay_mismatch", "browser_probe_decode_failed",
        "browser_probe_sample_invalid", "browser_probe_cleanup_incomplete",
    ]);
    function failure(code) { return Object.assign(new Error(code), { code }); }
    function publicCode(error, fallback) {
        return error && ERROR_CODES.has(error.code) ? error.code : fallback;
    }

    class Probe {
        constructor(canvas, onState) {
            this.canvas = canvas;
            this.onState = onState;
            this.run = null;
        }

        get busy() { return this.run !== null; }

        start() {
            if (this.run && this.run.ending && this.run.workDone) this._cleanup(this.run);
            if (this.run) return Promise.resolve({ status: "failed", code: "browser_probe_busy" });
            if (document.hidden) return this._unstarted("cancelled", "browser_probe_hidden");
            const started = global.performance.now();
            if (!Number.isFinite(started) || started < 0) {
                return this._unstarted("failed", "browser_probe_clock_invalid");
            }
            const run = {
                abort: new global.AbortController(), started, last: started,
                deadline: started + MAX_DURATION_MS, timer: null, wait: null,
                media: null, player: null, workDone: false, ending: false,
                result: null, generation: 0, submitted: 0, presented: 0, released: 0,
                frames: new Map(), stage: "loading",
            };
            this.run = run;
            const completion = new Promise((resolve) => { run.complete = resolve; });
            run.timer = global.setTimeout(() => {
                this._finish(run, "failed", "browser_probe_timeout");
            }, MAX_DURATION_MS);
            this._notify(run);
            this._execute(run).then(() => {
                run.workDone = true;
                if (run.ending) this._cleanup(run);
                else this._finish(run, "passed", "browser_probe_passed");
            }, (error) => {
                run.workDone = true;
                if (run.ending) this._cleanup(run);
                else {
                    const code = publicCode(error, "browser_probe_decode_failed");
                    this._finish(run, code === "browser_probe_hidden" ? "cancelled" : "failed", code);
                }
            });
            return completion;
        }

        cancel() {
            if (!this.run) return;
            if (this.run.ending) this._cleanup(this.run);
            else this._finish(this.run, "cancelled", "browser_probe_cancelled");
        }

        _unstarted(status, code) {
            const result = { status, code, submitted: 0, presented: 0, released: 0,
                busy: false, cleanupComplete: true, width: 320, height: 240 };
            try { if (this.onState) this.onState(result); } catch (_) {}
            return Promise.resolve(result);
        }

        _check(run) {
            if (this.run !== run || run.ending) throw failure("browser_connection_cancelled");
            const now = global.performance.now();
            if (!Number.isFinite(now) || now < run.last) throw failure("browser_probe_clock_invalid");
            run.last = now;
            if (now >= run.deadline) throw failure("browser_probe_timeout");
            if (document.hidden) throw failure("browser_probe_hidden");
            if (!this.canvas.isConnected || this.canvas.getBoundingClientRect().width <= 0
                || this.canvas.getBoundingClientRect().height <= 0) {
                throw failure("browser_probe_canvas_unavailable");
            }
            return now;
        }

        async _execute(run) {
            this._check(run);
            run.media = await global.FrankenRust.createMediaAdapter(run.abort.signal, {
                frames: 4, bytes: 2097152n,
            });
            this._check(run);
            run.player = new global.FrankenHevc.WebCodecsHevcPlayer(this.canvas, {
                onFramePresented: (receipt) => this._presented(run, receipt),
                onFrameClosed: (receipt) => this._released(run, receipt),
                onDecoderReclaimed: () => this._finish(run, "failed", "browser_probe_decode_failed"),
            }, run.media);
            if (!run.player.ctx) throw failure("browser_probe_canvas_unavailable");

            await this._configure(run);
            const first = await this._pictures(run, [0], "single_idr");
            await this._idle(run, 1000);
            const continuation = await this._pictures(run, [1], "after_idle");
            if (continuation === first) throw failure("browser_probe_pixels_unchanged");
            await this._pictures(run, [2, 3, 4], "burst");
            await this._idle(run, 250);
            await this._pictures(run, [5], "last_picture");

            await this._configure(run);
            if (await this._pictures(run, [0], "reset_idr") !== first
                || await this._pictures(run, [1], "reset_continuation") !== continuation) {
                throw failure("browser_probe_replay_mismatch");
            }
            this._check(run);
            const snapshot = run.player.snapshot();
            if (snapshot.inflightFrames !== 0 || snapshot.inflightBytes !== 0
                || run.released !== run.submitted) {
                throw failure("browser_probe_cleanup_incomplete");
            }
        }

        async _configure(run) {
            this._check(run);
            run.generation += 1;
            run.stage = "configuration";
            this._notify(run);
            const result = await run.player.configure(
                global.FrankenHevcProbeSample.parameters(String(run.generation))
            );
            this._check(run);
            if (!result.supported) throw failure(result.code);
        }

        _pictures(run, indices, stage) {
            this._check(run);
            run.stage = stage;
            this._notify(run);
            return new Promise((resolve, reject) => {
                const target = run.submitted + indices.length;
                run.wait = { resolve, reject, target, timer: global.setTimeout(() => {
                    this._rejectWait(run, failure("browser_probe_frame_timeout"));
                }, FRAME_TIMEOUT_MS) };
                try {
                    for (const index of indices) {
                        const now = this._check(run);
                        const id = ++run.submitted;
                        run.frames.set(String(id), { presented: false, closed: false, hash: null });
                        const admitted = run.player.submitAccessUnit({
                            generation: String(run.generation), frameId: String(id),
                            sourceTimestampUs: String(Math.floor(now * 1000)),
                            isKey: index === 0, data: global.FrankenHevcProbeSample.picture(index),
                        });
                        if (!admitted.admitted) throw failure("browser_probe_sample_invalid");
                    }
                } catch (error) { this._rejectWait(run, error); }
            });
        }

        _idle(run, duration) {
            this._check(run);
            run.stage = "idle";
            this._notify(run);
            return new Promise((resolve, reject) => {
                run.wait = { resolve, reject, target: null, timer: global.setTimeout(() => {
                    try {
                        this._check(run);
                        run.wait = null;
                        resolve();
                    } catch (error) { this._rejectWait(run, error); }
                }, duration) };
            });
        }

        _presented(run, receipt) {
            if (this.run !== run || run.ending) return;
            try {
                this._check(run);
                const frame = run.frames.get(receipt.frameId);
                if (!frame || frame.presented || frame.closed
                    || receipt.generation !== String(run.generation)) {
                    throw failure("browser_probe_decode_failed");
                }
                const pixels = run.player.ctx.getImageData(0, 0, 320, 240).data;
                if (pixels.length !== 320 * 240 * 4) throw failure("browser_probe_canvas_unavailable");
                let low = 255, high = 0, hash = 2166136261;
                for (let index = 0; index < pixels.length; index += 4) {
                    const luma = (pixels[index] + 2 * pixels[index + 1] + pixels[index + 2]) >> 2;
                    low = Math.min(low, luma);
                    high = Math.max(high, luma);
                    if (pixels[index + 3] !== 255) throw failure("browser_probe_blank_canvas");
                    for (let channel = 0; channel < 3; channel++) {
                        hash = Math.imul(hash ^ pixels[index + channel], 16777619);
                    }
                }
                if (high - low < 32) throw failure("browser_probe_blank_canvas");
                this._check(run);
                frame.hash = hash >>> 0;
                frame.presented = true;
                run.presented += 1;
                this._settlePicture(run);
            } catch (error) {
                const code = publicCode(error, "browser_probe_canvas_unavailable");
                this._finish(run, code === "browser_probe_hidden" ? "cancelled" : "failed", code);
            }
        }

        _released(run, receipt) {
            if (this.run !== run || run.ending) return;
            const frame = run.frames.get(receipt.frameId);
            if (!frame || frame.closed) {
                this._finish(run, "failed", "browser_probe_decode_failed");
                return;
            }
            frame.closed = true;
            run.released += 1;
            this._settlePicture(run);
        }

        _settlePicture(run) {
            const waiting = run.wait;
            if (!waiting || waiting.target === null) return;
            const frame = run.frames.get(String(waiting.target));
            if (frame && frame.presented && frame.closed) {
                run.wait = null;
                global.clearTimeout(waiting.timer);
                waiting.resolve(frame.hash);
            }
        }

        _rejectWait(run, error) {
            const waiting = run.wait;
            if (!waiting) return;
            run.wait = null;
            global.clearTimeout(waiting.timer);
            waiting.reject(error);
        }

        _finish(run, status, code) {
            if (this.run !== run || run.ending) return;
            // Fence callbacks before cancellation or native close can reenter.
            run.ending = true;
            run.result = { status, code };
            run.abort.abort();
            global.clearTimeout(run.timer);
            this._rejectWait(run, failure("browser_connection_cancelled"));
            try { if (run.player) run.player.reset(); } catch (_) {}
            if (run.workDone) this._cleanup(run);
            else this._completed(run);
        }

        _completed(run) {
            this._notify(run);
            if (run.complete) {
                run.complete(this._snapshot(run));
                run.complete = null;
            }
        }

        _cleanup(run) {
            if (this.run !== run || !run.ending || !run.workDone) return;
            let clean = false;
            try {
                clean = !run.player || run.player.reset();
                if (clean && run.media) run.media.free();
            } catch (_) { clean = false; }
            // A synchronous browser close can consume the remaining budget
            // before JavaScript gets another chance to service its watchdog.
            if (clean && run.result.status === "passed") {
                const now = global.performance.now();
                if (!Number.isFinite(now) || now < run.last) {
                    run.result = { status: "failed", code: "browser_probe_clock_invalid" };
                } else if (now >= run.deadline) {
                    run.result = { status: "failed", code: "browser_probe_timeout" };
                } else if (document.hidden) {
                    run.result = { status: "cancelled", code: "browser_probe_hidden" };
                }
            }
            if (clean) this.run = null;
            else if (run.result.status === "passed") {
                run.result = { status: "failed", code: "browser_probe_cleanup_incomplete" };
            }
            this._completed(run);
        }

        _snapshot(run) {
            return Object.assign({
                status: "running", code: null, stage: run.stage,
                submitted: run.submitted, presented: run.presented, released: run.released,
                busy: this.run === run, cleanupComplete: this.run !== run,
                width: 320, height: 240,
            }, run.result || {});
        }

        _notify(run) {
            try { if (this.onState) this.onState(this._snapshot(run)); } catch (_) {}
        }
    }
    global.FrankenHevcProbe = Object.freeze({ Probe });
})(window);
