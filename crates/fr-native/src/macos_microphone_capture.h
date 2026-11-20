#ifndef FR_MACOS_MICROPHONE_CAPTURE_H
#define FR_MACOS_MICROPHONE_CAPTURE_H
#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>
#include <math.h>

enum {
    FR_MIC_OK = 0, FR_MIC_PENDING = 1,
    FR_MIC_CONFIG = -1, FR_MIC_SELECTION = -2, FR_MIC_UNAVAILABLE = -3,
    FR_MIC_DENIED = -4, FR_MIC_EXPIRED = -5, FR_MIC_CLOCK = -6,
    FR_MIC_NATIVE = -7, FR_MIC_DISCONTINUITY = -8,
    FR_MIC_ALLOCATION = -9, FR_MIC_CLEANUP = -10
};
#define FR_MIC_FRAMES 480u
#define FR_MIC_SLOTS 4u
#define FR_MIC_CUSTODY_NS UINT64_C(100000000)

// A native sample timestamp is correlated conservatively with the same
// CLOCK_MONOTONIC domain as the original private child request. Sampling
// cannot give an old sample a later capture time. A delayed callback, unknown
// timebase, future source time or wide clock read supplies no fresh audio.
static inline bool fr_mic_captured_ns(uint64_t before, uint64_t awake,
                                     uint64_t after, uint64_t host,
                                     uint32_t numer, uint32_t denom,
                                     uint64_t *out) {
    if (!out || !numer || !denom || after < before || after - before > 1000000 ||
        host > awake) return false;
    __uint128_t elapsed = (__uint128_t)(awake - host) * numer;
    elapsed = (elapsed + denom - 1) / denom;
    if (elapsed > before) return false;
    uint64_t captured = before - (uint64_t)elapsed;
    if (after - captured >= FR_MIC_CUSTODY_NS) return false;
    *out = captured;
    return true;
}

typedef struct {
    uint64_t next_native, frames, sequence, captured_ns;
    bool seen;
} FrMicTimeline;

// Advance only on one complete, contiguous native 10 ms buffer. The public
// sample position is relative to this original stream; it never restarts
// after a hole, overflow or rejected callback.
static inline bool fr_mic_observe(FrMicTimeline *time, double native_sample,
                                  uint64_t captured, uint64_t *sequence,
                                  uint64_t *position) {
    if (!time || !sequence || !position || !isfinite(native_sample) ||
        native_sample < 0 || native_sample > 9007199254740991.0 ||
        floor(native_sample) != native_sample || time->sequence == UINT64_MAX ||
        time->frames > UINT64_MAX - FR_MIC_FRAMES) return false;
    uint64_t sample = (uint64_t)native_sample;
    if (time->seen && (sample != time->next_native || captured <= time->captured_ns))
        return false;
    *sequence = time->sequence;
    *position = time->frames;
    time->sequence++;
    time->frames += FR_MIC_FRAMES;
    time->next_native = sample + FR_MIC_FRAMES;
    time->captured_ns = captured;
    time->seen = true;
    return true;
}

static inline bool fr_mic_ring_room(uint64_t written, uint64_t read) {
    return written >= read && written != UINT64_MAX && written - read < FR_MIC_SLOTS;
}

static inline bool fr_mic_renewable(uint64_t now, uint64_t installed,
                                   uint64_t original, uint64_t replacement) {
    return installed == original && now < original && replacement > original;
}

// Only the supervised native child owns these calls. create never asks for
// microphone permission, selects a default device, or retains a Rust pointer.
// Any non-null result, including failed setup, needs destroy. Failed destroy
// quarantines the callback storage and its bounded native owner credit.
int fr_macos_mic_create(const char *uid, int guard_fd, uint64_t authorization_ns,
                        uint64_t setup_until_ns, void **out);
int fr_macos_mic_renew(void *handle, uint64_t original_authorization_ns,
                       uint64_t new_authorization_ns);
int fr_macos_mic_read(void *handle, int16_t *samples, size_t capacity,
                      uint32_t *count, uint64_t *sequence,
                      uint64_t *sample_position, uint64_t *captured_ns);
int fr_macos_mic_destroy(void *handle);

// Metadata and negative-only clock queries do not open a capture device.
int fr_macos_mic_default_uid(uint8_t *bytes, size_t capacity, size_t *count);
int fr_macos_mic_sleep_offset(uint64_t *lower, uint64_t *upper);
// Only the explicit permission-only child may ask the OS. Both deadlines are
// retained from one original Allow gesture; a grant never starts capture.
int fr_mic_authorize(uint64_t request_until_ns, uint64_t wait_until_ns);
#endif
