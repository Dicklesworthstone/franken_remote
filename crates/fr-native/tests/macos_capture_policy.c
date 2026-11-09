// Portable tests of the exact production policy, not native capture evidence.
// cc -std=c11 -Wall -Wextra -Werror macos_capture_policy.c -o policy && ./policy
#include "../src/macos_capture_policy.h"
#include <assert.h>
#include <stdio.h>

static void silence_never_renews_source_evidence(void) {
    FrCapSource source = {0};
    assert(!fr_cap_source_ready(&source, 1));
    assert(fr_cap_source_observe(&source, FR_CAP_SOURCE_COMPLETE, 10, 11) ==
           FR_CAP_SOURCE_REPLACED);
    assert(fr_cap_source_ready(&source, 10));
    assert(!fr_cap_source_ready(&source, 12));
    assert(!fr_cap_source_ready(&source, UINT64_MAX));
    assert(source.picture == 1);
}

static void actual_idle_verifies_only_its_original_picture(void) {
    FrCapSource source = {0};
    assert(fr_cap_source_observe(&source, FR_CAP_SOURCE_IDLE, 10, 11) ==
           FR_CAP_SOURCE_IGNORED);
    assert(!fr_cap_source_ready(&source, 10));
    assert(fr_cap_source_observe(&source, FR_CAP_SOURCE_COMPLETE, 12, 13) ==
           FR_CAP_SOURCE_REPLACED);
    assert(fr_cap_source_observe(&source, FR_CAP_SOURCE_IDLE, 20, 21) ==
           FR_CAP_SOURCE_VERIFIED);
    assert(source.picture == 1);
    assert(fr_cap_source_ready(&source, 20));
    assert(!fr_cap_source_ready(&source, 22));
    assert(fr_cap_source_observe(&source, FR_CAP_SOURCE_IDLE, 20, 100) ==
           FR_CAP_SOURCE_IGNORED);
    assert(!fr_cap_source_ready(&source, 99));
}

static void queued_and_future_events_cannot_fabricate_freshness(void) {
    FrCapSource source = {0};
    assert(fr_cap_source_observe(&source, FR_CAP_SOURCE_COMPLETE, 10, 100) ==
           FR_CAP_SOURCE_REPLACED);
    assert(!fr_cap_source_ready(&source, 90));
    assert(fr_cap_source_observe(&source, FR_CAP_SOURCE_IDLE, 101, 100) ==
           FR_CAP_SOURCE_INVALIDATED);
    assert(!source.available);
    assert(fr_cap_source_observe(&source, FR_CAP_SOURCE_IDLE, 102, 103) ==
           FR_CAP_SOURCE_IGNORED);
    assert(fr_cap_source_observe(&source, FR_CAP_SOURCE_COMPLETE, 104, 105) ==
           FR_CAP_SOURCE_REPLACED);
    assert(source.picture == 2);
    assert(fr_cap_source_ready(&source, 104));
}

static void source_gaps_require_a_new_complete_picture(void) {
    FrCapSource source = {0};
    assert(fr_cap_source_observe(&source, FR_CAP_SOURCE_COMPLETE, 10, 11) ==
           FR_CAP_SOURCE_REPLACED);
    assert(fr_cap_source_observe(&source, FR_CAP_SOURCE_INVALID, 0, 20) ==
           FR_CAP_SOURCE_INVALIDATED);
    // A pre-gap frame arriving late cannot restore the old pixels.
    assert(fr_cap_source_observe(&source, FR_CAP_SOURCE_COMPLETE, 19, 30) ==
           FR_CAP_SOURCE_IGNORED);
    assert(!fr_cap_source_ready(&source, 18));
    assert(fr_cap_source_observe(&source, FR_CAP_SOURCE_IDLE, 31, 32) ==
           FR_CAP_SOURCE_IGNORED);
    assert(!source.available);
    assert(fr_cap_source_observe(&source, FR_CAP_SOURCE_COMPLETE, 33, 34) ==
           FR_CAP_SOURCE_REPLACED);
    assert(fr_cap_source_ready(&source, 33));
}

static void replacement_preserves_the_encoded_reference_identity(void) {
    FrCapSource source = {0};
    assert(fr_cap_source_observe(&source, FR_CAP_SOURCE_COMPLETE, 10, 11) ==
           FR_CAP_SOURCE_REPLACED);
    uint64_t encoded = source.picture;
    assert(fr_cap_source_observe(&source, FR_CAP_SOURCE_COMPLETE, 20, 21) ==
           FR_CAP_SOURCE_REPLACED);
    assert(source.picture != encoded);
    assert(fr_cap_source_observe(&source, FR_CAP_SOURCE_IDLE, 30, 31) ==
           FR_CAP_SOURCE_VERIFIED);
    assert(source.picture != encoded);
    encoded = source.picture;
    assert(fr_cap_source_observe(&source, FR_CAP_SOURCE_IDLE, 40, 41) ==
           FR_CAP_SOURCE_VERIFIED);
    assert(source.picture == encoded);
}

static void exhausted_identity_never_restarts(void) {
    FrCapSource source = {.picture = UINT64_MAX};
    assert(fr_cap_source_observe(&source, FR_CAP_SOURCE_COMPLETE, 10, 11) ==
           FR_CAP_SOURCE_INVALIDATED);
    assert(source.exhausted);
    assert(fr_cap_source_observe(&source, FR_CAP_SOURCE_COMPLETE, 12, 13) ==
           FR_CAP_SOURCE_INVALIDATED);
    assert(!fr_cap_source_ready(&source, 1));
}

static void invalidation_fences_inflight_completion_even_after_recovery(void) {
    FrCapSource source = {0};
    assert(fr_cap_source_observe(&source, FR_CAP_SOURCE_COMPLETE, 10, 11) ==
           FR_CAP_SOURCE_REPLACED);
    uint64_t selected = source.invalidation;
    assert(fr_cap_source_current(&source, selected));
    fr_cap_source_invalidate(&source, 20);
    assert(!fr_cap_source_current(&source, selected));
    // A new complete picture cannot authorize the old retained/encoded image.
    assert(fr_cap_source_observe(&source, FR_CAP_SOURCE_COMPLETE, 30, 31) ==
           FR_CAP_SOURCE_REPLACED);
    assert(fr_cap_source_ready(&source, 30));
    assert(!fr_cap_source_current(&source, selected));
    assert(fr_cap_source_current(&source, source.invalidation));
}

static void new_verification_can_repair_only_an_earlier_gap(void) {
    FrCapSource source = {0};
    fr_cap_source_invalidate(&source, 10);
    FrCapOneShot request = {.accepting = true};
    uint64_t selected = source.invalidation;
    assert(!source.available);
    assert(fr_cap_one_shot_complete(&request));
    // A serviced capture after the gap can produce a new encoded image even
    // when no complete stream image has arrived. It is never an Idle proof.
    assert(fr_cap_source_current(&source, selected));
    assert(!fr_cap_source_ready(&source, 11));

    FrCapOneShot later = {.accepting = true};
    selected = source.invalidation;
    fr_cap_source_invalidate(&source, 20);
    assert(fr_cap_one_shot_complete(&later));
    assert(!fr_cap_source_current(&source, selected));
}

static void exhausted_invalidation_identity_never_restarts(void) {
    FrCapSource source = {.invalidation = UINT64_MAX - 1};
    fr_cap_source_invalidate(&source, 10);
    assert(source.invalidation == UINT64_MAX);
    assert(fr_cap_source_current(&source, UINT64_MAX));
    fr_cap_source_invalidate(&source, 20);
    assert(source.invalidation == UINT64_MAX);
    assert(source.exhausted);
    assert(!fr_cap_source_current(&source, UINT64_MAX));
    assert(fr_cap_source_observe(&source, FR_CAP_SOURCE_COMPLETE, 30, 31) ==
           FR_CAP_SOURCE_INVALIDATED);
    assert(!fr_cap_source_ready(&source, 30));
}

static void each_verification_has_one_cancellable_result(void) {
    FrCapOneShot request = {.accepting = true};
    assert(fr_cap_one_shot_complete(&request));
    assert(!fr_cap_one_shot_complete(&request));
    fr_cap_one_shot_close(&request);
    assert(!fr_cap_one_shot_complete(&request));

    FrCapOneShot expired = {.accepting = true};
    fr_cap_one_shot_close(&expired);
    assert(!fr_cap_one_shot_complete(&expired));
    assert(!expired.completed);
    // Closing one operation cannot admit or complete its replacement.
    FrCapOneShot next = {.accepting = true};
    assert(fr_cap_one_shot_complete(&next));
    assert(!fr_cap_one_shot_complete(&expired));
}

static void encoded_allocation_is_bounded_before_append(void) {
    assert(fr_cap_nal_fits(0, 4, 8));
    assert(!fr_cap_nal_fits(0, 5, 8));
    assert(!fr_cap_nal_fits(0, 0, 8));
    if (SIZE_MAX > UINT32_MAX) {
        assert(!fr_cap_nal_fits(0, (size_t)UINT32_MAX + 1, SIZE_MAX));
    }
    assert(!fr_cap_nal_fits(9, 1, 8));
    assert(!fr_cap_nal_fits(SIZE_MAX, 1, SIZE_MAX));
    assert(!fr_cap_nal_fits(SIZE_MAX - 3, 1, SIZE_MAX));
    assert(fr_cap_nal_fits(SIZE_MAX - 5, 1, SIZE_MAX));
}

int main(void) {
    silence_never_renews_source_evidence();
    actual_idle_verifies_only_its_original_picture();
    queued_and_future_events_cannot_fabricate_freshness();
    source_gaps_require_a_new_complete_picture();
    replacement_preserves_the_encoded_reference_identity();
    exhausted_identity_never_restarts();
    invalidation_fences_inflight_completion_even_after_recovery();
    new_verification_can_repair_only_an_earlier_gap();
    exhausted_invalidation_identity_never_restarts();
    each_verification_has_one_cancellable_result();
    encoded_allocation_is_bounded_before_append();
    puts("11 macOS capture policy tests passed");
    return 0;
}
