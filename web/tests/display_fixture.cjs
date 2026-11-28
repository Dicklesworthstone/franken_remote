"use strict";
// Independent canonical display-selection fixtures. No production wire encoder
// or protocol implementation is imported by these byte-level expectations.
const assert = require("node:assert/strict");
const scalar = (value, size) => {
    const bytes = Buffer.alloc(size);
    if (size === 16) bytes.writeBigUInt64BE(BigInt(value), 8);
    else if (size === 8) bytes.writeBigUInt64BE(BigInt(value));
    else bytes.writeUIntBE(value, 0, size);
    return bytes;
};
const signed = (value) => { const out = Buffer.alloc(4); out.writeInt32BE(value); return out; };
function record(kind, binding, payload) {
    const header = Buffer.alloc(24);
    header.write("FRD0");
    header.writeUInt16BE(kind, 6);
    header.writeUInt32BE(payload.length, 12);
    header.writeUInt32BE(binding, 16);
    return Buffer.concat([header, payload]);
}
const FIRST = Object.freeze({ handle: 21, geometry: 0, x: 0, y: 0, width: 320, height: 240 });
const SECOND = Object.freeze({ handle: 22, geometry: 4, x: -640, y: 0, width: 640, height: 480 });
function catalog(displays = [FIRST, SECOND], { revision = 31, session = 13, binding = 7 } = {}) {
    return record(0x20, binding, Buffer.concat([
        ...[11, 12, session].map((id) => scalar(id, 16)), scalar(revision, 8), scalar(displays.length, 1),
        ...displays.map((display) => Buffer.concat([
            scalar(display.handle, 16), scalar(display.geometry, 8), signed(display.x), signed(display.y),
            ...[display.width, display.height, display.width, display.height, 1, 1].map((value) => scalar(value, 4)),
            scalar(0, 1),
        ])),
    ]));
}
function selection(display = FIRST, revision = 31) {
    return record(0x22, 7, Buffer.concat([
        ...[11, 12, 13].map((id) => scalar(id, 16)), scalar(revision, 8), scalar(display.handle, 16),
        scalar(display.geometry, 8), ...[0, 0, display.width, display.height].map((value) => scalar(value, 4)),
    ]));
}
function capabilities(hello, { legacy = false, approval = false } = {}) {
    let payload = Buffer.concat([hello.subarray(24, -33), Buffer.from([0])]);
    assert.equal(payload.readUInt32BE(43), 5);
    const approvalName = Buffer.from("browser-local-approval");
    const approvalOffset = payload.indexOf(approvalName);
    assert.equal(approvalOffset, 51);
    payload[approvalOffset + approvalName.length + 2] = approval ? 1 : 0;
    if (legacy) {
        const name = Buffer.from("display-selection");
        const offset = payload.indexOf(name) - 4;
        assert.equal(payload.readUInt32BE(offset), name.length);
        payload = Buffer.concat([payload.subarray(0, offset), payload.subarray(offset + 4 + name.length + 3)]);
        payload.writeUInt32BE(4, 43);
    }
    return record(2, 0, payload);
}
function clockReply(probe, hostMicros) {
    assert.equal(probe.readUInt16BE(6), 0x84);
    return record(0x85, 7, Buffer.concat([probe.subarray(24), scalar(hostMicros, 8)]));
}
module.exports = { FIRST, SECOND, catalog, selection, capabilities, clockReply };
