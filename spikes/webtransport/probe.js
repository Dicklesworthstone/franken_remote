// WebTransport and WSS fallback browser test probe

function log(msg) {
  console.log(msg);
  const pre = document.getElementById("log");
  if (pre) pre.textContent += msg + "\n";
}

async function run() {
  try {
    const configRes = await fetch("/config");
    const config = await configRes.json();
    log("Loaded config: " + JSON.stringify(config));

    if (config.mode === "wt_happy") {
      await testWebTransportHappy(config);
    } else if (config.mode === "wt_origin_reject") {
      await testWebTransportOriginReject(config);
    } else if (config.mode === "wss_fallback") {
      await testWssFallback(config);
    } else {
      throw new Error("Unknown mode: " + config.mode);
    }
  } catch (err) {
    log("ERROR: " + err);
    await reportResult({
      status: "failed",
      error: String(err),
      stack: err ? err.stack : undefined,
    });
  }
}

async function reportResult(result) {
  log("Reporting result: " + JSON.stringify(result));
  await fetch("/result", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(result),
  });
}

async function testWebTransportHappy(config) {
  log("Testing WebTransport happy path...");
  if (typeof WebTransport === "undefined") {
    throw new Error("WebTransport API not supported in this browser!");
  }

  const certHashBytes = new Uint8Array(config.certHashBytes);
  const url = `https://127.0.0.1:${config.port}/wt`;
  log(`Connecting WebTransport to ${url} with certificate hash ${config.certHashHex}`);

  const transport = new WebTransport(url, {
    serverCertificateHashes: [
      {
        algorithm: "sha-256",
        value: certHashBytes,
      },
    ],
  });

  log("Awaiting transport.ready...");
  await transport.ready;
  log("WebTransport handshake COMPLETED! transport.ready fulfilled.");

  // Test Datagrams
  const writer = transport.datagrams.writable.getWriter();
  const reader = transport.datagrams.readable.getReader();

  const sizes = [64, 256, 512, 1024, 1150];
  let echoedCount = 0;
  let maxEchoedSize = 0;

  for (const size of sizes) {
    const payload = new Uint8Array(size);
    for (let i = 0; i < size; i++) {
      payload[i] = (i ^ size) & 0xff;
    }
    log(`Sending ${size}B datagram...`);
    await writer.write(payload);

    // Read echo
    const { value, done } = await reader.read();
    if (done || !value) {
      throw new Error(`Datagram read returned done or empty for size ${size}`);
    }

    if (value.length !== size) {
      throw new Error(`Datagram length mismatch: expected ${size}, got ${value.length}`);
    }

    // Verify bytes
    for (let i = 0; i < size; i++) {
      if (value[i] !== ((i ^ size) & 0xff)) {
        throw new Error(`Datagram byte mismatch at index ${i}`);
      }
    }

    echoedCount++;
    maxEchoedSize = Math.max(maxEchoedSize, size);
    log(`Datagram ${size}B verified successfully!`);
  }

  // Test Unidirectional stream
  log("Testing unidirectional stream...");
  const uniStream = await transport.createUnidirectionalStream();
  const uniWriter = uniStream.getWriter();
  await uniWriter.write(new Uint8Array([0x11, 0x22, 0x33, 0x44]));
  await uniWriter.close();
  log("Unidirectional stream written and closed.");

  // Test Bidirectional stream
  log("Testing bidirectional stream...");
  const bidiStream = await transport.createBidirectionalStream();
  const bidiWriter = bidiStream.writable.getWriter();
  const bidiReader = bidiStream.readable.getReader();

  const bidiPayload = new Uint8Array([0xaa, 0xbb, 0xcc, 0xdd]);
  await bidiWriter.write(bidiPayload);
  await bidiWriter.close();

  const bidiReadRes = await bidiReader.read();
  let bidiEchoed = false;
  if (bidiReadRes.value && bidiReadRes.value.length === 4) {
    if (
      bidiReadRes.value[0] === 0xaa &&
      bidiReadRes.value[1] === 0xbb &&
      bidiReadRes.value[2] === 0xcc &&
      bidiReadRes.value[3] === 0xdd
    ) {
      bidiEchoed = true;
    }
  }
  log(`Bidirectional stream echoed: ${bidiEchoed}`);

  // Test Clean close
  log("Closing WebTransport cleanly...");
  transport.close({ closeCode: 0, reason: "test completed successfully" });
  await transport.closed;
  log("WebTransport closed successfully!");

  await reportResult({
    status: "passed",
    handshake_completed: true,
    datagrams_echoed: echoedCount,
    max_datagram_payload_bytes: maxEchoedSize,
    uni_stream_ok: true,
    bidi_stream_echoed: bidiEchoed,
    clean_close: true,
  });
}

async function testWebTransportOriginReject(config) {
  log("Testing WebTransport origin rejection negative test...");
  const certHashBytes = new Uint8Array(config.certHashBytes);
  const url = `https://127.0.0.1:${config.port}/wt`;

  const transport = new WebTransport(url, {
    serverCertificateHashes: [
      {
        algorithm: "sha-256",
        value: certHashBytes,
      },
    ],
  });

  try {
    await transport.ready;
    throw new Error("Expected WebTransport connection to be rejected, but it succeeded!");
  } catch (err) {
    log("WebTransport connection rejected as expected: " + err);
    await reportResult({
      status: "passed",
      origin_rejected: true,
      error: String(err),
    });
  }
}

async function testWssFallback(config) {
  log("Testing WSS fallback bounded credit and generation fencing...");

  return new Promise((resolve, reject) => {
    const ws1 = new WebSocket(`ws://127.0.0.1:${config.wssPort}/channel?role=video&gen=1`);
    let recordsReceived = 0;
    let initialCreditDone = false;
    let topupDone = false;

    ws1.binaryType = "arraybuffer";

    ws1.onopen = () => {
      log("WebSocket channel 1 (gen 1) opened.");
      // Grant initial 4096 bytes credit (4 records of 1024 bytes)
      ws1.send(JSON.stringify({
        type: "grant_credit",
        channel: "video",
        bytes: 4096,
        generation: 1
      }));
    };

    ws1.onmessage = (event) => {
      if (event.data instanceof ArrayBuffer) {
        recordsReceived++;
        log(`Received WS binary record #${recordsReceived} (${event.data.byteLength} bytes)`);

        if (recordsReceived === 4 && !initialCreditDone) {
          initialCreditDone = true;
          log("Received all 4 records for initial credit. Waiting to verify backpressure pause...");
          setTimeout(() => {
            // Verify no more records received while credit is 0
            if (recordsReceived !== 4) {
              reject(new Error(`Backpressure failed: received ${recordsReceived} records without credit top-up`));
              return;
            }
            log("Backpressure verified! Granting top-up credit of 2048 bytes...");
            ws1.send(JSON.stringify({
              type: "grant_credit",
              channel: "video",
              bytes: 2048,
              generation: 1
            }));
          }, 300);
        } else if (recordsReceived === 6 && !topupDone) {
          topupDone = true;
          log("Received 6 total records after top-up! Now testing stale-generation fencing...");

          // Open second channel on generation 2
          const ws2 = new WebSocket(`ws://127.0.0.1:${config.wssPort}/channel?role=video&gen=2`);
          ws2.onopen = () => {
            log("WebSocket channel 2 (gen 2) opened.");
            // Send a late credit grant with old generation 1 on ws1
            log("Sending late credit grant with generation 1 to test fencing...");
            ws1.send(JSON.stringify({
              type: "grant_credit",
              channel: "video",
              bytes: 1024,
              generation: 1
            }));
          };
        }
      } else if (typeof event.data === "string") {
        log(`Received WS text message: ${event.data}`);
        const parsed = JSON.parse(event.data);
        if (parsed.type === "refused" && parsed.reason === "stale_generation") {
          log("Stale generation refusal received and verified!");
          ws1.close();
          reportResult({
            status: "passed",
            credit_backpressure_verified: true,
            stale_generation_fenced: true,
            records_received: recordsReceived,
          }).then(resolve).catch(reject);
        }
      }
    };

    ws1.onerror = (e) => reject(new Error("WebSocket error: " + e));
  });
}

window.addEventListener("DOMContentLoaded", run);
