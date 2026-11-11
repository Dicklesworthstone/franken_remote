// Portable tests of the exact production policy, not native capture evidence.
// cc -std=c11 -Wall -Wextra -Werror macos_capture_policy.c -o policy && ./policy
#include "../src/macos_capture_policy.h"
#include <assert.h>
#include <stdio.h>

static void deadline_has_exact_monotonic_boundaries(void) {
    FrCapDeadline deadline;
    assert(fr_cap_deadline_init(&deadline, 100, 2000));
    assert(!fr_cap_deadline_live(&deadline, 99));
    assert(fr_cap_deadline_remaining(&deadline, 99) == 0);
    assert(fr_cap_deadline_live(&deadline, 100));
    assert(fr_cap_deadline_remaining(&deadline, 100) == 2000);
    assert(fr_cap_deadline_live(&deadline, 2099));
    assert(fr_cap_deadline_remaining(&deadline, 2099) == 1);
    assert(!fr_cap_deadline_live(&deadline, 2100));
    assert(fr_cap_deadline_remaining(&deadline, 2100) == 0);
    assert(!fr_cap_deadline_live(&deadline, UINT64_MAX));
}

static void deadline_refuses_zero_and_overflow_without_wrapping(void) {
    FrCapDeadline deadline = {0};
    assert(!fr_cap_deadline_init(&deadline, 42, 0));
    assert(!fr_cap_deadline_live(&deadline, 42));
    assert(!fr_cap_deadline_init(&deadline, UINT64_MAX - 1, 2));
    assert(!fr_cap_deadline_live(&deadline, UINT64_MAX - 1));
    assert(!fr_cap_deadline_live(&deadline, 0));
    assert(fr_cap_deadline_init(&deadline, UINT64_MAX - 1, 1));
    assert(fr_cap_deadline_remaining(&deadline, UINT64_MAX - 1) == 1);
    assert(!fr_cap_deadline_live(&deadline, UINT64_MAX));
}

static void every_setup_stage_spends_the_same_original_budget(void) {
    FrCapDeadline deadline;
    assert(fr_cap_deadline_init(&deadline, 100, 2000));
    // These are policy clock inputs, not simulated native completion claims.
    assert(fr_cap_deadline_remaining(&deadline, 500) == 1600);
    assert(fr_cap_deadline_remaining(&deadline, 1700) == 400);
    assert(fr_cap_deadline_remaining(&deadline, 2099) == 1);
    FrCapOpeningPolicy opening = {.accepting = true};
    assert(fr_cap_opening_request(&opening));
    assert(fr_cap_opening_complete(&opening, true));
    assert(!fr_cap_opening_publish(&opening, &deadline, 2100));
    assert(deadline.began == 100 && deadline.until == 2100);
    assert(fr_cap_deadline_remaining(&deadline, 2200) == 0);
}

static void only_an_actual_timely_start_transfers_ownership_once(void) {
    FrCapDeadline deadline;
    assert(fr_cap_deadline_init(&deadline, 100, 2000));
    FrCapOpeningPolicy opening = {.accepting = true};
    assert(!fr_cap_opening_complete(&opening, true));
    assert(!fr_cap_opening_publish(&opening, &deadline, 100));
    assert(!fr_cap_opening_release(&opening));
    assert(fr_cap_opening_request(&opening));
    assert(!fr_cap_opening_request(&opening));
    assert(!fr_cap_opening_publish(&opening, &deadline, 100));
    assert(fr_cap_opening_complete(&opening, true));
    assert(!fr_cap_opening_complete(&opening, false));
    assert(!fr_cap_opening_publish(&opening, &deadline, 99));
    assert(fr_cap_opening_publish(&opening, &deadline, 2099));
    assert(!fr_cap_opening_publish(&opening, &deadline, 2099));
    assert(!fr_cap_opening_request(&opening));
    assert(!fr_cap_opening_retire(&opening));
    assert(fr_cap_opening_release(&opening));
    assert(!fr_cap_opening_release(&opening));
    assert(!fr_cap_opening_complete(&opening, true));
}

static void a_completed_start_cannot_publish_after_expiry_or_close(void) {
    FrCapDeadline deadline;
    assert(fr_cap_deadline_init(&deadline, 100, 2000));
    FrCapOpeningPolicy expired = {.accepting = true};
    assert(fr_cap_opening_request(&expired));
    assert(fr_cap_opening_complete(&expired, true));
    assert(!fr_cap_opening_publish(&expired, &deadline, 2100));
    assert(!fr_cap_opening_release(&expired));
    fr_cap_opening_close(&expired);
    assert(fr_cap_opening_retire(&expired));
    assert(!fr_cap_opening_publish(&expired, &deadline, 2099));

    FrCapOpeningPolicy closed = {.accepting = true};
    assert(fr_cap_opening_request(&closed));
    assert(fr_cap_opening_complete(&closed, true));
    fr_cap_opening_close(&closed);
    assert(!fr_cap_opening_publish(&closed, &deadline, 101));
    assert(fr_cap_opening_retire(&closed));
}

static void close_before_start_retires_without_fabricating_a_callback(void) {
    FrCapOpeningPolicy opening = {.accepting = true};
    fr_cap_opening_close(&opening);
    assert(!fr_cap_opening_request(&opening));
    assert(!fr_cap_opening_complete(&opening, true));
    assert(fr_cap_opening_retire(&opening));
    assert(!fr_cap_opening_retire(&opening));
    assert(!opening.requested && !opening.completed && !opening.started);
    assert(!fr_cap_opening_release(&opening));
    opening.codec_retired = true;
    assert(!fr_cap_opening_release(&opening));
    opening.stream_retired = true;
    assert(fr_cap_opening_release(&opening));
}

static void late_start_success_or_failure_keeps_its_original_retirement_owner(void) {
    FrCapDeadline deadline;
    assert(fr_cap_deadline_init(&deadline, 100, 2000));
    for (int success = 0; success <= 1; success++) {
        FrCapOpeningPolicy opening = {.accepting = true};
        assert(fr_cap_opening_request(&opening));
        fr_cap_opening_close(&opening);
        // Closing the encoder while start is pending cannot release the lease.
        opening.codec_retired = true;
        assert(!fr_cap_opening_retire(&opening));
        assert(!fr_cap_opening_release(&opening));
        assert(fr_cap_opening_complete(&opening, success != 0));
        assert(opening.started == (success != 0));
        assert(!fr_cap_opening_complete(&opening, success == 0));
        assert(!fr_cap_opening_publish(&opening, &deadline, 101));
        assert(fr_cap_opening_retire(&opening));
        assert(!fr_cap_opening_retire(&opening));
        assert(!fr_cap_opening_release(&opening));
        opening.stream_retired = true;
        assert(fr_cap_opening_release(&opening));
        assert(!fr_cap_opening_release(&opening));
    }
}

static void either_cleanup_order_requires_both_positive_acknowledgements(void) {
    for (int codec_first = 0; codec_first <= 1; codec_first++) {
        FrCapOpeningPolicy opening = {.accepting = true};
        assert(fr_cap_opening_request(&opening));
        assert(fr_cap_opening_complete(&opening, true));
        fr_cap_opening_close(&opening);
        assert(fr_cap_opening_retire(&opening));
        assert(!fr_cap_opening_release(&opening));
        if (codec_first)
            opening.codec_retired = true;
        else
            opening.stream_retired = true;
        // A hanging or failed other branch must retain the same owner/lease.
        assert(!fr_cap_opening_release(&opening));
        assert(!fr_cap_opening_request(&opening));
        assert(!fr_cap_opening_retire(&opening));
        opening.codec_retired = true;
        opening.stream_retired = true;
        assert(fr_cap_opening_release(&opening));
        assert(!fr_cap_opening_release(&opening));
    }
}

static void start_failure_cannot_be_replaced_by_a_second_success_callback(void) {
    FrCapDeadline deadline;
    assert(fr_cap_deadline_init(&deadline, 100, 2000));
    FrCapOpeningPolicy opening = {.accepting = true};
    assert(fr_cap_opening_request(&opening));
    assert(fr_cap_opening_complete(&opening, false));
    assert(!fr_cap_opening_complete(&opening, true));
    assert(!opening.started);
    assert(!fr_cap_opening_publish(&opening, &deadline, 101));
    fr_cap_opening_close(&opening);
    assert(fr_cap_opening_retire(&opening));
    assert(!fr_cap_opening_release(&opening));
}

static void silence_never_renews_source_evidence(void) {
    FrCapSource source = {0};
    assert(!fr_cap_source_ready(&source, 1));
    assert(fr_cap_source_observe(&source, FR_CAP_SOURCE_COMPLETE, 10, 11, 0) ==
           FR_CAP_SOURCE_REPLACED);
    assert(fr_cap_source_ready(&source, 10));
    assert(!fr_cap_source_ready(&source, 12));
    assert(!fr_cap_source_ready(&source, UINT64_MAX));
    assert(source.picture == 1);
}

static void actual_idle_verifies_only_its_original_picture(void) {
    FrCapSource source = {0};
    assert(fr_cap_source_observe(&source, FR_CAP_SOURCE_IDLE, 10, 11, 0) ==
           FR_CAP_SOURCE_IGNORED);
    assert(!fr_cap_source_ready(&source, 10));
    assert(fr_cap_source_observe(&source, FR_CAP_SOURCE_COMPLETE, 12, 13, 0) ==
           FR_CAP_SOURCE_REPLACED);
    assert(fr_cap_source_observe(&source, FR_CAP_SOURCE_IDLE, 20, 21, 0) ==
           FR_CAP_SOURCE_VERIFIED);
    assert(source.picture == 1);
    assert(fr_cap_source_ready(&source, 20));
    assert(!fr_cap_source_ready(&source, 22));
    assert(fr_cap_source_observe(&source, FR_CAP_SOURCE_IDLE, 20, 100, 0) ==
           FR_CAP_SOURCE_IGNORED);
    assert(!fr_cap_source_ready(&source, 99));
}

static void queued_and_future_events_cannot_fabricate_freshness(void) {
    FrCapSource source = {0};
    assert(fr_cap_source_observe(&source, FR_CAP_SOURCE_COMPLETE, 10, 100, 0) ==
           FR_CAP_SOURCE_REPLACED);
    assert(!fr_cap_source_ready(&source, 90));
    assert(fr_cap_source_observe(&source, FR_CAP_SOURCE_IDLE, 101, 100, 0) ==
           FR_CAP_SOURCE_INVALIDATED);
    assert(!source.available);
    assert(fr_cap_source_observe(&source, FR_CAP_SOURCE_IDLE, 102, 103, 0) ==
           FR_CAP_SOURCE_IGNORED);
    assert(fr_cap_source_observe(&source, FR_CAP_SOURCE_COMPLETE, 104, 105, 0) ==
           FR_CAP_SOURCE_REPLACED);
    assert(source.picture == 2);
    assert(fr_cap_source_ready(&source, 104));
}

static void scheduled_display_times_are_current_within_their_lead(void) {
    FrCapSource source = {0};
    // Delivered at 100 for display at 105: a scheduled frame, not a forgery.
    assert(fr_cap_source_observe(&source, FR_CAP_SOURCE_COMPLETE, 105, 100, 10) ==
           FR_CAP_SOURCE_REPLACED);
    assert(fr_cap_source_ready(&source, 100));
    assert(fr_cap_source_observe(&source, FR_CAP_SOURCE_IDLE, 110, 100, 10) ==
           FR_CAP_SOURCE_VERIFIED);
    // Beyond the lead it is invalid metadata and fences the picture.
    assert(fr_cap_source_observe(&source, FR_CAP_SOURCE_IDLE, 121, 110, 10) ==
           FR_CAP_SOURCE_INVALIDATED);
    assert(!source.available);
    // The bound saturates rather than wrapping at the end of the clock.
    FrCapSource late = {0};
    assert(fr_cap_source_observe(&late, FR_CAP_SOURCE_COMPLETE, UINT64_MAX, UINT64_MAX - 1,
                                 10) == FR_CAP_SOURCE_REPLACED);
}

static void source_gaps_require_a_new_complete_picture(void) {
    FrCapSource source = {0};
    assert(fr_cap_source_observe(&source, FR_CAP_SOURCE_COMPLETE, 10, 11, 0) ==
           FR_CAP_SOURCE_REPLACED);
    assert(fr_cap_source_observe(&source, FR_CAP_SOURCE_INVALID, 0, 20, 0) ==
           FR_CAP_SOURCE_INVALIDATED);
    // A pre-gap frame arriving late cannot restore the old pixels.
    assert(fr_cap_source_observe(&source, FR_CAP_SOURCE_COMPLETE, 19, 30, 0) ==
           FR_CAP_SOURCE_IGNORED);
    assert(!fr_cap_source_ready(&source, 18));
    assert(fr_cap_source_observe(&source, FR_CAP_SOURCE_IDLE, 31, 32, 0) ==
           FR_CAP_SOURCE_IGNORED);
    assert(!source.available);
    assert(fr_cap_source_observe(&source, FR_CAP_SOURCE_COMPLETE, 33, 34, 0) ==
           FR_CAP_SOURCE_REPLACED);
    assert(fr_cap_source_ready(&source, 33));
}

static void replacement_preserves_the_encoded_reference_identity(void) {
    FrCapSource source = {0};
    assert(fr_cap_source_observe(&source, FR_CAP_SOURCE_COMPLETE, 10, 11, 0) ==
           FR_CAP_SOURCE_REPLACED);
    uint64_t encoded = source.picture;
    assert(fr_cap_source_observe(&source, FR_CAP_SOURCE_COMPLETE, 20, 21, 0) ==
           FR_CAP_SOURCE_REPLACED);
    assert(source.picture != encoded);
    assert(fr_cap_source_observe(&source, FR_CAP_SOURCE_IDLE, 30, 31, 0) ==
           FR_CAP_SOURCE_VERIFIED);
    assert(source.picture != encoded);
    encoded = source.picture;
    assert(fr_cap_source_observe(&source, FR_CAP_SOURCE_IDLE, 40, 41, 0) ==
           FR_CAP_SOURCE_VERIFIED);
    assert(source.picture == encoded);
}

static void exhausted_identity_never_restarts(void) {
    FrCapSource source = {.picture = UINT64_MAX};
    assert(fr_cap_source_observe(&source, FR_CAP_SOURCE_COMPLETE, 10, 11, 0) ==
           FR_CAP_SOURCE_INVALIDATED);
    assert(source.exhausted);
    assert(fr_cap_source_observe(&source, FR_CAP_SOURCE_COMPLETE, 12, 13, 0) ==
           FR_CAP_SOURCE_INVALIDATED);
    assert(!fr_cap_source_ready(&source, 1));
}

static void invalidation_fences_inflight_completion_even_after_recovery(void) {
    FrCapSource source = {0};
    assert(fr_cap_source_observe(&source, FR_CAP_SOURCE_COMPLETE, 10, 11, 0) ==
           FR_CAP_SOURCE_REPLACED);
    uint64_t selected = source.invalidation;
    assert(fr_cap_source_current(&source, selected));
    fr_cap_source_invalidate(&source, 20);
    assert(!fr_cap_source_current(&source, selected));
    // A new complete picture cannot authorize the old retained/encoded image.
    assert(fr_cap_source_observe(&source, FR_CAP_SOURCE_COMPLETE, 30, 31, 0) ==
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
    assert(fr_cap_source_observe(&source, FR_CAP_SOURCE_COMPLETE, 30, 31, 0) ==
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

static void a_positive_live_check_stands_for_at_most_its_bound(void) {
    // Never checked: always ask the system.
    assert(!fr_cap_live_reusable(0, 0));
    assert(!fr_cap_live_reusable(0, 5));
    assert(fr_cap_live_reusable(1000, 1000));
    assert(fr_cap_live_reusable(1000, 1000 + FR_CAP_LIVE_REUSE_NS - 1));
    assert(!fr_cap_live_reusable(1000, 1000 + FR_CAP_LIVE_REUSE_NS));
    // A clock regression cannot stretch an old answer.
    assert(!fr_cap_live_reusable(1000, 999));
    assert(!fr_cap_live_reusable(UINT64_MAX, 0));
    assert(fr_cap_live_reusable(UINT64_MAX - 1, UINT64_MAX));
}

static void a_refusal_outranks_every_answer_that_began_before_it(void) {
    FrCapLive live = {0, 0};
    // Positive answers only move forward.
    fr_cap_live_record(&live, 100, true);
    assert(live.positive == 100);
    fr_cap_live_record(&live, 90, true);
    assert(live.positive == 100);
    fr_cap_live_record(&live, 0, true);
    assert(live.positive == 100);
    // A refusal clears the positive answer that began before it ...
    fr_cap_live_record(&live, 150, false);
    assert(live.positive == 0 && live.refused == 150);
    assert(!fr_cap_live_reusable(live.positive, 151));
    // ... and a slower positive check that began before the refusal cannot
    // restore an answer once the refusal is recorded.
    fr_cap_live_record(&live, 140, true);
    fr_cap_live_record(&live, 150, true);
    assert(live.positive == 0);
    // A refusal recorded late cannot clear an answer that began after it.
    fr_cap_live_record(&live, 200, true);
    assert(live.positive == 200);
    fr_cap_live_record(&live, 120, false);
    assert(live.positive == 200 && live.refused == 150);
    fr_cap_live_record(&live, 200, false);
    assert(live.positive == 0 && live.refused == 200);
    // A check that began after the refusal stands again: access came back.
    fr_cap_live_record(&live, 201, true);
    assert(live.positive == 201);
    assert(fr_cap_live_reusable(live.positive, 201 + FR_CAP_LIVE_REUSE_NS - 1));
    // The refresher's period leaves room for one round trip in the bound.
    assert(FR_CAP_LIVE_REFRESH_NS < FR_CAP_LIVE_REUSE_NS);
}

int main(void) {
    deadline_has_exact_monotonic_boundaries();
    deadline_refuses_zero_and_overflow_without_wrapping();
    every_setup_stage_spends_the_same_original_budget();
    only_an_actual_timely_start_transfers_ownership_once();
    a_completed_start_cannot_publish_after_expiry_or_close();
    close_before_start_retires_without_fabricating_a_callback();
    late_start_success_or_failure_keeps_its_original_retirement_owner();
    either_cleanup_order_requires_both_positive_acknowledgements();
    start_failure_cannot_be_replaced_by_a_second_success_callback();
    silence_never_renews_source_evidence();
    actual_idle_verifies_only_its_original_picture();
    queued_and_future_events_cannot_fabricate_freshness();
    scheduled_display_times_are_current_within_their_lead();
    source_gaps_require_a_new_complete_picture();
    replacement_preserves_the_encoded_reference_identity();
    exhausted_identity_never_restarts();
    invalidation_fences_inflight_completion_even_after_recovery();
    new_verification_can_repair_only_an_earlier_gap();
    exhausted_invalidation_identity_never_restarts();
    each_verification_has_one_cancellable_result();
    encoded_allocation_is_bounded_before_append();
    a_positive_live_check_stands_for_at_most_its_bound();
    a_refusal_outranks_every_answer_that_began_before_it();
    puts("23 macOS capture policy tests passed");
    return 0;
}
