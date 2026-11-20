// Permission-only mode of the SAME signed fr-audio-worker that later records.
// Launched only after an explicit local Allow microphone gesture. A grant
// never records or starts Talk; the original viewer requires another gesture.
// https://developer.apple.com/documentation/avfoundation/requesting-authorization-to-capture-and-save-media
#import <AVFoundation/AVFoundation.h>
#import <Foundation/Foundation.h>
#import <Security/Security.h>
#import <CoreAudio/CoreAudio.h>
#include <sys/socket.h>
#include <poll.h>
#include <errno.h>
#include <time.h>
#include <string.h>
#include <mach/mach_time.h>
#include "macos_audio_clock.h"
#include "macos_microphone_capture.h"

@interface FrMicPermissionReply : NSObject
@property(atomic, assign) int result;
@end
@implementation FrMicPermissionReply
@end

// Negative-only sleep veto for the original viewer intent. The caller retains
// the first interval; later samples must overlap it and never replace it.
// Expose raw Mach ticks so the exact production interval policy is shared with
// capture and playback instead of converting/rounding either endpoint.
int fr_macos_mic_sleep_offset(uint64_t *lower, uint64_t *upper) {
    if (!lower || !upper) return FR_MIC_CONFIG;
    *lower = 0;
    *upper = 0;
    mach_timebase_info_data_t timebase;
    if (mach_timebase_info(&timebase) != KERN_SUCCESS) return FR_MIC_CLOCK;
    uint64_t before = mach_continuous_time();
    uint64_t awake = mach_absolute_time();
    uint64_t after = mach_continuous_time();
    FrAudioSleepOffset offset;
    if (!fr_audio_sleep_offset(before, awake, after, timebase.numer,
                               timebase.denom, &offset)) return FR_MIC_CLOCK;
    *lower = offset.lower;
    *upper = offset.upper;
    return FR_MIC_OK;
}

// Resolves only the user's explicit "default" selection. This metadata query
// does not open an Audio Queue, request permission, or follow later defaults.
int fr_macos_mic_default_uid(uint8_t *bytes, size_t capacity, size_t *count) {
    if (count) *count = 0;
    if (!bytes || !count || capacity == 0) return FR_MIC_CONFIG;
    AudioObjectPropertyAddress address = {
        kAudioHardwarePropertyDefaultInputDevice, kAudioObjectPropertyScopeGlobal,
        kAudioObjectPropertyElementMain
    };
    AudioDeviceID device = kAudioObjectUnknown;
    UInt32 size = sizeof device;
    if (AudioObjectGetPropertyData(kAudioObjectSystemObject, &address, 0, NULL,
                                   &size, &device) != noErr || size != sizeof device ||
        device == kAudioObjectUnknown) return FR_MIC_UNAVAILABLE;
    AudioObjectPropertyAddress uid_address = {
        kAudioDevicePropertyDeviceUID, kAudioObjectPropertyScopeGlobal,
        kAudioObjectPropertyElementMain
    };
    CFStringRef uid = NULL;
    size = sizeof uid;
    if (AudioObjectGetPropertyData(device, &uid_address, 0, NULL, &size, &uid) != noErr ||
        size != sizeof uid || !uid) {
        if (uid) CFRelease(uid);
        return FR_MIC_UNAVAILABLE;
    }
    int result = FR_MIC_SELECTION;
    if (CFGetTypeID(uid) == CFStringGetTypeID()) {
        CFIndex length = CFStringGetLength(uid), needed = 0;
        if (length > 0 && length <= 255 &&
            CFStringGetBytes(uid, CFRangeMake(0, length), kCFStringEncodingUTF8,
                             0, false, NULL, 0, &needed) == length &&
            needed > 0 && needed <= 255 && (size_t)needed <= capacity) {
            CFIndex copied = 0;
            if (CFStringGetBytes(uid, CFRangeMake(0, length), kCFStringEncodingUTF8,
                                 0, false, bytes, needed, &copied) == length && copied == needed) {
                AudioDeviceID after = kAudioObjectUnknown;
                size = sizeof after;
                if (AudioObjectGetPropertyData(kAudioObjectSystemObject, &address, 0, NULL,
                                               &size, &after) == noErr &&
                    size == sizeof after && after == device) {
                    *count = (size_t)copied;
                    result = FR_MIC_OK;
                } else {
                    memset(bytes, 0, (size_t)copied);
                    result = FR_MIC_UNAVAILABLE;
                }
            }
        }
    }
    CFRelease(uid);
    return result;
}

static int permitted(uint64_t until, uint64_t maximum) {
    struct timespec now;
    if (clock_gettime(CLOCK_MONOTONIC, &now) != 0 || now.tv_sec < 0 ||
        now.tv_nsec < 0 || now.tv_nsec >= 1000000000) return FR_MIC_CLOCK;
    __uint128_t nanos = (__uint128_t)now.tv_sec * 1000000000 + now.tv_nsec;
    if (nanos > UINT64_MAX) return FR_MIC_CLOCK;
    if (nanos >= until) return FR_MIC_EXPIRED;
    if ((__uint128_t)until - nanos > maximum) return FR_MIC_CONFIG;
    unsigned char byte;
    ssize_t count = recv(2, &byte, 1, MSG_PEEK | MSG_DONTWAIT);
    if (count != -1 || (errno != EAGAIN && errno != EWOULDBLOCK)) return FR_MIC_DENIED;
    return FR_MIC_OK;
}

static bool declared(void) {
    id usage = [[NSBundle mainBundle] objectForInfoDictionaryKey:@"NSMicrophoneUsageDescription"];
    if (![usage isKindOfClass:[NSString class]] || [(NSString *)usage length] == 0 ||
        [(NSString *)usage length] > 1024) return false;
    SecTaskRef task = SecTaskCreateFromSelf(kCFAllocatorDefault);
    if (!task) return false;
    CFTypeRef value = SecTaskCopyValueForEntitlement(task,
        CFSTR("com.apple.security.device.audio-input"), NULL);
    bool enabled = value && CFGetTypeID(value) == CFBooleanGetTypeID() &&
                   CFBooleanGetValue((CFBooleanRef)value);
    if (value) CFRelease(value);
    CFRelease(task);
    return enabled;
}

int fr_mic_authorize(uint64_t request_until_ns, uint64_t wait_until_ns) {
    @autoreleasepool {
      @try {
        // Both limits came from the original Allow gesture through private
        // stdin. Starting this helper or receiving a prompt result renews
        // neither the 250ms launch window nor the 60s permission window.
        if (!request_until_ns || wait_until_ns < request_until_ns ||
            wait_until_ns - request_until_ns != UINT64_C(59750000000))
            return FR_MIC_CONFIG;
        int result = permitted(request_until_ns, UINT64_C(250000000));
        if (result != FR_MIC_OK) return result;
        // Missing usage metadata or hardened-runtime permission can terminate
        // an app at an OS privacy prompt. Refuse before asking the OS instead.
        if (![NSThread isMainThread] || !declared()) return FR_MIC_CONFIG;
        AVAuthorizationStatus status =
            [AVCaptureDevice authorizationStatusForMediaType:AVMediaTypeAudio];
        if (status == AVAuthorizationStatusAuthorized)
            return permitted(request_until_ns, UINT64_C(250000000));
        if (status != AVAuthorizationStatusNotDetermined) return FR_MIC_DENIED;
        FrMicPermissionReply *reply = [FrMicPermissionReply new];
        if (!reply) return FR_MIC_ALLOCATION;
        result = permitted(request_until_ns, UINT64_C(250000000));
        if (result != FR_MIC_OK) return result;
        // The OS retains this block and its Objective-C reply object. A late
        // callback never touches a returned stack frame or a Rust reference.
        [AVCaptureDevice requestAccessForMediaType:AVMediaTypeAudio
                                completionHandler:^(BOOL granted) {
            reply.result = granted ? 1 : 2;
        }];
        while (reply.result == 0) {
            result = permitted(wait_until_ns, UINT64_C(60000000000));
            if (result != FR_MIC_OK) return result;
            SInt32 run = CFRunLoopRunInMode(kCFRunLoopDefaultMode, 0.01, true);
            if (run == kCFRunLoopRunFinished) {
                // A permission-only helper may have no run-loop sources. Do
                // not spin while the system prompt is awaiting local input.
                struct pollfd revoke = {2, POLLIN, 0};
                int ready = poll(&revoke, 1, 10);
                if (ready > 0 || (ready < 0 && errno != EINTR)) return FR_MIC_DENIED;
            }
        }
        result = permitted(wait_until_ns, UINT64_C(60000000000));
        if (result != FR_MIC_OK) return result;
        if (reply.result != 1 ||
            [AVCaptureDevice authorizationStatusForMediaType:AVMediaTypeAudio] !=
                AVAuthorizationStatusAuthorized) return FR_MIC_DENIED;
        return permitted(wait_until_ns, UINT64_C(60000000000));
      } @catch (NSException *exception) {
        (void)exception;
        return FR_MIC_NATIVE;
      }
    }
}
