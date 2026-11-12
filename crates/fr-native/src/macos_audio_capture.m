// Original ScreenCaptureKit SYSTEM playback audio in the supervised media
// worker. No microphone, screen output, input or permission prompt is opened.
// A display anchors the system/application content filter; audio is NOT
// isolated to the visible display or to a single application's windows.
#import <CoreGraphics/CoreGraphics.h>
#import <CoreMedia/CoreMedia.h>
#import <Foundation/Foundation.h>
#import <ScreenCaptureKit/ScreenCaptureKit.h>
#include "macos_audio_clock.h"
#include "macos_audio_capture_policy.h"
#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <mach/mach_time.h>
#include <os/lock.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/file.h>
#include <time.h>
#include <unistd.h>

enum {
    FR_AC_OK = 0, FR_AC_PENDING = 1,
    FR_AC_INVALID = -1, FR_AC_UNAVAILABLE = -2, FR_AC_PERMISSION = -3,
    FR_AC_ALLOCATION = -4, FR_AC_CLOCK = -5, FR_AC_EXPIRED = -6,
    FR_AC_CLOSED = -7, FR_AC_FORMAT = -8, FR_AC_LIMIT = -9,
    FR_AC_NATIVE = -10, FR_AC_BUSY = -11, FR_AC_CLEANUP = -12,
    FR_AC_SUSPENDED = -13, FR_AC_DISCONTINUITY = -14,
};
#define FR_AC_OPEN_NS UINT64_C(2000000000)
#define FR_AC_CLOSE_NS UINT64_C(500000000)
#define FR_AC_SAMPLES 4800u
#define FR_AC_VALUES (FR_AC_SAMPLES * 2u)
#define FR_AC_CHUNKS 8u

typedef struct {
    uint64_t timestamp_samples;
    uint64_t valid_until_ns;
    uint32_t frames;
    uint32_t channels;
} FrAcStamp;
typedef struct { uint32_t gaps, overruns; } FrAcCounters;
_Static_assert(sizeof(FrAcStamp) == 24, "fixed audio capture stamp");
_Static_assert(sizeof(FrAcCounters) == 8, "fixed audio capture counters");

static uint64_t monotonic_ns(void) {
    return clock_gettime_nsec_np(CLOCK_MONOTONIC); // Darwin includes sleep.
}
static BOOL live(FrAcDeadline deadline) {
    return fr_ac_live(deadline, monotonic_ns());
}
static FrAcDeadline deadline_after(uint64_t allowance) {
    return fr_ac_deadline(monotonic_ns(), allowance);
}
static BOOL await(dispatch_semaphore_t done, FrAcDeadline deadline) {
    while (live(deadline)) {
        // A short wait is followed by the original sleep-inclusive check.
        // dispatch_time itself excludes sleep and never defines our lifetime.
        if (dispatch_semaphore_wait(done, dispatch_time(DISPATCH_TIME_NOW, 2 * NSEC_PER_MSEC)) == 0)
            return live(deadline);
    }
    return NO;
}
static void increment(uint32_t *value) {
    if (*value < UINT32_MAX) ++*value;
}

// Use the SAME per-user setup lock as macos_capture.m. On qualified machines
// an unfinished content query can stall another process's stream start. This
// exact lease covers discovery AND start, including late callback custody.
@interface FrAcLease : NSObject {
  @public int descriptor;
}
@end
@implementation FrAcLease
- (void)dealloc { if (descriptor >= 0) close(descriptor); }
@end

static int acquire_setup(FrAcDeadline deadline, FrAcLease *__strong *out) {
    *out = nil;
    char directory[PATH_MAX], path[PATH_MAX];
    size_t length = confstr(_CS_DARWIN_USER_TEMP_DIR, directory, sizeof directory);
    if (!length || length > sizeof directory ||
        snprintf(path, sizeof path, "%sfr-capture-setup.lock", directory) >= (int)sizeof path)
        return FR_AC_UNAVAILABLE;
    if (!live(deadline)) return FR_AC_EXPIRED;
    int descriptor = open(path, O_RDWR | O_CREAT | O_CLOEXEC | O_NOFOLLOW, 0600);
    if (descriptor < 0) return FR_AC_UNAVAILABLE;
    while (live(deadline)) {
        if (flock(descriptor, LOCK_EX | LOCK_NB) == 0) {
            if (!live(deadline)) break;
            FrAcLease *lease = [FrAcLease new];
            if (!lease) { close(descriptor); return FR_AC_ALLOCATION; }
            lease->descriptor = descriptor;
            *out = lease;
            return FR_AC_OK;
        }
        if (errno != EWOULDBLOCK && errno != EINTR) {
            close(descriptor);
            return FR_AC_UNAVAILABLE;
        }
        struct timespec pause = {.tv_sec = 0, .tv_nsec = 2 * NSEC_PER_MSEC};
        nanosleep(&pause, NULL);
    }
    close(descriptor);
    return FR_AC_EXPIRED;
}

@interface FrAcDiscovery : NSObject {
  @public
    os_unfair_lock lock;
    BOOL accepting;
    SCShareableContent *content;
    FrAcLease *lease;
    int result;
    dispatch_semaphore_t done;
}
@end
@implementation FrAcDiscovery
@end

static int discover(FrAcDeadline deadline, FrAcLease *lease,
                    SCShareableContent *__strong *out) {
    *out = nil;
    if (!live(deadline)) return FR_AC_EXPIRED;
    BOOL allowed = CGPreflightScreenCaptureAccess();
    if (!live(deadline)) return FR_AC_EXPIRED;
    if (!allowed) return FR_AC_PERMISSION;
    FrAcDiscovery *request = [FrAcDiscovery new];
    if (!request) return FR_AC_ALLOCATION;
    request->lock = OS_UNFAIR_LOCK_INIT;
    request->accepting = YES;
    request->result = FR_AC_PERMISSION;
    request->done = dispatch_semaphore_create(0);
    request->lease = lease;
    if (!request->done) return FR_AC_ALLOCATION;
    if (!live(deadline)) return FR_AC_EXPIRED;
    [SCShareableContent getShareableContentExcludingDesktopWindows:YES
                                           onScreenWindowsOnly:YES
                                             completionHandler:^(SCShareableContent *content, NSError *error) {
        @autoreleasepool {
            __attribute__((objc_precise_lifetime)) FrAcLease *completed = nil;
            os_unfair_lock_lock(&request->lock);
            BOOL accepted = request->accepting;
            request->accepting = NO;
            if (accepted) {
                request->content = content;
                request->result = !error && content ? FR_AC_OK : FR_AC_PERMISSION;
            }
            completed = request->lease;
            request->lease = nil;
            os_unfair_lock_unlock(&request->lock);
            if (accepted) dispatch_semaphore_signal(request->done);
            (void)completed;
        }
    }];
    BOOL timely = await(request->done, deadline);
    os_unfair_lock_lock(&request->lock);
    request->accepting = NO;
    int result = timely ? request->result : FR_AC_EXPIRED;
    SCShareableContent *content = request->content;
    request->content = nil;
    os_unfair_lock_unlock(&request->lock);
    if (!live(deadline)) result = FR_AC_EXPIRED;
    if (result == FR_AC_OK) *out = content;
    return result;
}

@interface FrAcOwner : NSObject <SCStreamOutput, SCStreamDelegate> {
  @public
    os_unfair_lock lock;
    atomic_int fault;
    FrAcOpening opening;
    atomic_int stop_result;
    dispatch_semaphore_t start_done;
    dispatch_semaphore_t stop_done;
    dispatch_queue_t queue;
    SCStream *stream;
    FrAcLease *lease;
    CMClockRef source_clock;
    FrAudioSleepOffset original_sleep;
    mach_timebase_info_data_t timebase;
    CGDirectDisplayID display;
    uint32_t channels;
    BOOL output_added;
    BOOL have_pts;
    CMTime last_pts;
    CMTime last_clock;
    FrAcStamp stamps[FR_AC_CHUNKS];
    float pcm[FR_AC_CHUNKS][FR_AC_VALUES];
    uint32_t head, count, samples;
    FrAcCounters counters;
}
@end

static int fail(FrAcOwner *owner, int error) {
    int expected = FR_AC_OK;
    atomic_compare_exchange_strong(&owner->fault, &expected, error);
    return atomic_load(&owner->fault);
}

static BOOL sleep_offset(FrAcOwner *owner, FrAudioSleepOffset *out) {
    uint64_t before = mach_continuous_time();
    uint64_t awake = mach_absolute_time();
    uint64_t after = mach_continuous_time();
    return fr_audio_sleep_offset(before, awake, after, owner->timebase.numer,
                                 owner->timebase.denom, out);
}

static int current(FrAcOwner *owner) {
    int error = atomic_load(&owner->fault);
    if (error) return error;
    if (fr_ac_opening_closed(&owner->opening)) return FR_AC_CLOSED;
    FrAudioSleepOffset offset;
    if (!sleep_offset(owner, &offset)) return fail(owner, FR_AC_CLOCK);
    if (!fr_audio_sleep_unchanged(owner->original_sleep, offset))
        return fail(owner, FR_AC_SUSPENDED);
    return FR_AC_OK;
}

static void discard_oldest(FrAcOwner *owner) {
    if (!owner->count) return;
    owner->samples -= owner->stamps[owner->head].frames;
    memset(owner->pcm[owner->head], 0, sizeof owner->pcm[owner->head]);
    owner->head = (owner->head + 1) % FR_AC_CHUNKS;
    --owner->count;
    increment(&owner->counters.gaps);
}

// Source age comes from the stream's documented sample-buffer timebase, not
// callback arrival. Bracket the source-clock read with the conservative earlier
// sleep-inclusive clock, so time spent querying/copying consumes the same bound.
static int stamp_sample(FrAcOwner *owner, SCStream *original,
                         CMSampleBufferRef sample, FrAcStamp *stamp,
                         FrAcDeadline *source_window) {
    uint64_t before = monotonic_ns();
    if (!before) return FR_AC_CLOCK;
    CMClockRef clock = original.synchronizationClock;
    if (!clock) return FR_AC_CLOCK;
    CMTime now = CMClockGetTime(clock);
    CMTime pts = CMSampleBufferGetPresentationTimeStamp(sample);
    if (!CMTIME_IS_NUMERIC(now) || !CMTIME_IS_NUMERIC(pts) || now.epoch != 0 ||
        pts.epoch != 0 || pts.value < 0 || CMTimeCompare(pts, now) > 0)
        return FR_AC_CLOCK;
    CMTime age = CMTimeConvertScale(CMTimeSubtract(now, pts), 1000000000,
                                     kCMTimeRoundingMethod_RoundTowardPositiveInfinity);
    CMTime position = CMTimeConvertScale(pts, 48000, kCMTimeRoundingMethod_RoundTowardZero);
    if (!CMTIME_IS_NUMERIC(age) || age.value < 0 || !CMTIME_IS_NUMERIC(position) ||
        position.value < 0) return FR_AC_CLOCK;
    // This callback queue is serial. A new source clock or reversed actual
    // PTS is a terminal discontinuity, not a fresh audio generation.
    os_unfair_lock_lock(&owner->lock);
    BOOL changed = (owner->source_clock && owner->source_clock != clock) ||
        (owner->have_pts && (CMTimeCompare(pts, owner->last_pts) <= 0 ||
                             CMTimeCompare(now, owner->last_clock) < 0));
    if (!owner->source_clock) owner->source_clock = (CMClockRef)CFRetain(clock);
    owner->last_pts = pts;
    owner->last_clock = now;
    owner->have_pts = YES;
    os_unfair_lock_unlock(&owner->lock);
    if (changed) return FR_AC_DISCONTINUITY;
    if ((uint64_t)age.value >= FR_AC_AGE_NS) return FR_AC_EXPIRED;
    if (!fr_ac_source_window(before, (uint64_t)age.value, source_window)) return FR_AC_CLOCK;
    stamp->timestamp_samples = (uint64_t)position.value;
    stamp->valid_until_ns = source_window->until;
    return FR_AC_OK;
}

static int copy_sample(FrAcOwner *owner, CMSampleBufferRef sample, FrAcStamp *stamp,
                        float *interleaved) {
    if (!sample || !CMSampleBufferIsValid(sample) || !CMSampleBufferDataIsReady(sample))
        return FR_AC_FORMAT;
    CMAudioFormatDescriptionRef description = CMSampleBufferGetFormatDescription(sample);
    if (!description || CMFormatDescriptionGetMediaType(description) != kCMMediaType_Audio)
        return FR_AC_FORMAT;
    const AudioStreamBasicDescription *format = CMAudioFormatDescriptionGetStreamBasicDescription(description);
    if (!format || format->mFormatID != kAudioFormatLinearPCM ||
        format->mSampleRate != 48000.0 || format->mChannelsPerFrame != owner->channels ||
        format->mBitsPerChannel != 32 || format->mFramesPerPacket != 1)
        return FR_AC_FORMAT;
    AudioFormatFlags flags = format->mFormatFlags;
    BOOL planar = (flags & kAudioFormatFlagIsNonInterleaved) != 0;
    AudioFormatFlags required = kAudioFormatFlagsNativeFloatPacked |
        (planar ? kAudioFormatFlagIsNonInterleaved : 0);
    uint32_t stride = sizeof(float) * (planar ? 1 : owner->channels);
    if (flags != required || format->mBytesPerFrame != stride || format->mBytesPerPacket != stride)
        return FR_AC_FORMAT;
    CMItemCount frames = CMSampleBufferGetNumSamples(sample);
    if (frames <= 0 || frames > FR_AC_SAMPLES) return FR_AC_LIMIT;
    stamp->frames = (uint32_t)frames;
    stamp->channels = owner->channels;
    // The framework copies into application-owned, bounded PCM storage. No
    // CMSampleBuffer/CMBlockBuffer or borrowed AudioBufferList escapes callback.
    float scratch[FR_AC_VALUES];
    struct { UInt32 mNumberBuffers; AudioBuffer mBuffers[2]; } buffers = {0};
    buffers.mNumberBuffers = planar ? owner->channels : 1;
    for (uint32_t index = 0; index < buffers.mNumberBuffers; ++index) {
        buffers.mBuffers[index].mNumberChannels = planar ? 1 : owner->channels;
        buffers.mBuffers[index].mDataByteSize = (UInt32)frames * stride;
        buffers.mBuffers[index].mData = scratch + (planar ? index * (uint32_t)frames : 0);
    }
    if (CMSampleBufferCopyPCMDataIntoAudioBufferList(sample, 0, (int32_t)frames,
                                                    (AudioBufferList *)&buffers) != noErr)
        return FR_AC_NATIVE;
    for (uint32_t frame = 0; frame < (uint32_t)frames; ++frame) {
        for (uint32_t channel = 0; channel < owner->channels; ++channel) {
            interleaved[frame * owner->channels + channel] =
                scratch[planar ? channel * (uint32_t)frames + frame : frame * owner->channels + channel];
        }
    }
    return FR_AC_OK;
}

@implementation FrAcOwner
- (void)stream:(SCStream *)original didOutputSampleBuffer:(CMSampleBufferRef)sample
        ofType:(SCStreamOutputType)type {
    @autoreleasepool {
        if (current(self) != FR_AC_OK) return;
        if (original != stream || type != SCStreamOutputTypeAudio) {
            fail(self, FR_AC_FORMAT);
            return;
        }
        FrAcStamp stamp = {0};
        FrAcDeadline source_window = {0};
        int result = sample && CMSampleBufferIsValid(sample)
            ? stamp_sample(self, original, sample, &stamp, &source_window) : FR_AC_FORMAT;
        float copied[FR_AC_VALUES];
        if (!result) result = copy_sample(self, sample, &stamp, copied);
        if (!result) result = current(self);
        uint64_t now = monotonic_ns();
        if (!now) result = FR_AC_CLOCK;
        if (!result && now < source_window.began) result = FR_AC_CLOCK;
        if (!result && !fr_ac_live(source_window, now)) result = FR_AC_EXPIRED;
        if (result && result != FR_AC_EXPIRED) { fail(self, result); return; }
        os_unfair_lock_lock(&lock);
        if (fr_ac_opening_closed(&opening) || atomic_load(&fault)) {
            os_unfair_lock_unlock(&lock);
            return;
        }
        if (result == FR_AC_EXPIRED) {
            increment(&counters.gaps);
        } else {
            BOOL overflow = NO;
            while (count && (count == FR_AC_CHUNKS || samples + stamp.frames > FR_AC_SAMPLES)) {
                discard_oldest(self);
                overflow = YES;
            }
            if (overflow) increment(&counters.overruns);
            uint32_t index = (head + count) % FR_AC_CHUNKS;
            stamps[index] = stamp;
            memcpy(pcm[index], copied, (size_t)stamp.frames * channels * sizeof(float));
            samples += stamp.frames;
            ++count;
        }
        os_unfair_lock_unlock(&lock);
    }
}
- (void)stream:(SCStream *)original didStopWithError:(NSError *)error {
    (void)original; // This delegate belongs to exactly one immutable stream.
    (void)error;
    if (!fr_ac_opening_closed(&opening)) fail(self, FR_AC_NATIVE);
}
- (void)dealloc {
    if (source_clock) CFRelease(source_clock);
}
@end

// At most one stop request. A timeout during start retains this owner and its
// setup lease; the original start completion starts the stop, never a retry.
static void stop_original(FrAcOwner *owner) {
    if (!fr_ac_opening_claim_stop(&owner->opening)) return;
    if (!fr_ac_opening_needs_native_stop(&owner->opening)) {
        atomic_store(&owner->stop_result, FR_AC_OK);
        dispatch_semaphore_signal(owner->stop_done);
        return;
    }
    [owner->stream stopCaptureWithCompletionHandler:^(NSError *error) {
        atomic_store(&owner->stop_result, error ? FR_AC_CLEANUP : FR_AC_OK);
        dispatch_semaphore_signal(owner->stop_done);
    }];
}

int fr_ac_close(void *raw) {
    if (!raw) return FR_AC_INVALID;
    @autoreleasepool {
        FrAcOwner *owner = (__bridge FrAcOwner *)raw;
        fr_ac_opening_close(&owner->opening);
        FrAcDeadline deadline = deadline_after(FR_AC_CLOSE_NS);
        os_unfair_lock_lock(&owner->lock);
        memset(owner->pcm, 0, sizeof owner->pcm);
        owner->count = owner->samples = 0;
        os_unfair_lock_unlock(&owner->lock);
        @try {
            stop_original(owner);
            if (!await(owner->stop_done, deadline) || atomic_load(&owner->stop_result) != FR_AC_OK)
                return FR_AC_CLEANUP;
            NSError *error = nil;
            if (owner->output_added &&
                ![owner->stream removeStreamOutput:owner type:SCStreamOutputTypeAudio error:&error])
                return FR_AC_CLEANUP;
            if (error || !live(deadline)) return FR_AC_CLEANUP;
            // Serial callback barrier after removal. Late callbacks see closing
            // before touching a sample; actual completion precedes release.
            dispatch_semaphore_t drained = dispatch_semaphore_create(0);
            if (!drained) return FR_AC_CLEANUP;
            dispatch_async(owner->queue, ^{ dispatch_semaphore_signal(drained); });
            if (!await(drained, deadline)) return FR_AC_CLEANUP;
            owner->stream = nil;
            owner->lease = nil;
            (void)CFBridgingRelease(raw);
            return FR_AC_OK;
        } @catch (NSException *exception) {
            (void)exception;
            // Keep the sole native retain on every uncertain cleanup path.
            // The parent must kill/reap this process; nothing reuses the owner.
            return FR_AC_CLEANUP;
        }
    }
}

int fr_ac_open(uint32_t channels, void **out) {
    if (!out || (channels != 1 && channels != 2)) return FR_AC_INVALID;
    *out = NULL;
    if (@available(macOS 13.0, *)) {
      @autoreleasepool {
        FrAcDeadline deadline = deadline_after(FR_AC_OPEN_NS);
        FrAcLease *lease = nil;
        SCShareableContent *content = nil;
        int result = acquire_setup(deadline, &lease);
        if (!result) result = discover(deadline, lease, &content);
        if (result) return result;
        CGDirectDisplayID selected = CGMainDisplayID();
        SCDisplay *anchor = nil;
        for (SCDisplay *candidate in content.displays) {
            if (candidate.displayID == selected) { anchor = candidate; break; }
        }
        if (!anchor || !live(deadline)) return anchor ? FR_AC_EXPIRED : FR_AC_UNAVAILABLE;
        FrAcOwner *owner = [FrAcOwner new];
        if (!owner) return FR_AC_ALLOCATION;
        owner->lock = OS_UNFAIR_LOCK_INIT;
        atomic_init(&owner->fault, FR_AC_OK);
        fr_ac_opening_init(&owner->opening, FR_AC_UNAVAILABLE);
        atomic_init(&owner->stop_result, FR_AC_CLEANUP);
        owner->channels = channels;
        owner->display = selected;
        owner->lease = lease;
        owner->start_done = dispatch_semaphore_create(0);
        owner->stop_done = dispatch_semaphore_create(0);
        owner->queue = dispatch_queue_create("fr.system-audio", DISPATCH_QUEUE_SERIAL);
        if (!owner->start_done || !owner->stop_done || !owner->queue ||
            mach_timebase_info(&owner->timebase) != KERN_SUCCESS ||
            !sleep_offset(owner, &owner->original_sleep)) return FR_AC_ALLOCATION;
        // Native custody exists before registering an output or starting any
        // asynchronous operation. Every later failure goes through close.
        void *retained = (__bridge_retained void *)owner;
        @try {
            SCContentFilter *filter = [[SCContentFilter alloc]
                initWithDisplay:anchor excludingApplications:@[] exceptingWindows:@[]];
            SCStreamConfiguration *configuration = [SCStreamConfiguration new];
            configuration.width = 2;
            configuration.height = 2;
            configuration.queueDepth = 3;
            configuration.minimumFrameInterval = CMTimeMake(1, 1);
            configuration.showsCursor = NO;
            configuration.capturesAudio = YES;
            configuration.sampleRate = 48000;
            configuration.channelCount = channels;
            configuration.excludesCurrentProcessAudio = YES;
            // No screen or microphone output is registered. Microphone capture
            // remains its default disabled state, including on newer systems.
            if (!filter || !configuration) result = FR_AC_ALLOCATION;
            if (!result) {
                owner->stream = [[SCStream alloc] initWithFilter:filter configuration:configuration delegate:owner];
                if (!owner->stream) result = FR_AC_ALLOCATION;
            }
            NSError *error = nil;
            if (!result && !live(deadline)) result = FR_AC_EXPIRED;
            if (!result) {
                owner->output_added = [owner->stream addStreamOutput:owner type:SCStreamOutputTypeAudio
                                                   sampleHandlerQueue:owner->queue error:&error];
                if (!owner->output_added || error) result = FR_AC_NATIVE;
            }
            if (!result && !live(deadline)) result = FR_AC_EXPIRED;
            if (!result) {
                if (!fr_ac_opening_begin(&owner->opening)) result = FR_AC_CLOSED;
                else {
                    [owner->stream startCaptureWithCompletionHandler:^(NSError *start_error) {
                        if (fr_ac_opening_complete(&owner->opening,
                                                   start_error ? FR_AC_NATIVE : FR_AC_OK)) {
                            dispatch_semaphore_signal(owner->start_done);
                            stop_original(owner);
                        }
                    }];
                    result = await(owner->start_done, deadline)
                        ? fr_ac_opening_result(&owner->opening) : FR_AC_EXPIRED;
                }
            }
            if (!result) result = current(owner);
            if (!result && (!owner->stream.synchronizationClock || !CGDisplayIsActive(selected)))
                result = FR_AC_UNAVAILABLE;
            if (!result && !live(deadline)) result = FR_AC_EXPIRED;
        } @catch (NSException *exception) {
            (void)exception;
            result = FR_AC_NATIVE;
        }
        if (result) {
            (void)fr_ac_close(retained);
            return result;
        }
        owner->lease = nil;
        *out = retained;
        return FR_AC_OK;
      }
    }
    return FR_AC_UNAVAILABLE;
}

int fr_ac_check(void *raw) {
    if (!raw) return FR_AC_INVALID;
    @autoreleasepool {
        FrAcOwner *owner = (__bridge FrAcOwner *)raw;
        int result = current(owner);
        if (result) return result;
        // Nonprompting revocation evidence is checked at each actual native
        // read and each retained-frame publication gate, independently of the
        // stream's asynchronous failure notification. Denial is one-way.
        if (!CGPreflightScreenCaptureAccess()) return fail(owner, FR_AC_PERMISSION);
        if (!CGDisplayIsActive(owner->display)) return fail(owner, FR_AC_UNAVAILABLE);
        return current(owner);
    }
}

int fr_ac_read(void *raw, float *samples, size_t capacity,
                FrAcStamp *stamp, FrAcCounters *counters) {
    if (!raw || !samples || capacity < FR_AC_VALUES || !stamp || !counters) return FR_AC_INVALID;
    *stamp = (FrAcStamp){0};
    @autoreleasepool {
        FrAcOwner *owner = (__bridge FrAcOwner *)raw;
        int result = current(owner);
        os_unfair_lock_lock(&owner->lock);
        *counters = owner->counters;
        if (!result) {
            uint64_t now = monotonic_ns();
            if (!now) result = FR_AC_CLOCK;
            while (!result && owner->count && now >= owner->stamps[owner->head].valid_until_ns)
                discard_oldest(owner);
            if (!result && owner->count) {
                *stamp = owner->stamps[owner->head];
                memcpy(samples, owner->pcm[owner->head],
                       (size_t)stamp->frames * stamp->channels * sizeof(float));
                memset(owner->pcm[owner->head], 0, sizeof owner->pcm[owner->head]);
                owner->samples -= stamp->frames;
                owner->head = (owner->head + 1) % FR_AC_CHUNKS;
                --owner->count;
            } else if (!result) {
                result = FR_AC_PENDING;
            }
            *counters = owner->counters;
        }
        os_unfair_lock_unlock(&owner->lock);
        if (result == FR_AC_CLOCK) return fail(owner, result);
        int after = current(owner);
        return after ? after : result;
    }
}
