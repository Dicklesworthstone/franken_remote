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

static void quiet_audio_and_a_stuck_callback_are_distinct(void) {
    FrAcCallback callback;
    fr_ac_callback_init(&callback);
    atomic_int fault;
    atomic_init(&fault, 0);
    assert(!fr_ac_callback_expired(&callback, fr_ac_callback_snapshot(&callback), UINT64_MAX)); // Quiet, not stalled.
    uint64_t original = fr_ac_callback_begin(&callback, 1000);
    assert(original == 1000 + FR_AC_AGE_NS);
    assert(!fr_ac_callback_expired(&callback, original, original - 1));
    assert(!fr_ac_callback_begin(&callback, 1001)); // No renewal while entered.
    assert(fr_ac_callback_expired(&callback, original, original));
    assert(fr_ac_latch(&fault, -6) == -6); // Command thread observes timeout.
    assert(fr_ac_callback_complete(&callback, original)); // Native call returns late.
    assert(!fr_ac_callback_expired(&callback, original, UINT64_MAX));
    assert(atomic_load(&fault) == -6); // Completing does not revive the owner.
    assert(fr_ac_latch(&fault, -3) == -6); // Restored/changed permission cannot replace it.
}

static void callback_completion_cannot_clear_another_original_obligation(void) {
    FrAcCallback callback;
    fr_ac_callback_init(&callback);
    uint64_t first = fr_ac_callback_begin(&callback, 1000);
    assert(fr_ac_callback_complete(&callback, first));
    assert(!fr_ac_callback_begin(&callback, 1000)); // No duplicate/reversed clock ticket.
    uint64_t next = fr_ac_callback_begin(&callback, 1001);
    assert(next && next != first);
    assert(!fr_ac_callback_complete(&callback, first));
    assert(fr_ac_callback_expired(&callback, next, next));
    atomic_int fault;
    atomic_init(&fault, 0);
    assert(fr_ac_latch(&fault, -3) == -3); // First permission failure wins.
    assert(fr_ac_latch(&fault, -6) == -3);
    assert(fr_ac_callback_complete(&callback, next));
    assert(!fr_ac_callback_complete(&callback, next));
    assert(atomic_load(&fault) == -3);
}

static void callback_clock_reversal_requires_an_already_observed_original(void) {
    FrAcCallback callback;
    fr_ac_callback_init(&callback);
    uint64_t quiet_snapshot = fr_ac_callback_snapshot(&callback);
    uint64_t original = fr_ac_callback_begin(&callback, 1000);
    assert(!fr_ac_callback_expired(&callback, quiet_snapshot, 999));
    // The callback began after a quiet snapshot; this earlier command sample
    // cannot be evidence of a reversed clock on that newly entered callback.
    uint64_t entered_snapshot = fr_ac_callback_snapshot(&callback);
    assert(entered_snapshot == original);
    assert(fr_ac_callback_expired(&callback, entered_snapshot, 999));
    assert(!fr_ac_callback_expired(&callback, entered_snapshot, 1000));
    assert(fr_ac_callback_complete(&callback, original));
    uint64_t later = fr_ac_callback_begin(&callback, 2000);
    assert(later && later != original);
    assert(!fr_ac_callback_expired(&callback, entered_snapshot, UINT64_MAX));
    assert(fr_ac_callback_complete(&callback, later));
}

int main(void) {
    source_age_is_spent_before_and_during_copy();
    expired_open_keeps_original_start_cleanup();
    failed_native_start_does_not_prove_cleanup();
    close_racing_the_original_callback_stops_exactly_once();
    quiet_audio_and_a_stuck_callback_are_distinct();
    callback_completion_cannot_clear_another_original_obligation();
    callback_clock_reversal_requires_an_already_observed_original();
    puts("macos audio capture policy: 7 passed (including 200 callback races)");
    return 0;
}
