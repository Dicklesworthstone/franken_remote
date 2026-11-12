#ifndef FR_MACOS_AUDIO_CAPTURE_POLICY_H
#define FR_MACOS_AUDIO_CAPTURE_POLICY_H
#include <stdbool.h>
#include <stdint.h>
#include <stdatomic.h>

#define FR_AC_AGE_NS UINT64_C(100000000)
typedef struct { uint64_t began, until; } FrAcDeadline;

static inline FrAcDeadline fr_ac_deadline(uint64_t now, uint64_t allowance) {
    return (FrAcDeadline){now, now && allowance && UINT64_MAX - now >= allowance ? now + allowance : 0};
}
static inline bool fr_ac_live(FrAcDeadline deadline, uint64_t now) {
    return deadline.until && now >= deadline.began && now < deadline.until;
}

// The earlier monotonic sample brackets the actual source-clock read. Source
// age has already been conservatively rounded UP from Core Media rational time.
// An expensive clock query or PCM copy consumes this bound; it never rebases it.
static inline bool fr_ac_source_window(uint64_t before, uint64_t age_ns,
                                        FrAcDeadline *out) {
    if (!out || age_ns >= FR_AC_AGE_NS) return false;
    *out = fr_ac_deadline(before, FR_AC_AGE_NS - age_ns);
    return fr_ac_live(*out, before);
}

static inline int fr_ac_latch(atomic_int *original, int error) {
    int expected = 0;
    atomic_compare_exchange_strong(original, &expected, error);
    return atomic_load(original);
}

// Quiet audio has no callback obligation. Once a callback actually enters,
// its original deadline is independently visible to the command thread even
// if a Core Media query/copy never returns. A late/duplicate completion may
// clear only its own original slot, never a later callback's obligation.
typedef struct {
    atomic_uint_fast64_t active_until;
    atomic_uint_fast64_t last_until;
} FrAcCallback;
static inline void fr_ac_callback_init(FrAcCallback *callback) {
    atomic_init(&callback->active_until, 0);
    atomic_init(&callback->last_until, 0);
}
static inline uint64_t fr_ac_callback_begin(FrAcCallback *callback, uint64_t now) {
    FrAcDeadline window = fr_ac_deadline(now, FR_AC_AGE_NS);
    if (!fr_ac_live(window, now) || window.until <= atomic_load(&callback->last_until)) return 0;
    uint_fast64_t expected = 0;
    if (!atomic_compare_exchange_strong(&callback->active_until, &expected, window.until)) return 0;
    // Strictly increasing ticket values avoid ABA even at a repeated clock
    // reading. A repeated/regressed native clock refuses this new callback.
    atomic_store(&callback->last_until, window.until);
    return window.until;
}
static inline uint64_t fr_ac_callback_snapshot(const FrAcCallback *callback) {
    return atomic_load(&callback->active_until);
}
static inline bool fr_ac_callback_expired(const FrAcCallback *callback,
                                           uint64_t original, uint64_t now) {
    // Snapshot BEFORE reading the clock. Otherwise a new callback beginning
    // after that clock sample could be mistaken for clock reversal. The second
    // read also excludes a completed/replaced original obligation.
    FrAcDeadline window = {original >= FR_AC_AGE_NS ? original - FR_AC_AGE_NS : 0, original};
    return original && !fr_ac_live(window, now) &&
           atomic_load(&callback->active_until) == original;
}
static inline bool fr_ac_callback_complete(FrAcCallback *callback, uint64_t original) {
    if (!original) return false;
    uint_fast64_t expected = original;
    return atomic_compare_exchange_strong(&callback->active_until, &expected, 0);
}

enum FrAcStart { FR_AC_ALLOCATED, FR_AC_STARTING, FR_AC_COMPLETING, FR_AC_COMPLETED };
typedef struct {
    atomic_int stage;
    atomic_int result;
    atomic_bool closed;
    atomic_bool stop_requested;
} FrAcOpening;

static inline void fr_ac_opening_init(FrAcOpening *owner, int initial_result) {
    atomic_init(&owner->stage, FR_AC_ALLOCATED);
    atomic_init(&owner->result, initial_result);
    atomic_init(&owner->closed, false);
    atomic_init(&owner->stop_requested, false);
}
// Begin/close are called by the single opening owner, before publishing its
// handle. Only completion races them on the framework's callback thread.
static inline bool fr_ac_opening_begin(FrAcOpening *owner) {
    if (atomic_load(&owner->closed)) return false;
    int expected = FR_AC_ALLOCATED;
    return atomic_compare_exchange_strong(&owner->stage, &expected, FR_AC_STARTING);
}
static inline bool fr_ac_opening_complete(FrAcOpening *owner, int result) {
    int expected = FR_AC_STARTING;
    if (!atomic_compare_exchange_strong(&owner->stage, &expected, FR_AC_COMPLETING)) return false;
    atomic_store(&owner->result, result);
    atomic_store(&owner->stage, FR_AC_COMPLETED);
    return true;
}
static inline void fr_ac_opening_close(FrAcOpening *owner) {
    atomic_store(&owner->closed, true);
}
static inline bool fr_ac_opening_closed(const FrAcOpening *owner) {
    return atomic_load(&owner->closed);
}
static inline int fr_ac_opening_result(const FrAcOpening *owner) {
    return atomic_load(&owner->result);
}
static inline bool fr_ac_opening_needs_native_stop(const FrAcOpening *owner) {
    // Even a failed start may have acquired native resources. An NSError is
    // not proof of stop/destruction; every actual attempt requires native stop.
    return atomic_load(&owner->stage) == FR_AC_COMPLETED;
}
static inline bool fr_ac_opening_claim_stop(FrAcOpening *owner) {
    if (!fr_ac_opening_closed(owner)) return false;
    int stage = atomic_load(&owner->stage);
    if (stage != FR_AC_ALLOCATED && stage != FR_AC_COMPLETED) return false;
    bool expected = false;
    return atomic_compare_exchange_strong(&owner->stop_requested, &expected, true);
}
#endif
