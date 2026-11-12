#ifndef FR_MACOS_AUDIO_CLOCK_H
#define FR_MACOS_AUDIO_CLOCK_H
#include <stdbool.h>
#include <stdint.h>

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
