/*
 * worker.js — Dedicated WebCodecs Decoder Worker
 *
 * Runs VideoDecoder off the main thread, keeping UI interactions silky smooth.
 * Enforces bounded decode queues and prompt frame closure.
 */

let decoder = null;
let pendingFrames = 0;
const MAX_PENDING_QUEUE = 3;

self.onmessage = async (e) => {
    const { type, data } = e.data;

    switch (type) {
        case 'configure':
            initDecoder(data);
            break;

        case 'decode':
            if (decoder && decoder.state === 'configured') {
                if (decoder.decodeQueueSize > MAX_PENDING_QUEUE) {
                    // Backpressure: drop delta frame or wait for recovery
                    return;
                }
                const chunk = new EncodedVideoChunk({
                    type: data.isKey ? 'key' : 'delta',
                    timestamp: data.timestamp,
                    data: data.bytes
                });
                decoder.decode(chunk);
            }
            break;

        case 'reset':
            if (decoder) {
                decoder.reset();
            }
            break;

        case 'close':
            if (decoder) {
                decoder.close();
                decoder = null;
            }
            break;
    }
};

function initDecoder(config) {
    if (decoder) {
        decoder.close();
    }

    decoder = new VideoDecoder({
        output: (frame) => {
            // Transfer VideoFrame to main thread for immediate presentation
            self.postMessage({
                type: 'frame',
                frame: frame
            }, [frame]);
        },
        error: (err) => {
            self.postMessage({
                type: 'error',
                message: err.message
            });
        }
    });

    try {
        decoder.configure({
            codec: config.codec || 'hvc1.1.6.L93.B0',
            description: config.description,
            codedWidth: config.width,
            codedHeight: config.height,
            hardwareAcceleration: 'prefer-hardware'
        });
    } catch (err) {
        self.postMessage({
            type: 'error',
            message: 'Failed to configure VideoDecoder: ' + err.message
        });
    }
}
