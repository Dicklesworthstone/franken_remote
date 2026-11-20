// Exact production timestamp/custody arithmetic. These are portable policy
// tests, not Audio Queue, microphone permission or hardware qualification.
#include "../src/macos_microphone_capture.h"
#include "../src/macos_audio_clock.h"
#include <assert.h>
#include <stdio.h>
#include <string.h>

static void captured_time_preserves_original_age(void) {
    uint64_t captured = 0;
    assert(fr_mic_captured_ns(1000000000, 900000000, 1000000100,
                             880000000, 1, 1, &captured));
    assert(captured == 980000000);
    // The cross-clock interval only shortens custody. Round elapsed time UP,
    // including a Mach timebase with fractional nanoseconds per tick.
    assert(fr_mic_captured_ns(1000000000, 100, 1000000100, 97, 125, 3, &captured));
    assert(captured == 999999875);
    assert(fr_mic_captured_ns(1000000000, 100, 1000000100, 99, 1, 3, &captured));
    assert(captured == 999999999);
    assert(!fr_mic_captured_ns(1000000000, 100, 999999999, 99, 1, 1, &captured));
    assert(!fr_mic_captured_ns(1000000000, 100, 1001000001, 99, 1, 1, &captured));
    assert(!fr_mic_captured_ns(1000000000, 100, 1000000100, 101, 1, 1, &captured));
    assert(!fr_mic_captured_ns(1000000000, 100, 1000000100, 99, 0, 1, &captured));
    assert(!fr_mic_captured_ns(1000000000, 100, 1000000100, 99, 1, 0, &captured));
    assert(!fr_mic_captured_ns(1000000000, UINT64_MAX, 1000000100,
                              0, UINT32_MAX, 1, &captured));
    // Exactly expired is terminal, even if the current request is brand new.
    assert(!fr_mic_captured_ns(1000000000, 1000000000, 1000000000,
                              900000000, 1, 1, &captured));
    assert(fr_mic_captured_ns(1000000000, 1000000000, 1000000000,
                             900000001, 1, 1, &captured));
    assert(captured == 900000001);
}

static void samples_never_hide_gaps_or_restart(void) {
    FrMicTimeline timeline = {0};
    uint64_t sequence = 99, position = 99;
    for (unsigned i = 0; i < 200; ++i) {
        assert(fr_mic_observe(&timeline, 1234.0 + i * FR_MIC_FRAMES,
                              1000000 + i * UINT64_C(10000000), &sequence, &position));
        assert(sequence == i && position == i * FR_MIC_FRAMES);
    }
    FrMicTimeline original = timeline;
    double bad[] = {0, 1234, timeline.next_native - 1.0, timeline.next_native + 1.0,
                    timeline.next_native + 0.5, -1, NAN, INFINITY, 9007199254740992.0};
    for (unsigned i = 0; i < sizeof bad / sizeof bad[0]; ++i) {
        assert(!fr_mic_observe(&timeline, bad[i], UINT64_C(3000000000),
                               &sequence, &position));
        assert(memcmp(&timeline, &original, sizeof timeline) == 0);
    }
    assert(!fr_mic_observe(&timeline, (double)timeline.next_native,
                           timeline.captured_ns, &sequence, &position));
    assert(!fr_mic_observe(&timeline, (double)timeline.next_native,
                           timeline.captured_ns - 1, &sequence, &position));
    timeline.sequence = UINT64_MAX;
    assert(!fr_mic_observe(&timeline, (double)timeline.next_native,
                           UINT64_C(3000000000), &sequence, &position));
    timeline = original;
    timeline.frames = UINT64_MAX - FR_MIC_FRAMES + 1;
    assert(!fr_mic_observe(&timeline, (double)timeline.next_native,
                           UINT64_C(3000000000), &sequence, &position));
}

static void no_overflow_or_deadline_resurrection(void) {
    assert(fr_mic_ring_room(0, 0));
    assert(fr_mic_ring_room(3, 0));
    assert(!fr_mic_ring_room(4, 0));
    assert(!fr_mic_ring_room(0, 1));
    assert(fr_mic_ring_room(UINT64_MAX - 1, UINT64_MAX - 4));
    assert(!fr_mic_ring_room(UINT64_MAX, UINT64_MAX - 1));
    assert(fr_mic_renewable(99, 100, 100, 200));
    assert(!fr_mic_renewable(100, 100, 100, 200));
    assert(!fr_mic_renewable(101, 100, 100, 200));
    assert(!fr_mic_renewable(99, 100, 99, 200));
    assert(!fr_mic_renewable(99, 100, 100, 100));
    assert(!fr_mic_renewable(99, 100, 100, 50));
}

static void sleep_cannot_renew_sample_or_permission_age(void) {
    FrAudioSleepOffset original, awake, slept;
    assert(fr_audio_sleep_offset(1000000, 900000, 1000010, 1, 1, &original));
    assert(fr_audio_sleep_offset(2000000, 1900000, 2000010, 1, 1, &awake));
    assert(fr_audio_sleep_unchanged(original, awake));
    assert(fr_audio_sleep_offset(3000000, 1900000, 3000010, 1, 1, &slept));
    assert(!fr_audio_sleep_unchanged(original, slept));
    // Scheduling delay cannot be interpreted as a reliable new correlation.
    assert(!fr_audio_sleep_offset(1000000, 900000, 2000001, 1, 1, &awake));
}

int main(void) {
    captured_time_preserves_original_age();
    samples_never_hide_gaps_or_restart();
    no_overflow_or_deadline_resurrection();
    sleep_cannot_renew_sample_or_permission_age();
    puts("macOS microphone capture policy: 4 groups passed");
    return 0;
}
