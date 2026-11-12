// Executes the exact portable helpers used by macos_audio_capture.m. This is
// clock/lifetime policy evidence, not ScreenCaptureKit or hardware evidence.
#include "../src/macos_audio_capture_policy.h"
#include <assert.h>
#include <pthread.h>
#include <stdio.h>

static void source_age_is_spent_before_and_during_copy(void) {
    FrAcDeadline source;
    assert(fr_ac_source_window(1000, FR_AC_AGE_NS - 20, &source));
    assert(source.began == 1000 && source.until == 1020);
    assert(!fr_ac_live(source, 999)); // Reversed monotonic clock.
    assert(fr_ac_live(source, 1019));
    assert(!fr_ac_live(source, 1020)); // Delayed query/copy cannot renew it.
    assert(!fr_ac_live(source, UINT64_MAX));
    assert(!fr_ac_source_window(1000, FR_AC_AGE_NS, &source));
    assert(!fr_ac_source_window(1000, UINT64_MAX, &source));
    assert(!fr_ac_source_window(UINT64_MAX - 1, 0, &source));
    assert(!fr_ac_source_window(0, 0, &source));
    assert(!fr_ac_source_window(1000, 0, NULL));
    FrAcDeadline original = fr_ac_deadline(100, 200);
    assert(fr_ac_live(original, 101)); // setup lock
    assert(fr_ac_live(original, 299)); // discovery completed late
    assert(!fr_ac_live(original, 300)); // no new startup allowance
}

static void expired_open_keeps_original_start_cleanup(void) {
    FrAcOpening original;
    fr_ac_opening_init(&original, -2);
    assert(fr_ac_opening_begin(&original));
    assert(!fr_ac_opening_begin(&original));
    fr_ac_opening_close(&original);
    assert(!fr_ac_opening_claim_stop(&original)); // Start still owns lease.
    assert(fr_ac_opening_complete(&original, 0));
    assert(fr_ac_opening_needs_native_stop(&original));
    assert(fr_ac_opening_claim_stop(&original));
    assert(!fr_ac_opening_claim_stop(&original));
    assert(!fr_ac_opening_begin(&original));
    assert(!fr_ac_opening_complete(&original, -10));
    assert(fr_ac_opening_result(&original) == 0);
}

static void failed_native_start_does_not_prove_cleanup(void) {
    FrAcOpening failed;
    fr_ac_opening_init(&failed, -2);
    assert(fr_ac_opening_begin(&failed));
    assert(fr_ac_opening_complete(&failed, -10));
    assert(!fr_ac_opening_claim_stop(&failed));
    fr_ac_opening_close(&failed);
    assert(fr_ac_opening_claim_stop(&failed));
    assert(fr_ac_opening_needs_native_stop(&failed));
    assert(fr_ac_opening_result(&failed) == -10);
    assert(!fr_ac_opening_complete(&failed, 0));
    assert(fr_ac_opening_result(&failed) == -10);

    FrAcOpening unopened;
    fr_ac_opening_init(&unopened, -2);
    fr_ac_opening_close(&unopened);
    assert(!fr_ac_opening_begin(&unopened));
    assert(fr_ac_opening_claim_stop(&unopened));
    assert(!fr_ac_opening_needs_native_stop(&unopened));
}

typedef struct { FrAcOpening original; atomic_int stops; } Race;
static void *complete(void *raw) {
    Race *race = raw;
    assert(fr_ac_opening_complete(&race->original, 0));
    if (fr_ac_opening_claim_stop(&race->original)) atomic_fetch_add(&race->stops, 1);
    return NULL;
}
static void close_racing_the_original_callback_stops_exactly_once(void) {
    for (unsigned i = 0; i < 200; ++i) {
        Race race;
        fr_ac_opening_init(&race.original, -2);
        atomic_init(&race.stops, 0);
        assert(fr_ac_opening_begin(&race.original));
        pthread_t callback;
        assert(pthread_create(&callback, NULL, complete, &race) == 0);
        fr_ac_opening_close(&race.original);
        if (fr_ac_opening_claim_stop(&race.original)) atomic_fetch_add(&race.stops, 1);
        assert(pthread_join(callback, NULL) == 0);
        assert(atomic_load(&race.stops) == 1);
        assert(fr_ac_opening_closed(&race.original));
        assert(!fr_ac_opening_claim_stop(&race.original));
    }
}

int main(void) {
    source_age_is_spent_before_and_during_copy();
    expired_open_keeps_original_start_cleanup();
    failed_native_start_does_not_prove_cleanup();
    close_racing_the_original_callback_stops_exactly_once();
    puts("macos audio capture policy: 4 passed (including 200 callback races)");
    return 0;
}
