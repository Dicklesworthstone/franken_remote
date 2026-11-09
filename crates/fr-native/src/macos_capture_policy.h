// Pure policy used by the ScreenCaptureKit adapter and its portable tests.
// These transitions consume native metadata; they do not synthesize OS events.
#ifndef FR_MACOS_CAPTURE_POLICY_H
#define FR_MACOS_CAPTURE_POLICY_H

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

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
static inline enum FrCapSourceUpdate fr_cap_source_observe(
    FrCapSource *source, enum FrCapSourceEvent event, uint64_t displayed, uint64_t received) {
    if (source->exhausted) return FR_CAP_SOURCE_INVALIDATED;
    if (event == FR_CAP_SOURCE_INVALID || displayed == 0 || displayed > received) {
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

#endif
