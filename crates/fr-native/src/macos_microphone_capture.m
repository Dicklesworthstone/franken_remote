// Explicit local microphone capture in the supervised fr-audio-worker only.
// Audio Queue calls one serial dispatch queue. It writes fixed PCM slots; it
// never runs Rust, codecs, transport, consent UI, or an authority callback.
// https://developer.apple.com/documentation/audiotoolbox/audioqueuenewinputwithdispatchqueue(_:_:_:_:_:)
// https://developer.apple.com/documentation/audiotoolbox/audioqueuedispose(_:_:)
#import <AudioToolbox/AudioToolbox.h>
#import <CoreAudio/CoreAudio.h>
#import <AVFoundation/AVFoundation.h>
#import <Foundation/Foundation.h>
#import <dispatch/dispatch.h>
#include <mach/mach_time.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <fcntl.h>
#include <unistd.h>
#include <errno.h>
#include <stdatomic.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include "macos_audio_clock.h"
#include "macos_audio_device.h"
#include "macos_microphone_capture.h"

#define FR_MIC_NATIVE_BUFFERS 3u
#define FR_MIC_MAX_OWNERS 4u
#define FR_MIC_MAX_UID_BYTES 1024u

typedef struct {
    int16_t samples[FR_MIC_FRAMES];
    uint64_t sequence, position, captured_ns;
} FrMicSlot;

typedef struct {
    AudioQueueRef queue;
    CFTypeRef callbacks;
    AudioQueueBufferRef buffers[FR_MIC_NATIVE_BUFFERS];
    AudioDeviceID device;
    CFStringRef uid;
    Float64 rate;
    UInt32 device_buffer;
    AudioStreamID streams[8];
    UInt32 stream_count;
    int guard_fd;
    uint64_t setup_until_ns;
    mach_timebase_info_data_t timebase;
    FrAudioSleepOffset sleep;
    atomic_uint_fast64_t authorization_ns, written, read, last_capture_ns;
    atomic_int error;
    FrMicTimeline timeline; // only the serial native producer touches this
    FrMicSlot slots[FR_MIC_SLOTS];
} FrMicCapture;

// A failed native disposal retains both callback storage and its owner credit.
static atomic_uint owners = 0;

static int failed(FrMicCapture *owner, int error) {
    int expected = FR_MIC_OK;
    atomic_compare_exchange_strong_explicit(&owner->error, &expected, error,
                                            memory_order_acq_rel, memory_order_acquire);
    return atomic_load_explicit(&owner->error, memory_order_acquire);
}

static bool monotonic(uint64_t *out) {
    struct timespec now;
    if (clock_gettime(CLOCK_MONOTONIC, &now) != 0 || now.tv_sec < 0 ||
        now.tv_nsec < 0 || now.tv_nsec >= 1000000000) return false;
    __uint128_t nanos = (__uint128_t)now.tv_sec * 1000000000 + now.tv_nsec;
    if (nanos > UINT64_MAX) return false;
    *out = (uint64_t)nanos;
    return true;
}

static bool sleep_offset(FrMicCapture *owner, FrAudioSleepOffset *out) {
    uint64_t before = mach_continuous_time();
    uint64_t awake = mach_absolute_time();
    uint64_t after = mach_continuous_time();
    return fr_audio_sleep_offset(before, awake, after,
                                 owner->timebase.numer, owner->timebase.denom, out);
}

static int permitted(FrMicCapture *owner, uint64_t *now) {
    int error = atomic_load_explicit(&owner->error, memory_order_acquire);
    if (error != FR_MIC_OK) return error;
    FrAudioSleepOffset current;
    if (!monotonic(now) || !sleep_offset(owner, &current) ||
        !fr_audio_sleep_unchanged(owner->sleep, current))
        return failed(owner, FR_MIC_CLOCK);
    uint64_t until = atomic_load_explicit(&owner->authorization_ns, memory_order_acquire);
    if (*now >= until ||
        (atomic_load_explicit(&owner->written, memory_order_acquire) == 0 &&
         *now >= owner->setup_until_ns)) return failed(owner, FR_MIC_EXPIRED);
    // The duplicated original socket is read-only here. Leave both data and EOF
    // observable by Rust's own Guard; no native callback can consume a revoke.
    unsigned char byte;
    ssize_t n = recv(owner->guard_fd, &byte, 1, MSG_PEEK | MSG_DONTWAIT);
    if (n != -1 || (errno != EAGAIN && errno != EWOULDBLOCK))
        return failed(owner, FR_MIC_DENIED);
    return FR_MIC_OK;
}

static bool authorized(void) {
    @autoreleasepool {
      @try {
        // No requestAccess call in capture. The separately supervised, explicit
        // local Allow action must have authorized this exact helper beforehand.
        return [AVCaptureDevice authorizationStatusForMediaType:AVMediaTypeAudio] ==
               AVAuthorizationStatusAuthorized;
      } @catch (NSException *exception) {
        (void)exception;
        return false;
      }
    }
}

static int property(AudioObjectID object, AudioObjectPropertySelector selector,
                    AudioObjectPropertyScope scope, void *out, UInt32 size) {
    AudioObjectPropertyAddress address = {selector, scope, kAudioObjectPropertyElementMain};
    UInt32 actual = size;
    return AudioObjectGetPropertyData(object, &address, 0, NULL, &actual, out) == noErr &&
           actual == size ? FR_MIC_OK : FR_MIC_UNAVAILABLE;
}

static int selected_device(FrMicCapture *owner) {
    owner->device = fr_audio_device_for_uid(owner->uid);
    return owner->device == kAudioObjectUnknown ? FR_MIC_SELECTION : FR_MIC_OK;
}

static int device_properties(FrMicCapture *owner, bool initial) {
    UInt32 alive = 0, frames = 0;
    Float64 rate = 0;
    CFStringRef uid = NULL;
    int result = property(owner->device, kAudioDevicePropertyDeviceUID,
                          kAudioObjectPropertyScopeGlobal, &uid, sizeof uid);
    bool same = result == FR_MIC_OK && uid && CFEqual(uid, owner->uid);
    if (uid) CFRelease(uid);
    if (!same || property(owner->device, kAudioDevicePropertyDeviceIsAlive,
                         kAudioObjectPropertyScopeGlobal, &alive, sizeof alive) != FR_MIC_OK ||
        !alive || property(owner->device, kAudioDevicePropertyNominalSampleRate,
                           kAudioObjectPropertyScopeGlobal, &rate, sizeof rate) != FR_MIC_OK ||
        !isfinite(rate) || rate < 8000 || rate > 384000 ||
        property(owner->device, kAudioDevicePropertyBufferFrameSize,
                 kAudioObjectPropertyScopeGlobal, &frames, sizeof frames) != FR_MIC_OK ||
        !frames || (double)frames / rate > 0.050) return FR_MIC_UNAVAILABLE;
    AudioStreamID streams[8];
    UInt32 bytes = sizeof streams;
    AudioObjectPropertyAddress address = {
        kAudioDevicePropertyStreams, kAudioDevicePropertyScopeInput,
        kAudioObjectPropertyElementMain
    };
    if (AudioObjectGetPropertyData(owner->device, &address, 0, NULL, &bytes, streams) != noErr ||
        bytes == 0 || bytes > sizeof streams || bytes % sizeof streams[0] != 0)
        return FR_MIC_UNAVAILABLE;
    UInt32 count = bytes / sizeof streams[0];
    if (initial) {
        owner->rate = rate;
        owner->device_buffer = frames;
        owner->stream_count = count;
        memcpy(owner->streams, streams, bytes);
        return FR_MIC_OK;
    }
    if (owner->rate != rate || owner->device_buffer != frames ||
        owner->stream_count != count || memcmp(owner->streams, streams, bytes) != 0)
        return FR_MIC_DISCONTINUITY;
    CFStringRef routed = NULL;
    bytes = sizeof routed;
    OSStatus status = AudioQueueGetProperty(owner->queue, kAudioQueueProperty_CurrentDevice,
                                            &routed, &bytes);
    same = status == noErr && bytes == sizeof routed && routed && CFEqual(routed, owner->uid);
    if (routed) CFRelease(routed);
    return same ? FR_MIC_OK : FR_MIC_DISCONTINUITY;
}

static void captured(FrMicCapture *owner, AudioQueueRef queue, AudioQueueBufferRef buffer,
                     const AudioTimeStamp *stamp) {
    uint64_t now;
    if (permitted(owner, &now) != FR_MIC_OK) return;
    bool original = false;
    for (unsigned i = 0; i < FR_MIC_NATIVE_BUFFERS; ++i)
        if (owner->buffers[i] == buffer) original = true;
    if (!original || queue != owner->queue || !stamp || !buffer->mAudioData ||
        buffer->mAudioDataByteSize != FR_MIC_FRAMES * sizeof(int16_t) ||
        buffer->mAudioDataBytesCapacity != FR_MIC_FRAMES * sizeof(int16_t) ||
        (stamp->mFlags & (kAudioTimeStampHostTimeValid | kAudioTimeStampSampleTimeValid)) !=
            (kAudioTimeStampHostTimeValid | kAudioTimeStampSampleTimeValid)) {
        failed(owner, FR_MIC_DISCONTINUITY);
        return;
    }
    uint64_t write = atomic_load_explicit(&owner->written, memory_order_relaxed);
    uint64_t read = atomic_load_explicit(&owner->read, memory_order_acquire);
    if (!fr_mic_ring_room(write, read)) {
        failed(owner, FR_MIC_DISCONTINUITY);
        return;
    }
    uint64_t before, after, acquired;
    if (!monotonic(&before)) { failed(owner, FR_MIC_CLOCK); return; }
    uint64_t awake = mach_absolute_time();
    if (!monotonic(&after) ||
        !fr_mic_captured_ns(before, awake, after, stamp->mHostTime,
                           owner->timebase.numer, owner->timebase.denom, &acquired)) {
        failed(owner, FR_MIC_CLOCK);
        return;
    }
    FrMicSlot *slot = &owner->slots[write % FR_MIC_SLOTS];
    if (!fr_mic_observe(&owner->timeline, stamp->mSampleTime, acquired,
                        &slot->sequence, &slot->position)) {
        failed(owner, FR_MIC_DISCONTINUITY);
        return;
    }
    slot->captured_ns = acquired;
    memcpy(slot->samples, buffer->mAudioData, sizeof slot->samples);
    memset(buffer->mAudioData, 0, buffer->mAudioDataByteSize);
    if (permitted(owner, &now) != FR_MIC_OK) {
        memset(slot->samples, 0, sizeof slot->samples);
        return;
    }
    atomic_store_explicit(&owner->last_capture_ns, acquired, memory_order_release);
    atomic_store_explicit(&owner->written, write + 1, memory_order_release);
    if (AudioQueueEnqueueBuffer(queue, buffer, 0, NULL) != noErr)
        failed(owner, FR_MIC_NATIVE);
}

int fr_macos_mic_create(const char *uid, int guard_fd, uint64_t authorization_ns,
                        uint64_t setup_until_ns, void **out) {
    if (!out) return FR_MIC_CONFIG;
    *out = NULL;
    if (!uid || !*uid || strnlen(uid, FR_MIC_MAX_UID_BYTES + 1) > FR_MIC_MAX_UID_BYTES ||
        guard_fd < 0) return FR_MIC_SELECTION;
    uint64_t now;
    if (!monotonic(&now)) return FR_MIC_CLOCK;
    if (now >= authorization_ns || now >= setup_until_ns) return FR_MIC_EXPIRED;
    if (!authorized()) return FR_MIC_DENIED;
    unsigned count = atomic_load_explicit(&owners, memory_order_acquire);
    do {
        if (count >= FR_MIC_MAX_OWNERS) return FR_MIC_UNAVAILABLE;
    } while (!atomic_compare_exchange_weak_explicit(&owners, &count, count + 1,
                 memory_order_acq_rel, memory_order_acquire));
    FrMicCapture *owner = calloc(1, sizeof *owner);
    if (!owner) {
        atomic_fetch_sub_explicit(&owners, 1, memory_order_acq_rel);
        return FR_MIC_ALLOCATION;
    }
    owner->guard_fd = -1;
    owner->setup_until_ns = setup_until_ns;
    atomic_init(&owner->authorization_ns, authorization_ns);
    atomic_init(&owner->written, 0);
    atomic_init(&owner->read, 0);
    atomic_init(&owner->last_capture_ns, 0);
    atomic_init(&owner->error, FR_MIC_OK);
    *out = owner;
    if (!atomic_is_lock_free(&owner->authorization_ns) || !atomic_is_lock_free(&owner->written) ||
        !atomic_is_lock_free(&owner->read) || !atomic_is_lock_free(&owner->last_capture_ns) ||
        !atomic_is_lock_free(&owner->error))
        return failed(owner, FR_MIC_UNAVAILABLE);
    if (mach_timebase_info(&owner->timebase) != KERN_SUCCESS ||
        !sleep_offset(owner, &owner->sleep)) return failed(owner, FR_MIC_CLOCK);
    owner->guard_fd = fcntl(guard_fd, F_DUPFD_CLOEXEC, 3);
    struct sockaddr_un peer;
    socklen_t peer_bytes = sizeof peer;
    int type = 0;
    socklen_t type_bytes = sizeof type;
    if (owner->guard_fd < 0 ||
        getpeername(owner->guard_fd, (struct sockaddr *)&peer, &peer_bytes) != 0 ||
        peer.sun_family != AF_UNIX ||
        getsockopt(owner->guard_fd, SOL_SOCKET, SO_TYPE, &type, &type_bytes) != 0 ||
        type != SOCK_STREAM) return failed(owner, FR_MIC_CONFIG);
    owner->uid = CFStringCreateWithCString(kCFAllocatorDefault, uid, kCFStringEncodingUTF8);
    if (!owner->uid) return failed(owner, FR_MIC_SELECTION);
    int result = selected_device(owner);
    if (result == FR_MIC_OK) result = device_properties(owner, true);
    if (result != FR_MIC_OK) return failed(owner, result);
    if (permitted(owner, &now) != FR_MIC_OK) return atomic_load(&owner->error);
    AudioStreamBasicDescription format = {0};
    format.mSampleRate = 48000;
    format.mFormatID = kAudioFormatLinearPCM;
    format.mFormatFlags = kLinearPCMFormatFlagIsSignedInteger | kLinearPCMFormatFlagIsPacked;
    format.mBytesPerPacket = sizeof(int16_t);
    format.mFramesPerPacket = 1;
    format.mBytesPerFrame = sizeof(int16_t);
    format.mChannelsPerFrame = 1;
    format.mBitsPerChannel = 16;
    dispatch_queue_t callbacks = dispatch_queue_create("org.frankenremote.microphone", DISPATCH_QUEUE_SERIAL);
    if (!callbacks) return failed(owner, FR_MIC_ALLOCATION);
    owner->callbacks = CFBridgingRetain(callbacks);
    OSStatus status = AudioQueueNewInputWithDispatchQueue(&owner->queue, &format, 0, callbacks,
        ^(AudioQueueRef queue, AudioQueueBufferRef buffer, const AudioTimeStamp *stamp,
          UInt32 packets, const AudioStreamPacketDescription *descriptions) {
            (void)packets;
            (void)descriptions;
            captured(owner, queue, buffer, stamp);
        });
    if (status != noErr || !owner->queue) return failed(owner, FR_MIC_NATIVE);
    if (AudioQueueSetProperty(owner->queue, kAudioQueueProperty_CurrentDevice,
                              &owner->uid, sizeof owner->uid) != noErr)
        return failed(owner, FR_MIC_SELECTION);
    AudioStreamBasicDescription actual = {0};
    UInt32 bytes = sizeof actual;
    if (AudioQueueGetProperty(owner->queue, kAudioQueueProperty_StreamDescription,
                              &actual, &bytes) != noErr || bytes != sizeof actual ||
        actual.mSampleRate != format.mSampleRate || actual.mFormatID != format.mFormatID ||
        actual.mFormatFlags != format.mFormatFlags || actual.mBytesPerPacket != format.mBytesPerPacket ||
        actual.mFramesPerPacket != 1 || actual.mBytesPerFrame != sizeof(int16_t) ||
        actual.mChannelsPerFrame != 1 || actual.mBitsPerChannel != 16)
        return failed(owner, FR_MIC_CONFIG);
    for (unsigned i = 0; i < FR_MIC_NATIVE_BUFFERS; ++i) {
        if (AudioQueueAllocateBuffer(owner->queue, FR_MIC_FRAMES * sizeof(int16_t),
                                      &owner->buffers[i]) != noErr || !owner->buffers[i])
            return failed(owner, FR_MIC_ALLOCATION);
        if (AudioQueueEnqueueBuffer(owner->queue, owner->buffers[i], 0, NULL) != noErr)
            return failed(owner, FR_MIC_NATIVE);
    }
    result = device_properties(owner, false);
    if (result != FR_MIC_OK) return failed(owner, result);
    if (!authorized()) return failed(owner, FR_MIC_DENIED);
    // Recheck after all native preparation, immediately before recording. Native
    // work may block; that never extends the original setup or talk lifetime.
    result = permitted(owner, &now);
    if (result != FR_MIC_OK) return result;
    if (AudioQueueStart(owner->queue, NULL) != noErr) return failed(owner, FR_MIC_NATIVE);
    return permitted(owner, &now);
}

int fr_macos_mic_renew(void *handle, uint64_t original_authorization_ns,
                       uint64_t new_authorization_ns) {
    FrMicCapture *owner = handle;
    if (!owner) return FR_MIC_CONFIG;
    uint64_t now;
    int result = permitted(owner, &now);
    if (result != FR_MIC_OK) return result;
    uint64_t installed = atomic_load_explicit(&owner->authorization_ns, memory_order_acquire);
    if (!fr_mic_renewable(now, installed, original_authorization_ns, new_authorization_ns))
        return failed(owner, FR_MIC_EXPIRED);
    atomic_store_explicit(&owner->authorization_ns, new_authorization_ns, memory_order_release);
    if (!monotonic(&now)) return failed(owner, FR_MIC_CLOCK);
    if (now >= original_authorization_ns) return failed(owner, FR_MIC_EXPIRED);
    // An expired callback may have fenced this owner while the control thread
    // installed the new deadline. Its terminal result always wins.
    return permitted(owner, &now);
}

int fr_macos_mic_read(void *handle, int16_t *samples, size_t capacity,
                      uint32_t *count, uint64_t *sequence,
                      uint64_t *sample_position, uint64_t *captured_ns) {
    if (count) *count = 0;
    FrMicCapture *owner = handle;
    if (!owner || !samples || capacity < FR_MIC_FRAMES || !count || !sequence ||
        !sample_position || !captured_ns) return FR_MIC_CONFIG;
    uint64_t now;
    int result = permitted(owner, &now);
    if (result != FR_MIC_OK) return result;
    if (!authorized()) return failed(owner, FR_MIC_DENIED);
    result = device_properties(owner, false);
    if (result != FR_MIC_OK) return failed(owner, result);
    result = permitted(owner, &now);
    if (result != FR_MIC_OK) return result;
    uint64_t read = atomic_load_explicit(&owner->read, memory_order_relaxed);
    uint64_t written = atomic_load_explicit(&owner->written, memory_order_acquire);
    if (read == written) {
        uint64_t last = atomic_load_explicit(&owner->last_capture_ns, memory_order_acquire);
        if (written != 0 && (last > now || now - last >= FR_MIC_CUSTODY_NS))
            return failed(owner, FR_MIC_EXPIRED);
        return FR_MIC_PENDING;
    }
    if (read > written || written - read > FR_MIC_SLOTS)
        return failed(owner, FR_MIC_DISCONTINUITY);
    FrMicSlot *slot = &owner->slots[read % FR_MIC_SLOTS];
    if (slot->captured_ns > now || now - slot->captured_ns >= FR_MIC_CUSTODY_NS)
        return failed(owner, FR_MIC_EXPIRED);
    memcpy(samples, slot->samples, sizeof slot->samples);
    *sequence = slot->sequence;
    *sample_position = slot->position;
    *captured_ns = slot->captured_ns;
    memset(slot->samples, 0, sizeof slot->samples);
    atomic_store_explicit(&owner->read, read + 1, memory_order_release);
    result = permitted(owner, &now);
    if (result == FR_MIC_OK &&
        (*captured_ns > now || now - *captured_ns >= FR_MIC_CUSTODY_NS))
        result = failed(owner, FR_MIC_EXPIRED);
    if (result != FR_MIC_OK) {
        memset(samples, 0, FR_MIC_FRAMES * sizeof *samples);
        return result;
    }
    *count = FR_MIC_FRAMES;
    return FR_MIC_OK;
}

int fr_macos_mic_destroy(void *handle) {
    FrMicCapture *owner = handle;
    if (!owner) return FR_MIC_OK;
    failed(owner, FR_MIC_DENIED);
    if (owner->queue) {
        (void)AudioQueueStop(owner->queue, true);
        // Called only by the independent child control thread, never from an
        // Audio Queue callback. Successful synchronous disposal stops callbacks.
        if (AudioQueueDispose(owner->queue, true) != noErr) return FR_MIC_CLEANUP;
        owner->queue = NULL;
    }
    if (owner->callbacks) {
        dispatch_queue_t callbacks = (__bridge dispatch_queue_t)owner->callbacks;
        dispatch_sync(callbacks, ^{});
        CFRelease(owner->callbacks);
    }
    if (owner->uid) CFRelease(owner->uid);
    if (owner->guard_fd >= 0) close(owner->guard_fd);
    memset(owner->slots, 0, sizeof owner->slots);
    free(owner);
    atomic_fetch_sub_explicit(&owners, 1, memory_order_acq_rel);
    return FR_MIC_OK;
}
