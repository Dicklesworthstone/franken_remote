#ifndef FR_MACOS_AUDIO_CLOCK_H
#define FR_MACOS_AUDIO_CLOCK_H
#include <math.h>
#include <stdbool.h>
#include <stdint.h>

// All queue positions are in 48 kHz frames, independent of channel count.
#define FR_AUDIO_QUEUE_FRAMES 1920
#define FR_AUDIO_MAX_10MS_FRAMES 481
#define FR_AUDIO_MAX_20MS_FRAMES 961

// Reserve the negotiated packet plus its maximum drift correction. Device
// latency is in hardware frames and rounds UP before the 5 ms lead is added.
static inline bool fr_audio_output_lead(uint64_t device_frames, double device_rate,
                                        uint32_t largest_frames, uint64_t *out) {
    if (!out || !isfinite(device_rate) || device_rate < 8000 || device_rate > 384000 ||
        (largest_frames != FR_AUDIO_MAX_10MS_FRAMES && largest_frames != FR_AUDIO_MAX_20MS_FRAMES))
        return false;
    double lead = ceil((double)device_frames * 48000.0 / device_rate) + 240.0;
    if (lead > FR_AUDIO_QUEUE_FRAMES - largest_frames) return false;
    *out = (uint64_t)lead;
    return true;
}

// The continuous Mach clock adds time spent asleep to the awake Mach clock.
// Bracket the awake read with continuous reads: the sampling-width bound must
// include sleep too. Preemption widens the interval instead of being mistaken
// for a changed offset. Never renew this original interval.
typedef struct { uint64_t lower, upper; } FrAudioSleepOffset;

static inline bool fr_audio_sleep_offset(uint64_t continuous_before, uint64_t awake,
                                         uint64_t continuous_after, uint32_t numer,
                                         uint32_t denom, FrAudioSleepOffset *out) {
    if (!out || !numer || !denom || continuous_after < continuous_before ||
        continuous_after < awake ||
        (__uint128_t)(continuous_after - continuous_before) * numer >
            (__uint128_t)1000000 * denom)
        return false;
    out->lower = continuous_before > awake ? continuous_before - awake : 0;
    out->upper = continuous_after - awake;
    return true;
}

static inline bool fr_audio_sleep_unchanged(FrAudioSleepOffset original,
                                            FrAudioSleepOffset current) {
    return original.lower <= original.upper && current.lower <= current.upper &&
           current.lower <= original.upper && current.upper >= original.lower;
}
#endif
