// Synthetic-media qualification only; no host capture, transport or authority.
async function probe() {
  const fixture = await (await fetch('/fixture.json', {signal: AbortSignal.timeout(3000)})).json();
  const bytes = value => Uint8Array.from(atob(value), c => c.charCodeAt(0));
  const report = {
    userAgent: navigator.userAgent, secureContext: isSecureContext,
    visibility: document.visibilityState, screen: [screen.width, screen.height],
    devicePixelRatio,
    geometry: [fixture.width, fixture.height], codec: fixture.codec,
    hardwareAcceleration: 'not tested (preference is not proof)',
    scope: 'synthetic HEVC decode and canvas readback',
    physicalPresentation: 'not tested', frames: [], status: 'failed',
    nativeDecoderSurfaceBounds: 'not tested (decodeQueueSize excludes native surfaces)',
  };
  const canvas = document.createElement('canvas');
  canvas.width = fixture.width; canvas.height = fixture.height;
  document.body.append(canvas);
  const context = canvas.getContext('2d', {willReadFrequently: true});
  const config = {
    codec: fixture.codec, description: bytes(fixture.description),
    codedWidth: fixture.width, codedHeight: fixture.height,
    optimizeForLatency: true, hardwareAcceleration: 'prefer-hardware',
  };
  let decoder;
  let failure;
  let retained = 0;
  let maxRetained = 0;
  let maxQueue = 0;
  const waiting = new Map();
  const delay = ms => new Promise(resolve => setTimeout(resolve, ms));
  try {
    report.configSupported = (await VideoDecoder.isConfigSupported(config)).supported;
    if (!report.configSupported) throw new Error('configuration_unsupported');
    decoder = new VideoDecoder({
      error(error) { failure = error.name; },
      output(frame) {
        retained++; maxRetained = Math.max(maxRetained, retained);
        try {
          const row = waiting.get(frame.timestamp);
          if (!row || retained > 1) throw new Error('unexpected_output');
          if (frame.displayWidth !== fixture.width || frame.displayHeight !== fixture.height)
            throw new Error('geometry_mismatch');
          if (frame.allocationSize({format: 'RGBA'}) > 4096 * 2160 * 4)
            throw new Error('decoded_byte_bound');
          row.decodedMs = performance.now() - row.started;
          context.drawImage(frame, 0, 0);
          // Readback proves canvas pixels were populated, not optical display.
          const pixels = context.getImageData(0, 0, 64, 64).data;
          let low = 255, high = 0;
          let hash = 2166136261;
          for (let i = 0; i < pixels.length; i++) {
            const value = pixels[i];
            hash = Math.imul(hash ^ value, 16777619);
            if (i % 4 !== 3) { low = Math.min(low, value); high = Math.max(high, value); }
          }
          if (high - low < 32) throw new Error('blank_readback');
          row.canvasHash = (hash >>> 0).toString(16);
          row.drawnMs = performance.now() - row.started;
          row.done = true;
        } catch (error) { failure = error.message; }
        finally { frame.close(); retained--; }
      },
    });
    decoder.configure(config);
    async function submit(index, stage, timestamp) {
      if (failure) throw new Error(failure);
      if (waiting.size >= 4 || decoder.decodeQueueSize >= 4) throw new Error('queue_bound');
      const sample = fixture.samples[index];
      const data = bytes(sample.data);
      if (data.byteLength > 4 * 1024 * 1024) throw new Error('sample_bound');
      const row = {stage, timestamp, started: performance.now()};
      waiting.set(timestamp, row);
      decoder.decode(new EncodedVideoChunk({type: sample.key ? 'key' : 'delta', timestamp, data}));
      maxQueue = Math.max(maxQueue, decoder.decodeQueueSize);
      return row;
    }
    async function collect(rows) {
      const until = performance.now() + 2000;
      while (rows.some(row => !row.done) && !failure && performance.now() < until) await delay(5);
      if (failure) throw new Error(failure);
      if (rows.some(row => !row.done)) throw new Error('output_timeout_without_flush');
      for (const row of rows) {
        waiting.delete(row.timestamp);
        const {started, done, ...result} = row;
        report.frames.push(result);
      }
    }
    await collect([await submit(0, 'single_idr', 0)]);
    await delay(2000);
    await collect([await submit(1, 'p_after_idle', 2000000)]);
    const burst = [];
    for (let index = 2; index < 6; index++) burst.push(await submit(index, 'burst', 2000000 + index * 33333));
    await collect(burst);
    await delay(500);
    await collect([await submit(6, 'low_cadence_p', 3000000)]);
    // Drop the remaining old chain before submitting anything dependent on it.
    decoder.reset(); decoder.configure(config);
    report.deltaAfterResetRefused = false;
    try {
      decoder.decode(new EncodedVideoChunk({
        type: 'delta', timestamp: 3500000, data: bytes(fixture.samples[1].data),
      }));
    } catch (error) {
      if (error.name !== 'DataError') throw error;
      report.deltaAfterResetRefused = true;
    }
    if (!report.deltaAfterResetRefused) throw new Error('delta_accepted_after_reset');
    await collect([await submit(0, 'reset_recovery_idr', 4000000)]);
    await collect([await submit(1, 'recovery_p', 4033333)]);
    if (report.frames[0].canvasHash === report.frames[1].canvasHash ||
        report.frames[0].canvasHash !== report.frames[7].canvasHash ||
        report.frames[1].canvasHash !== report.frames[8].canvasHash)
      throw new Error('stale_or_inconsistent_readback');
    report.status = 'passed';
  } catch (error) { report.reason = error.message; }
  finally {
    if (decoder && decoder.state !== 'closed') decoder.close();
    waiting.clear();
    report.maxDecodeQueue = maxQueue;
    report.maxRetainedFrames = maxRetained;
    report.retainedAtEnd = retained;
    report.flushCalls = 0;
  }
  return report;
}
async function main() {
  let report;
  try { report = await probe(); }
  catch (error) { report = {status: 'failed', reason: error.name, scope: 'probe setup'}; }
  try {
    const response = await fetch('/result', {
      method: 'POST', headers: {'Content-Type': 'application/json'},
      body: JSON.stringify(report), signal: AbortSignal.timeout(3000),
    });
    if (!response.ok) throw new Error('result_delivery_failed');
  } catch {
    // The runner reports its own bounded result timeout if delivery fails.
    document.body.textContent = 'Probe result delivery failed.';
  }
}
main();
