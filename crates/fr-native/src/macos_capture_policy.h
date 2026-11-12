// Pure policy used by the ScreenCaptureKit adapter and its portable tests.
// These transitions consume native metadata; they do not synthesize OS events.
#ifndef FR_MACOS_CAPTURE_POLICY_H
#define FR_MACOS_CAPTURE_POLICY_H

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

// A pending rate never creates capture work. First decide from the existing
// positive source evidence whether an encode is required; only that encode may
// change the rate, and a changed rate requires a fresh independent picture.
typedef struct {
    bool encode;
    bool change_rate;
    bool force_idr;
} FrCapEncodePlan;

static inline FrCapEncodePlan fr_cap_encode_plan(bool verification, bool only_if_changed,
                                                bool force_idr, bool same_picture,
                                                uint32_t applied, uint32_t target) {
    bool encode = verification || !only_if_changed || force_idr || !same_picture;
    bool change_rate = encode && target != applied;
    return (FrCapEncodePlan){encode, change_rate, force_idr || change_rate};
}

// All native-open stages share this original monotonic interval. A stage may
// consume its remainder; neither a successful callback nor a retry renews it.
typedef struct {
    uint64_t began;
    uint64_t until;
} FrCapDeadline;

static inline bool fr_cap_deadline_init(FrCapDeadline *deadline, uint64_t now,
                                         uint64_t budget) {
    deadline->began = now;
    deadline->until = now;
    if (budget == 0 || budget > UINT64_MAX - now) return false;
    deadline->until = now + budget;
    return true;
}

static inline bool fr_cap_deadline_live(const FrCapDeadline *deadline, uint64_t now) {
    return now >= deadline->began && now < deadline->until;
}

static inline uint64_t fr_cap_deadline_remaining(const FrCapDeadline *deadline, uint64_t now) {
    return fr_cap_deadline_live(deadline, now) ? deadline->until - now : 0;
}

// A start completion can outlive its caller. Closing admission keeps that
// original operation owned until its actual start result is known, then until
// BOTH the stream/output path and the encoder have positively retired. Only
// one caller may claim native retirement or release the owner's resources.
typedef struct {
    bool accepting;
    bool requested;
    bool completed;
    bool started;
    bool retiring;
    bool codec_retired;
    bool stream_retired;
    bool published;
    bool released;
} FrCapOpeningPolicy;

static inline bool fr_cap_opening_request(FrCapOpeningPolicy *opening) {
    if (!opening->accepting || opening->requested || opening->retiring || opening->published)
        return false;
    opening->requested = true;
    return true;
}

static inline bool fr_cap_opening_complete(FrCapOpeningPolicy *opening, bool started) {
    if (!opening->requested || opening->completed || opening->published) return false;
    opening->completed = true;
    opening->started = started;
    return true;
}

static inline void fr_cap_opening_close(FrCapOpeningPolicy *opening) {
    opening->accepting = false;
}

static inline bool fr_cap_opening_retire(FrCapOpeningPolicy *opening) {
    if (opening->accepting || opening->published || opening->retiring ||
        (opening->requested && !opening->completed))
        return false;
    opening->retiring = true;
    return true;
}

static inline bool fr_cap_opening_publish(FrCapOpeningPolicy *opening,
                                          const FrCapDeadline *deadline, uint64_t now) {
    if (!fr_cap_deadline_live(deadline, now) || !opening->accepting || !opening->requested ||
        !opening->completed || !opening->started || opening->retiring || opening->published)
        return false;
    opening->accepting = false;
    opening->published = true;
    return true;
}

static inline bool fr_cap_opening_release(FrCapOpeningPolicy *opening) {
    if (opening->released ||
        (!opening->published &&
         !(opening->retiring && opening->codec_retired && opening->stream_retired)))
        return false;
    opening->released = true;
    return true;
}

enum FrCapSourceEvent {
    FR_CAP_SOURCE_INVALID = 0,
    FR_CAP_SOURCE_COMPLETE = 1,
    FR_CAP_SOURCE_IDLE = 2,
};

enum FrCapSourceUpdate {
    FR_CAP_SOURCE_IGNORED = 0,
    FR_CAP_SOURCE_REPLACED = 1,
    FR_CAP_SOURCE_VERIFIED = 2,
    FR_CAP_SOURCE_INVALIDATED = 3,
};

typedef struct {
    uint64_t picture;
    uint64_t observed;
    uint64_t floor;
    uint64_t invalidation;
    bool available;
    bool exhausted;
} FrCapSource;

// Invalid/blank/suspended/started/stopped samples discard picture evidence.
// Callback receipt time is a rejection barrier, NEVER a source observation.
static inline void fr_cap_source_invalidate(FrCapSource *source, uint64_t received) {
    source->available = false;
    source->observed = 0;
    if (source->invalidation == UINT64_MAX) {
        source->exhausted = true;
    } else {
        source->invalidation++;
    }
    if (received > source->floor) source->floor = received;
}

// Display times and received are both mach absolute ticks. A delayed sample
// cannot become current merely because its callback finally ran. Idle verifies
// an existing complete picture; it never creates a picture after a gap.
//
// A display time is the frame's scheduled presentation, so ScreenCaptureKit
// delivers a completed frame BEFORE it: on a 60 Hz 5K display (macOS 27) every
// Complete frame arrived 0.07-7.5 ms ahead of its display time. `lead` bounds
// that schedule; a time further ahead of receipt is invalid metadata.
static inline enum FrCapSourceUpdate fr_cap_source_observe(FrCapSource *source,
                                                           enum FrCapSourceEvent event,
                                                           uint64_t displayed, uint64_t received,
                                                           uint64_t lead) {
    if (source->exhausted) return FR_CAP_SOURCE_INVALIDATED;
    uint64_t latest = received > UINT64_MAX - lead ? UINT64_MAX : received + lead;
    if (event == FR_CAP_SOURCE_INVALID || displayed == 0 || displayed > latest) {
        fr_cap_source_invalidate(source, received);
        return FR_CAP_SOURCE_INVALIDATED;
    }
    if (displayed <= source->floor) return FR_CAP_SOURCE_IGNORED;
    source->floor = displayed;
    if (event == FR_CAP_SOURCE_IDLE) {
        if (!source->available) return FR_CAP_SOURCE_IGNORED;
        source->observed = displayed;
        return FR_CAP_SOURCE_VERIFIED;
    }
    if (event != FR_CAP_SOURCE_COMPLETE || source->picture == UINT64_MAX) {
        source->exhausted = true;
        fr_cap_source_invalidate(source, received);
        return FR_CAP_SOURCE_INVALIDATED;
    }
    source->picture++;
    source->available = true;
    source->observed = displayed;
    return FR_CAP_SOURCE_REPLACED;
}

// A broker request carries a pre-request lower-bound timestamp. It can only
// describe source work positively observed at or after this native call began.
static inline bool fr_cap_source_ready(const FrCapSource *source, uint64_t requested) {
    return !source->exhausted && source->available && source->observed >= requested;
}

// Sample this generation with a selected stream picture or immediately before
// a one-shot capture starts. A later invalidation fences that operation even
// if another complete picture has since repaired the stream. Availability is
// deliberately not required: a new one-shot can repair an earlier source gap.
static inline bool fr_cap_source_current(const FrCapSource *source, uint64_t invalidation) {
    return !source->exhausted && source->invalidation == invalidation;
}

// The caller and callback share this policy under their short native lock.
// Closing admission is not proof that an uninterruptible callback has stopped.
typedef struct {
    bool accepting;
    bool completed;
} FrCapOneShot;

static inline bool fr_cap_one_shot_complete(FrCapOneShot *request) {
    if (!request->accepting || request->completed) return false;
    request->completed = true;
    return true;
}

static inline void fr_cap_one_shot_close(FrCapOneShot *request) {
    request->accepting = false;
}

// Canonical NAL growth is checked before allocation/copy, including every
// four-byte length. A payload must be representable in that wire prefix.
static inline bool fr_cap_nal_fits(size_t current, size_t payload, size_t capacity) {
    return payload != 0 && payload <= UINT32_MAX && current <= capacity && capacity - current >= 4 &&
           payload <= capacity - current - 4;
}

// Screen Recording access and the display's mode/placement are synchronous
// round trips to tccd and WindowServer (measured ~20 ms per picture at three
// checks each). A positive answer younger than this bound stands for the
// next checks; a refusal is never kept, and the stream's own stop is checked
// on every call. So revocation or a mode change is seen within 100 ms.
#define FR_CAP_LIVE_REUSE_NS 100000000ull

// `checked` is the monotonic time of the last positive native check, or 0.
static inline bool fr_cap_live_reusable(uint64_t checked, uint64_t now) {
    return checked != 0 && now >= checked && now - checked < FR_CAP_LIVE_REUSE_NS;
}

// A background refresher and the requests themselves both check, and both
// record each answer under the capture lock, stamped before its calls.
// `positive` is the stamp of the newest positive answer, or 0. A refusal
// clears every positive answer stamped at or before it, and a positive answer
// that finishes later can never restore one of those. A newer positive answer
// (its calls began after the refusal's) may stand again: access was given back.
typedef struct {
    uint64_t positive;
    uint64_t refused;
} FrCapLive;

static inline void fr_cap_live_record(FrCapLive *live, uint64_t stamp, bool positive) {
    if (!positive) {
        if (stamp > live->refused) live->refused = stamp;
        if (live->positive <= live->refused) live->positive = 0;
    } else if (stamp != 0 && stamp > live->refused && stamp > live->positive) {
        live->positive = stamp;
    }
}

// How often the refresher asks. A positive answer is then at most this old
// plus one round trip when a request reads it, inside FR_CAP_LIVE_REUSE_NS
// unless tccd or WindowServer answer slower than the difference; the request
// then checks for itself, as before.
#define FR_CAP_LIVE_REFRESH_NS 50000000ull

#endif
