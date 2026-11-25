"use strict";
// A local WebCodecs/Web Audio test. Only the built-in sample can reach the
// graph. This owner has no session, transport, or microphone interface.
(function (global) {
    const RATE = 48000;
    const FRAMES = 960;
    const PACKET_US = 20000;
    const LIMIT_MS = 3000;

    // Six raw mono Opus packets, encoded in order from a generated quiet
    // 440 Hz tone (libopus, 48 kHz, 20 ms, peak PCM16 amplitude 2400).
    const packets = [
        "+LTR7rFBpf9bB3hoza/fN6arCuf7+7I2crBvl5TNBCZh8L0nTwrtABeyOT/HWnlD1+D4CvYuRfl9lxSZDK8+4afdeeHTntQ83XimFK4lF1kIm/8xUO1IY0T7a8Pwp+fmHSm+w0R7hpFFUJ2MtknuNIbf+daElROoRk8LJZwSZOCrrWFnebCVJNDSeknIOtHzGNi3zKFFcHK2Y94KnhTrIUjafPbbFq2kSFSvrWOrzGaTW2NT0rxhEjw3cZNeX0UYdye0DbKDiHc8S60=",
        "+LFyml7VEAfn7o1cET37WaAWMV37/4GRhbQ0LFAXvKRj+m30WMvflEE/pdqZZOhXRt+k2oLtEOLkxXbT663MIsXrbR196rWC7q5/HETorrLD0VmOTusPqUjJwn2UbFm/yOG2XPOlE8dzPK/bJEDyKHeu",
        "+LBDWhdH9hB0zgYSLYeMerAL9eablE+cmu/fUNzaD+CCxJenJfA95wVMCYV8JdtNRCKyvkGfsqEaROvs4bGgWJF4pInzTIqzpLEnV4y1+U5jumLF6ej3EXV8GiLDVsGfknTggHrYlgrJg8+6c+fvX273OGM9fRw5IV3leZi2z5HMk64=",
        "+LBFfVn67sestRT1gXcFUler3xL3UqCq0tlp34vyWiCYmRZmQE53VaL5hFs9oclFcTOBIZlIXbeSjlfBCBRxSUdpqm8mRSJjQeH2iYHvDsqPvZwxEtTWainGnnhqG2b8EdQKbx2wQs5dZW6R8isylolhhv2c0O3FbMrZxfgIfzVX6fuu",
        "+K/pIEhgPiGMGN0d6b4taC6VXh3Pc1VBlr7mtDqpJ4kLGvkmhhUE6Ksu3j4kVVLHcUZENdbNGA6VNDnnUDVmiDar6i7zBKB+GJU1ERYQFL4HqdklqbXrSFLhFlopUrVjX4fwS1I5sPVcFgq5ciTXO5Les3BXNNDb8m64/O2hWdW4U0h/SM2nrg==",
        "+LQs97iQNWSk7tk76nFFIL3YZ6kHsfDtI0zPqgRH6/Wnefurxw1Xp6G0BuOtGWANPYUTEIgeFweC7+p8OQgnBwXxTERe/Q0HlnYhj7FgozJ9CI3fpAv6CgkpiR1kFpPmBYobVY3UZNETUu7DGtPRNNUUazaSykAN7zfhuAhXwqHTjDhotE5m7qWUNbKqHBPgdsWCMoMSqPFSNvatlwqwbEPhJj2tocNi5oaFefxpMyfJWvM3yaw=",
    ];

    function matchesTone(samples) {
        if (!samples.every((v) => Number.isFinite(v) && Math.abs(v) <= 0.15)) return false;
        // Measure only the steady interior, allowing the decoder's phase delay.
        let sine = 0, cosine = 0, energy = 0;
        const count = 3 * FRAMES;
        for (let i = 0; i < count; i++) {
            const phase = 2 * Math.PI * 440 * i / RATE;
            const value = samples[FRAMES + i];
            sine += value * Math.sin(phase);
            cosine += value * Math.cos(phase);
            energy += value * value;
        }
        return energy / count > 0.0002 &&
            2 * (sine * sine + cosine * cosine) > 0.75 * count * energy;
    }

    class Probe {
        constructor(report) {
            this.report = report;
            this.run = null;
        }
        get busy() { return this.run !== null; }

        start() {
            if (this.busy) return;
            if (!global.isSecureContext || typeof global.AudioDecoder !== "function" ||
                typeof global.EncodedAudioChunk !== "function" || typeof global.AudioContext !== "function") {
                this.report("unavailable");
                return;
            }
            const run = {
                decoder: null, context: null, source: null, received: 0,
                deadline: global.performance.now() + LIMIT_MS, result: null, closing: false,
            };
            this.run = run;
            run.timer = global.setTimeout(() => this.finish(run, "timeout"), LIMIT_MS);
            this.report("running");
            try {
                // Resume inside the initiating click, before any await.
                run.context = new global.AudioContext();
                run.context.resume().then(() => this.decode(run)).catch(() => this.finish(run, "failed"));
            } catch {
                this.finish(run, "failed");
            }
        }

        live(run) {
            if (this.run !== run || run.result) return false;
            if (global.performance.now() >= run.deadline) {
                this.finish(run, "timeout");
                return false;
            }
            return true;
        }

        async decode(run) {
            if (!this.live(run)) return;
            if (run.context.state !== "running") return this.finish(run, "failed");
            const buffer = run.context.createBuffer(1, FRAMES * packets.length, RATE);
            const samples = buffer.getChannelData(0);
            run.decoder = new global.AudioDecoder({
                error: () => this.finish(run, "failed"),
                output: (frame) => {
                    try {
                        if (!this.live(run)) return;
                        if (run.received >= packets.length || frame.timestamp !== run.received * PACKET_US ||
                            frame.numberOfChannels !== 1 || frame.numberOfFrames !== FRAMES ||
                            frame.sampleRate !== RATE) return this.finish(run, "invalid_output");
                        const offset = FRAMES * run.received;
                        frame.copyTo(samples.subarray(offset, offset + FRAMES), { planeIndex: 0, format: "f32-planar" });
                        run.received++;
                    } catch {
                        this.finish(run, "invalid_output");
                    } finally {
                        frame.close();
                    }
                },
            });
            run.decoder.configure({ codec: "opus", sampleRate: RATE, numberOfChannels: 1 });
            for (const [i, packet] of packets.entries()) {
                run.decoder.decode(new global.EncodedAudioChunk({
                    type: "key", timestamp: i * PACKET_US, duration: PACKET_US,
                    data: Uint8Array.from(global.atob(packet), (c) => c.charCodeAt(0)),
                }));
            }
            await run.decoder.flush();
            if (!this.live(run)) return;
            // Reject silence, non-finite samples and unexpectedly loud output
            // before allowing this fixed, low-amplitude tone to reach the graph.
            if (run.received !== packets.length || !matchesTone(samples)) {
                return this.finish(run, "invalid_output");
            }
            run.decoder.close();
            run.source = run.context.createBufferSource();
            run.source.buffer = buffer;
            run.source.connect(run.context.destination);
            run.source.onended = () => {
                if (this.live(run)) this.finish(run, run.context.state === "running" ? "passed" : "failed");
            };
            run.source.start();
        }

        cancel() {
            if (this.run) this.finish(this.run, "cancelled");
        }
        async finish(run, result) {
            if (this.run !== run || run.closing) return;
            run.result ||= result;
            run.closing = true;
            global.clearTimeout(run.timer);
            // Retain the owner until the original graph confirms closure. A
            // stale WebCodecs callback can release a frame, never start audio.
            this.report("closing");
            try {
                if (run.decoder && run.decoder.state !== "closed") run.decoder.close();
                if (run.source) {
                    run.source.onended = null;
                    run.source.disconnect();
                    run.source = null;
                }
                if (run.context && run.context.state !== "closed") await run.context.close();
                this.run = null;
                this.report(run.result);
            } catch {
                run.closing = false;
                this.report("cleanup_failed");
            }
        }
    }
    global.FrankenOpusProbe = Probe;
})(window);
