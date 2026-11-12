// Exercises the actual portable arithmetic used by the Core Audio boundary.
// No Mach clock or device is substituted; real sampling remains a Mac test.
#include "../src/macos_audio_clock.h"
#include <assert.h>

int main(void) {
    FrAudioSleepOffset original, current;
    assert(fr_audio_sleep_offset(110, 102, 114, 1, 1, &original));
    assert(original.lower == 8 && original.upper == 12);
    assert(fr_audio_sleep_offset(1010, 1003, 1016, 1, 1, &current));
    assert(fr_audio_sleep_unchanged(original, current));
    // The awake clock advanced only 1000 ticks; sleep added another 5000.
    assert(fr_audio_sleep_offset(6110, 1102, 6114, 1, 1, &current));
    assert(!fr_audio_sleep_unchanged(original, current));
    // A regressed offset is also a discontinuity, never a new epoch.
    assert(fr_audio_sleep_offset(1100, 1102, 1104, 1, 1, &current));
    assert(!fr_audio_sleep_unchanged(original, current));
    // Near boot, the awake sample may pass the earlier continuous sample.
    assert(fr_audio_sleep_offset(0, 2, 4, 125, 3, &current));
    assert(current.lower == 0 && current.upper == 2);
    assert(!fr_audio_sleep_offset(0, 5, 4, 1, 1, &current));
    assert(!fr_audio_sleep_offset(4, 5, 3, 1, 1, &current));
    assert(!fr_audio_sleep_offset(0, 2, 4, 0, 1, &current));
    assert(!fr_audio_sleep_offset(0, 2, 4, 1, 0, &current));
    // More than 1 ms of sampling uncertainty cannot hide an unknown sleep.
    assert(fr_audio_sleep_offset(0, 100, 24000, 125, 3, &current));
    assert(!fr_audio_sleep_offset(0, 100, 24001, 125, 3, &current));
    // Sleep between any two reads is part of the continuous-width bound;
    // an unchanged awake clock cannot conceal ten seconds inside the sample.
    assert(!fr_audio_sleep_offset(1100, 1001, UINT64_C(10000001101),
                                   1, 1, &current));
    assert(!fr_audio_sleep_offset(0, UINT64_MAX, UINT64_MAX,
                                   UINT32_MAX, UINT32_MAX, &current));
    return 0;
}
