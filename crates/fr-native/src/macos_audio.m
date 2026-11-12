// Core Audio output only. No codec, capture, input, authority endpoint or UI.
// One worker thread owns every API call; Audio Queue callbacks only release a
// fixed buffer slot. A returned buffer is NOT a consumed-sample clock.
#import <AudioToolbox/AudioToolbox.h>
#import <CoreAudio/CoreAudio.h>
#include <mach/mach_time.h>
#include "macos_audio_clock.h"
#include <stdatomic.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <math.h>

enum {
    FR_AUDIO_OK = 0, FR_AUDIO_PENDING = 1,
    FR_AUDIO_CONFIG = -1, FR_AUDIO_UNAVAILABLE = -2,
    FR_AUDIO_ALLOC = -3, FR_AUDIO_CLOCK = -4,
    FR_AUDIO_CHANGED = -5, FR_AUDIO_BOUNDS = -6,
    FR_AUDIO_BUSY = -7, FR_AUDIO_NATIVE = -8,
    FR_AUDIO_DENIED = -9, FR_AUDIO_UNKNOWN = -10,
    FR_AUDIO_CLOSED = -11
};
#define FR_AUDIO_BUFFERS 3
#define FR_AUDIO_MAX_OWNERS 4
#define FR_AUDIO_MAX_FRAMES 961
#define FR_AUDIO_QUEUE_FRAMES 1920

typedef struct {
    uint64_t samples;
    uint64_t lead;
} FrAudioClock;

typedef struct {
    AudioQueueRef queue;
    AudioQueueTimelineRef timeline;
    AudioDeviceID device;
    CFStringRef uid;
    Float64 device_rate;
    UInt32 device_buffer;
    UInt32 device_latency;
    UInt32 safety_offset;
    UInt32 stream_count;
    AudioStreamID streams[8];
    UInt32 stream_latencies[8];
    mach_timebase_info_data_t timebase;
    FrAudioSleepOffset original_sleep_offset;
    uint32_t channels;
    uint32_t buffer_bytes;
    uint64_t lead;
    uint64_t submitted_end;
    AudioQueueBufferRef buffers[FR_AUDIO_BUFFERS];
    atomic_bool busy[FR_AUDIO_BUFFERS];
    atomic_bool closed;
} FrAudioOutput;

// Failed disposal retains callback storage AND its owner credit. Repeated
// native failures cannot allocate unbounded orphan Audio Queues.
static atomic_uint owners = 0;

static bool sleep_offset(FrAudioOutput *owner, FrAudioSleepOffset *out) {
    uint64_t before = mach_continuous_time();
    uint64_t awake = mach_absolute_time();
    uint64_t after = mach_continuous_time();
    return fr_audio_sleep_offset(before, awake, after,
        owner->timebase.numer, owner->timebase.denom, out);
}

static bool sleep_unchanged(FrAudioOutput *owner) {
    FrAudioSleepOffset current;
    return sleep_offset(owner, &current) &&
           fr_audio_sleep_unchanged(owner->original_sleep_offset, current);
}

static int property(AudioObjectID object, AudioObjectPropertySelector selector,
                    AudioObjectPropertyScope scope, void *out, UInt32 size) {
    AudioObjectPropertyAddress address = {
        selector, scope, kAudioObjectPropertyElementMain
    };
    UInt32 actual = size;
    if (AudioObjectGetPropertyData(object, &address, 0, NULL, &actual, out) != noErr ||
        actual != size) return FR_AUDIO_UNAVAILABLE;
    return FR_AUDIO_OK;
}

static int current_device(AudioDeviceID *device) {
    return property(kAudioObjectSystemObject, kAudioHardwarePropertyDefaultOutputDevice,
                    kAudioObjectPropertyScopeGlobal, device, sizeof *device);
}

static int device_properties(FrAudioOutput *owner, bool initial) {
    AudioDeviceID current = kAudioObjectUnknown;
    UInt32 alive = 0, buffer = 0, latency = 0, safety = 0;
    Float64 rate = 0;
    if (current_device(&current) != FR_AUDIO_OK || current != owner->device ||
        property(owner->device, kAudioDevicePropertyDeviceIsAlive,
                 kAudioObjectPropertyScopeGlobal, &alive, sizeof alive) != FR_AUDIO_OK || !alive ||
        property(owner->device, kAudioDevicePropertyNominalSampleRate,
                 kAudioObjectPropertyScopeGlobal, &rate, sizeof rate) != FR_AUDIO_OK ||
        property(owner->device, kAudioDevicePropertyBufferFrameSize,
                 kAudioObjectPropertyScopeGlobal, &buffer, sizeof buffer) != FR_AUDIO_OK ||
        !isfinite(rate) || rate < 8000 || rate > 384000 || buffer == 0)
        return FR_AUDIO_CHANGED;
    if (!initial && (rate != owner->device_rate || buffer != owner->device_buffer))
        return FR_AUDIO_CHANGED;
    if (property(owner->device, kAudioDevicePropertyLatency,
                 kAudioDevicePropertyScopeOutput, &latency, sizeof latency) != FR_AUDIO_OK ||
        property(owner->device, kAudioDevicePropertySafetyOffset,
                 kAudioDevicePropertyScopeOutput, &safety, sizeof safety) != FR_AUDIO_OK)
        return FR_AUDIO_UNAVAILABLE;
    // Output streams run concurrently. Account for the largest stream latency;
    // refuse an unbounded/unknown device graph rather than assuming zero delay.
    AudioStreamID streams[8];
    UInt32 bytes = sizeof streams;
    AudioObjectPropertyAddress address = {
        kAudioDevicePropertyStreams, kAudioDevicePropertyScopeOutput,
        kAudioObjectPropertyElementMain
    };
    if (AudioObjectGetPropertyData(owner->device, &address, 0, NULL, &bytes, streams) != noErr ||
        bytes == 0 || bytes > sizeof streams || bytes % sizeof streams[0] != 0)
        return FR_AUDIO_UNAVAILABLE;
    UInt32 stream_latency = 0;
    UInt32 stream_latencies[8] = {0};
    for (UInt32 i = 0; i < bytes / sizeof streams[0]; ++i) {
        UInt32 value = 0;
        if (property(streams[i], kAudioStreamPropertyLatency,
                     kAudioObjectPropertyScopeGlobal, &value, sizeof value) != FR_AUDIO_OK)
            return FR_AUDIO_UNAVAILABLE;
        if (value > stream_latency) stream_latency = value;
        stream_latencies[i] = value;
    }
    uint64_t frames = (uint64_t)buffer + latency + safety + stream_latency;
    double lead = ceil((double)frames * 48000.0 / rate) + 240.0;
    // Include the largest drift-corrected 20 ms frame inside the 40 ms bound.
    // A high-latency device (including many wireless outputs) is refused.
    if (!isfinite(lead) || lead < 240.0 ||
        lead > FR_AUDIO_QUEUE_FRAMES - FR_AUDIO_MAX_FRAMES)
        return FR_AUDIO_BOUNDS;
    UInt32 stream_count = bytes / sizeof streams[0];
    if (!initial) {
        // The device can change its safety/presentation latency or stream graph
        // without changing nominal rate or buffer size. Never reuse old timing.
        if (latency != owner->device_latency || safety != owner->safety_offset ||
            stream_count != owner->stream_count ||
            memcmp(streams, owner->streams, bytes) != 0 ||
            memcmp(stream_latencies, owner->stream_latencies,
                   stream_count * sizeof stream_latencies[0]) != 0)
            return FR_AUDIO_CHANGED;
        CFStringRef routed = NULL;
        UInt32 uid_size = sizeof routed;
        OSStatus status = AudioQueueGetProperty(owner->queue,
            kAudioQueueProperty_CurrentDevice, &routed, &uid_size);
        bool pinned = status == noErr && uid_size == sizeof routed && routed &&
                      CFEqual(routed, owner->uid);
        // AudioQueueGetProperty copies Core Foundation values despite its name.
        if (routed) CFRelease(routed);
        return pinned ? FR_AUDIO_OK : FR_AUDIO_CHANGED;
    }
    owner->device_rate = rate;
    owner->device_buffer = buffer;
    owner->device_latency = latency;
    owner->safety_offset = safety;
    owner->stream_count = stream_count;
    memcpy(owner->streams, streams, bytes);
    memcpy(owner->stream_latencies, stream_latencies,
           stream_count * sizeof stream_latencies[0]);
    owner->lead = (uint64_t)lead;
    return FR_AUDIO_OK;
}

static void released(void *context, AudioQueueRef queue, AudioQueueBufferRef buffer) {
    FrAudioOutput *owner = context;
    (void)queue;
    for (unsigned i = 0; i < FR_AUDIO_BUFFERS; ++i) {
        if (owner->buffers[i] == buffer) {
            // Audio Queue acquired the contents; it no longer borrows this
            // allocation. No audio, allocation, lock or application callback.
            memset(buffer->mAudioData, 0, buffer->mAudioDataBytesCapacity);
            atomic_store_explicit(&owner->busy[i], false, memory_order_release);
            return;
        }
    }
}

// On failure the state stays allocated because callbacks may still refer to it.
// Rust retains a false retirement witness and never retries an ambiguous native
// disposal; a subsequent owner cannot reuse this quarantined state.
int fr_macos_audio_destroy(void *handle) {
    FrAudioOutput *owner = handle;
    if (!owner) return FR_AUDIO_OK;
    atomic_store_explicit(&owner->closed, true, memory_order_release);
    if (owner->queue) {
        // Both calls affect only this stream. Stop is immediate, never a drain
        // of obsolete sound; Dispose(true) joins the native callback lifetime.
        (void)AudioQueueSetParameter(owner->queue, kAudioQueueParam_Volume, 0);
        (void)AudioQueueStop(owner->queue, true);
        // Disposing the queue also disposes its timelines. A separate timeline
        // failure must never prevent retiring the queue and its callbacks.
        if (AudioQueueDispose(owner->queue, true) != noErr) return FR_AUDIO_NATIVE;
        owner->queue = NULL;
        owner->timeline = NULL;
    }
    if (owner->uid) CFRelease(owner->uid);
    free(owner);
    atomic_fetch_sub_explicit(&owners, 1, memory_order_acq_rel);
    return FR_AUDIO_OK;
}

int fr_macos_audio_create(uint32_t channels, uint32_t largest_frames, void **out) {
    if (!out) return FR_AUDIO_CONFIG;
    *out = NULL;
    if ((channels != 1 && channels != 2) || largest_frames < 2 ||
        largest_frames > FR_AUDIO_MAX_FRAMES) return FR_AUDIO_CONFIG;
    unsigned count = atomic_load_explicit(&owners, memory_order_acquire);
    do {
        if (count >= FR_AUDIO_MAX_OWNERS) return FR_AUDIO_BUSY;
    } while (!atomic_compare_exchange_weak_explicit(&owners, &count, count + 1,
                 memory_order_acq_rel, memory_order_acquire));
    FrAudioOutput *owner = calloc(1, sizeof *owner);
    if (!owner) {
        atomic_fetch_sub_explicit(&owners, 1, memory_order_acq_rel);
        return FR_AUDIO_ALLOC;
    }
    atomic_init(&owner->closed, false);
    for (unsigned i = 0; i < FR_AUDIO_BUFFERS; ++i) atomic_init(&owner->busy[i], false);
    *out = owner; // caller owns cleanup even if a later native call fails
    if (mach_timebase_info(&owner->timebase) != KERN_SUCCESS ||
        !sleep_offset(owner, &owner->original_sleep_offset)) return FR_AUDIO_CLOCK;
    owner->channels = channels;
    owner->buffer_bytes = largest_frames * channels * sizeof(int16_t);
    if (current_device(&owner->device) != FR_AUDIO_OK || owner->device == kAudioObjectUnknown)
        return FR_AUDIO_UNAVAILABLE;
    int checked = device_properties(owner, true);
    if (checked != FR_AUDIO_OK) return checked;
    if (property(owner->device, kAudioDevicePropertyDeviceUID,
                 kAudioObjectPropertyScopeGlobal, &owner->uid, sizeof owner->uid) != FR_AUDIO_OK ||
        !owner->uid) return FR_AUDIO_UNAVAILABLE;
    AudioStreamBasicDescription format = {0};
    format.mSampleRate = 48000;
    format.mFormatID = kAudioFormatLinearPCM;
    format.mFormatFlags = kAudioFormatFlagIsSignedInteger | kAudioFormatFlagIsPacked |
                          kAudioFormatFlagsNativeEndian;
    format.mBytesPerPacket = channels * sizeof(int16_t);
    format.mFramesPerPacket = 1;
    format.mBytesPerFrame = channels * sizeof(int16_t);
    format.mChannelsPerFrame = channels;
    format.mBitsPerChannel = 16;
    if (AudioQueueNewOutput(&format, released, owner, NULL, NULL, 0, &owner->queue) != noErr ||
        !owner->queue) return FR_AUDIO_UNAVAILABLE;
    if (AudioQueueSetProperty(owner->queue, kAudioQueueProperty_CurrentDevice,
                              &owner->uid, sizeof owner->uid) != noErr ||
        AudioQueueCreateTimeline(owner->queue, &owner->timeline) != noErr)
        return FR_AUDIO_UNAVAILABLE;
    for (unsigned i = 0; i < FR_AUDIO_BUFFERS; ++i) {
        if (AudioQueueAllocateBuffer(owner->queue, owner->buffer_bytes, &owner->buffers[i]) != noErr ||
            !owner->buffers[i]) return FR_AUDIO_ALLOC;
    }
    // Silent startup only, enough to exercise the real queue clock. Never count
    // this as remote PCM submission, a configured decoder, or observed sound.
    AudioQueueBufferRef silence = owner->buffers[0];
    silence->mAudioDataByteSize = 240 * channels * sizeof(int16_t);
    memset(silence->mAudioData, 0, silence->mAudioDataByteSize);
    atomic_store_explicit(&owner->busy[0], true, memory_order_release);
    if (AudioQueueEnqueueBuffer(owner->queue, silence, 0, NULL) != noErr ||
        AudioQueueStart(owner->queue, NULL) != noErr) return FR_AUDIO_UNAVAILABLE;
    owner->submitted_end = 240;
    return FR_AUDIO_OK;
}

int fr_macos_audio_clock(void *handle, FrAudioClock *out) {
    FrAudioOutput *owner = handle;
    if (!owner || !out || atomic_load_explicit(&owner->closed, memory_order_acquire))
        return FR_AUDIO_CLOSED;
    int checked = device_properties(owner, false);
    if (checked != FR_AUDIO_OK) return checked;
    UInt32 running = 0, size = sizeof running;
    if (AudioQueueGetProperty(owner->queue, kAudioQueueProperty_IsRunning, &running, &size) != noErr ||
        size != sizeof running) return FR_AUDIO_NATIVE;
    if (!running) return FR_AUDIO_PENDING;
    AudioTimeStamp time = {0};
    Boolean discontinuity = false;
    if (AudioQueueGetCurrentTime(owner->queue, owner->timeline, &time, &discontinuity) != noErr ||
        discontinuity || !(time.mFlags & kAudioTimeStampSampleTimeValid) ||
        !isfinite(time.mSampleTime) || time.mSampleTime < 0 || time.mSampleTime >= 9007199254740992.0)
        return FR_AUDIO_CLOCK;
    if (!sleep_unchanged(owner)) return FR_AUDIO_CHANGED;
    out->samples = (uint64_t)floor(time.mSampleTime);
    out->lead = owner->lead;
    return FR_AUDIO_OK;
}

typedef int (*FrAudioPermission)(void *context, uint64_t samples);

int fr_macos_audio_submit(void *handle, const int16_t *pcm, uint32_t samples,
                          uint64_t start, uint64_t end,
                          FrAudioPermission permission, void *context) {
    FrAudioOutput *owner = handle;
    if (!owner || !pcm || !permission ||
        atomic_load_explicit(&owner->closed, memory_order_acquire)) return FR_AUDIO_CLOSED;
    if (samples == 0 || samples > FR_AUDIO_MAX_FRAMES || start > end || end - start != samples ||
        start < owner->submitted_end || end > UINT64_C(9007199254740992)) return FR_AUDIO_CONFIG;
    uint32_t bytes = samples * owner->channels * sizeof(int16_t);
    if (bytes > owner->buffer_bytes) return FR_AUDIO_BOUNDS;
    unsigned slot = FR_AUDIO_BUFFERS;
    for (unsigned i = 0; i < FR_AUDIO_BUFFERS; ++i) {
        bool expected = false;
        if (atomic_compare_exchange_strong_explicit(&owner->busy[i], &expected, true,
                memory_order_acq_rel, memory_order_acquire)) { slot = i; break; }
    }
    if (slot == FR_AUDIO_BUFFERS) return FR_AUDIO_BUSY;
    AudioQueueBufferRef buffer = owner->buffers[slot];
    memcpy(buffer->mAudioData, pcm, bytes);
    buffer->mAudioDataByteSize = bytes;
    FrAudioClock clock = {0};
    int checked = fr_macos_audio_clock(owner, &clock);
    if (checked == FR_AUDIO_OK &&
        (start < clock.samples || end < clock.samples || end - clock.samples > FR_AUDIO_QUEUE_FRAMES))
        checked = FR_AUDIO_BOUNDS;
    AudioTimeStamp requested = {0}, actual = {0};
    requested.mSampleTime = (Float64)start;
    requested.mFlags = kAudioTimeStampSampleTimeValid;
    // Final application gate, after preparation and native queue-clock work.
    // A bounded, negative-only sleep veto follows it: sleep inside the Rust
    // callback must not survive until AppKit's first post-wake event pump.
    if (checked == FR_AUDIO_OK && !permission(context, clock.samples)) checked = FR_AUDIO_DENIED;
    if (checked == FR_AUDIO_OK && !sleep_unchanged(owner)) checked = FR_AUDIO_CHANGED;
    if (checked != FR_AUDIO_OK) {
        memset(buffer->mAudioData, 0, bytes);
        atomic_store_explicit(&owner->busy[slot], false, memory_order_release);
        return checked;
    }
    if (AudioQueueEnqueueBufferWithParameters(owner->queue, buffer, 0, NULL, 0, 0, 0, NULL,
                                              &requested, &actual) != noErr)
        return FR_AUDIO_UNKNOWN;
    if (!(actual.mFlags & kAudioTimeStampSampleTimeValid) || !isfinite(actual.mSampleTime) ||
        actual.mSampleTime != requested.mSampleTime)
        return FR_AUDIO_UNKNOWN;
    owner->submitted_end = end;
    return FR_AUDIO_OK;
}

int fr_macos_audio_stop(void *handle) {
    FrAudioOutput *owner = handle;
    if (!owner) return FR_AUDIO_OK;
    atomic_store_explicit(&owner->closed, true, memory_order_release);
    if (AudioQueueStop(owner->queue, true) != noErr || AudioQueueReset(owner->queue) != noErr)
        return FR_AUDIO_NATIVE;
    return FR_AUDIO_OK;
}
