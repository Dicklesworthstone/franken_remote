// macOS host capture (fr-p1-host-macos-vif): ScreenCaptureKit display capture
// straight into a VideoToolbox HEVC session. The captured IOSurface-backed
// NV12 buffer goes to the encoder without an application-side full-frame CPU
// copy; the encoded access unit is copied out. Settings are the ones the Phase 0 spike measured to
// pass admission (spikes/media-macos/README.md): RealTime, low-latency rate
// control, no frame reordering, one reference picture. Rust strips SEI and
// admits every unit with HevcGuard before it leaves the worker.
#import <CoreGraphics/CoreGraphics.h>
#import <CoreMedia/CoreMedia.h>
#import <CoreVideo/CoreVideo.h>
#import <Foundation/Foundation.h>
#import <ScreenCaptureKit/ScreenCaptureKit.h>
#import <VideoToolbox/VideoToolbox.h>
#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <math.h>
#include <mach/mach_time.h>
#include "macos_capture_policy.h"
#include "macos_display.h"
#include <os/lock.h>
#include <unistd.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/file.h>
#include <time.h>

#define FR_CAP_OK 0
#define FR_CAP_UNCHANGED 1
#define FR_CAP_INVALID -1
#define FR_CAP_UNAVAILABLE -2
#define FR_CAP_PERMISSION -3
#define FR_CAP_ALLOCATION -4
#define FR_CAP_GEOMETRY -5
#define FR_CAP_TIMEOUT -6
#define FR_CAP_CLOSED -7
#define FR_CAP_CODEC -8
#define FR_CAP_TOO_LARGE -9

typedef FrDisplay FrCapDisplay;

static dispatch_time_t after_ms(uint32_t ms) {
    return dispatch_time(DISPATCH_TIME_NOW, (int64_t)ms * (int64_t)NSEC_PER_MSEC);
}

// This is one native-open budget, not a fresh allowance for each framework
// call. The broker's earlier preparation deadline remains the outer bound.
// Native synchronous calls cannot be preempted here; late returns are refused
// and the original worker process remains the hard hang-containment boundary.
#define FR_CAP_OPEN_MS 2000
typedef struct {
    FrCapDeadline clock;
    dispatch_time_t wait;
} FrCapOpenDeadline;

static BOOL open_live(const FrCapOpenDeadline *deadline) {
    return fr_cap_deadline_live(&deadline->clock, clock_gettime_nsec_np(CLOCK_UPTIME_RAW));
}

// A pending discovery or failed start retains this exact lease. Its descriptor
// is not closed merely because the calling request stopped accepting results.
@interface FrCapSetupLease : NSObject {
  @public
    int descriptor;
}
@end
@implementation FrCapSetupLease
- (void)dealloc {
    if (descriptor >= 0) close(descriptor);
}
@end

// Capture-system setup is serialized across this user's processes. On macOS
// 27.0.1 a stream start hangs (well past any timeout) while another live
// process of the same user has queried shareable content but not yet started
// its stream; a running stream does not block anyone. So discovery never
// touches the capture system (Core Graphics only), and opening queries the
// content and starts the stream under this lock, as one step. The lock is
// advisory, in the user's private temporary directory, and the kernel drops
// it if the holder dies. Failure to acquire it refuses setup; starting another
// query after its entire budget expired would bypass that serialization.
static int setup_lock(const FrCapOpenDeadline *deadline, FrCapSetupLease *__strong *out) {
    *out = nil;
    if (!open_live(deadline)) return FR_CAP_TIMEOUT;
    char dir[PATH_MAX], path[PATH_MAX];
    size_t n = confstr(_CS_DARWIN_USER_TEMP_DIR, dir, sizeof dir);
    if (n == 0 || n > sizeof dir) return FR_CAP_UNAVAILABLE;
    if (snprintf(path, sizeof path, "%sfr-capture-setup.lock", dir) >= (int)sizeof path)
        return FR_CAP_UNAVAILABLE;
    if (!open_live(deadline)) return FR_CAP_TIMEOUT;
    int fd = open(path, O_RDWR | O_CREAT | O_CLOEXEC | O_NOFOLLOW, 0600);
    if (fd < 0) return open_live(deadline) ? FR_CAP_UNAVAILABLE : FR_CAP_TIMEOUT;
    for (;;) {
        if (!open_live(deadline)) {
            close(fd);
            return FR_CAP_TIMEOUT;
        }
        if (flock(fd, LOCK_EX | LOCK_NB) == 0) break;
        if (errno != EWOULDBLOCK && errno != EINTR) {
            close(fd);
            return open_live(deadline) ? FR_CAP_UNAVAILABLE : FR_CAP_TIMEOUT;
        }
        uint64_t remaining = fr_cap_deadline_remaining(
            &deadline->clock, clock_gettime_nsec_np(CLOCK_UPTIME_RAW));
        if (remaining > 2 * NSEC_PER_MSEC) remaining = 2 * NSEC_PER_MSEC;
        struct timespec pause = {.tv_sec = 0, .tv_nsec = (long)remaining};
        nanosleep(&pause, NULL);
    }
    if (!open_live(deadline)) {
        close(fd);
        return FR_CAP_TIMEOUT;
    }
    FrCapSetupLease *lease = [FrCapSetupLease new];
    if (!lease) {
        close(fd);
        return FR_CAP_ALLOCATION;
    }
    lease->descriptor = fd;
    *out = lease;
    return FR_CAP_OK;
}

@interface FrCapDiscovery : NSObject {
  @public
    os_unfair_lock lock;
    FrCapOneShot request;
    FrCapSetupLease *lease;
    SCShareableContent *content;
    int result;
    dispatch_semaphore_t done;
}
@end
@implementation FrCapDiscovery
@end

// Shareable content needs the Screen Recording grant; never prompt from here.
// A late callback only retires its original query/lease, never starts a stream.
static int shareable(const FrCapOpenDeadline *deadline, FrCapSetupLease *lease,
                      SCShareableContent *__strong *out) {
    *out = nil;
    if (!open_live(deadline)) return FR_CAP_TIMEOUT;
    BOOL permission = CGPreflightScreenCaptureAccess();
    if (!open_live(deadline)) return FR_CAP_TIMEOUT;
    if (!permission) return FR_CAP_PERMISSION;
    FrCapDiscovery *discovery = [FrCapDiscovery new];
    if (!discovery) return FR_CAP_ALLOCATION;
    discovery->lock = OS_UNFAIR_LOCK_INIT;
    discovery->request.accepting = true;
    discovery->lease = lease;
    discovery->result = FR_CAP_PERMISSION;
    discovery->done = dispatch_semaphore_create(0);
    if (!discovery->done) return FR_CAP_ALLOCATION;
    if (!open_live(deadline)) return FR_CAP_TIMEOUT;
    [SCShareableContent getShareableContentWithCompletionHandler:^(SCShareableContent *c, NSError *e) {
      @autoreleasepool {
          // Keep the lease through this callback even if an expired caller
          // has already returned. The ordinary caller retains it across the
          // subsequent configure/start operation when this result is timely.
          __attribute__((objc_precise_lifetime)) FrCapSetupLease *completed_lease = nil;
          os_unfair_lock_lock(&discovery->lock);
          BOOL accepted = fr_cap_one_shot_complete(&discovery->request);
          if (accepted) {
              discovery->content = c;
              discovery->result = !e && c ? FR_CAP_OK : FR_CAP_PERMISSION;
          }
          completed_lease = discovery->lease;
          discovery->lease = nil;
          os_unfair_lock_unlock(&discovery->lock);
          if (accepted) dispatch_semaphore_signal(discovery->done);
          (void)completed_lease;
      }
    }];
    BOOL timely = dispatch_semaphore_wait(discovery->done, deadline->wait) == 0 && open_live(deadline);
    os_unfair_lock_lock(&discovery->lock);
    fr_cap_one_shot_close(&discovery->request);
    int result = timely ? discovery->result : FR_CAP_TIMEOUT;
    SCShareableContent *content = discovery->content;
    discovery->content = nil;
    os_unfair_lock_unlock(&discovery->lock);
    if (result == FR_CAP_OK) *out = content;
    return result;
}

// The display's current mode and placement (macos_display.h).
static int describe(CGDirectDisplayID id, FrCapDisplay *out) {
    switch (fr_display_describe(id, out, NULL)) {
    case FR_DISPLAY_OK: return FR_CAP_OK;
    case FR_DISPLAY_GONE: return FR_CAP_GEOMETRY;
    default: return FR_CAP_UNAVAILABLE;
    }
}

static BOOL same_display(const FrCapDisplay *a, const FrCapDisplay *b) {
    return fr_display_same(a, b) ? YES : NO;
}

// Active displays from Core Graphics, without a capture-system round trip
// (see setup_lock). A mirror shows another display's picture: only the
// source is offered.
static int list_displays(FrCapDisplay *out, size_t capacity, size_t *count) {
    if (!CGPreflightScreenCaptureAccess()) return FR_CAP_PERMISSION;
    CGDirectDisplayID ids[16];
    uint32_t total = 0;
    if (CGGetActiveDisplayList(16, ids, &total) != kCGErrorSuccess) return FR_CAP_UNAVAILABLE;
    size_t n = 0;
    for (uint32_t i = 0; i < total; i++) {
        if (CGDisplayMirrorsDisplay(ids[i]) != kCGNullDirectDisplay) continue;
        if (n == capacity) return FR_CAP_TOO_LARGE;
        int status = describe(ids[i], &out[n]);
        if (status) return status;
        n++;
    }
    *count = n;
    return FR_CAP_OK;
}

int fr_cap_displays(FrCapDisplay *out, size_t capacity, size_t *count) {
    if (!out || !count) return FR_CAP_INVALID;
    *count = 0;
    return list_displays(out, capacity, count);
}

// FR_CAP_OK when the display still has exactly this mode and placement.
int fr_cap_display_check(const FrCapDisplay *expected) {
    if (!expected) return FR_CAP_INVALID;
    FrCapDisplay now;
    int status = describe(expected->display_id, &now);
    if (status) return status;
    return same_display(expected, &now) ? FR_CAP_OK : FR_CAP_GEOMETRY;
}

// The macOS form of Linux's parent-death signal: a kernel process-exit event
// on the expected parent ends this process. Both parent checks refuse a
// worker that was already reparented.
int fr_cap_bind_parent(int32_t expected) {
    static dispatch_source_t watch;
    if (expected <= 1 || watch || getppid() != expected) return FR_CAP_INVALID;
    watch = dispatch_source_create(DISPATCH_SOURCE_TYPE_PROC, (uintptr_t)expected, DISPATCH_PROC_EXIT,
                                   dispatch_get_global_queue(QOS_CLASS_USER_INITIATED, 0));
    if (!watch) return FR_CAP_UNAVAILABLE;
    dispatch_source_set_event_handler(watch, ^{
      _exit(0);
    });
    dispatch_resume(watch);
    if (getppid() != expected) _exit(0);
    return FR_CAP_OK;
}

@interface FrCapture : NSObject <SCStreamOutput, SCStreamDelegate> {
  @public
    os_unfair_lock lock;
    CVPixelBufferRef latest;   // newest complete frame, retained
    FrCapSource source;        // positive source evidence, protected by lock
    uint64_t encoded;          // source.picture at the last successful encode
    SCContentFilter *filter;
    SCStreamConfiguration *configuration;
    uint64_t verification_requests;
    uint64_t verification_encoded;
    size_t unit_limit;         // original caller's encoded-byte ceiling
    BOOL stopped;              // the stream ended (display gone, permission revoked)
    FrCapDisplay display;      // the display's mode and placement at open
    SCStream *stream;
    dispatch_queue_t queue;
    VTCompressionSessionRef session;
    uint32_t width;
    uint32_t height;
    // One encode in flight; the callback fills these and signals.
    dispatch_semaphore_t output;
    NSMutableData *unit;
    BOOL keyframe;
    BOOL dropped;              // rate control skipped the picture
    OSStatus result;
    // Presentation times, microseconds: never closer than one frame interval,
    // so rate control budgets requests that arrive early at the stream rate.
    int64_t interval;
    int64_t last_pts;
}
@end

static CVPixelBufferRef sample_picture(CMSampleBufferRef sample, uint32_t width, uint32_t height) {
    if (!sample || !CMSampleBufferIsValid(sample) || !CMSampleBufferDataIsReady(sample)) return NULL;
    CVPixelBufferRef pixels = CMSampleBufferGetImageBuffer(sample);
    if (!pixels || CVPixelBufferGetWidth(pixels) != width ||
        CVPixelBufferGetHeight(pixels) != height ||
        CVPixelBufferGetPixelFormatType(pixels) != kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange)
        return NULL;
    return pixels; // borrowed until the sample callback returns
}

// Only metadata identifies source progress. Callback receipt time never does.
// The SDK specifies SCStreamFrameInfoDisplayTime in mach absolute ticks.
static enum FrCapSourceEvent sample_event(CMSampleBufferRef sample, uint32_t width,
                                          uint32_t height, uint64_t *displayed,
                                          CVPixelBufferRef *picture) {
    *displayed = 0;
    *picture = NULL;
    if (!sample || !CMSampleBufferIsValid(sample)) return FR_CAP_SOURCE_INVALID;
    CFArrayRef attachments = CMSampleBufferGetSampleAttachmentsArray(sample, false);
    if (!attachments || CFArrayGetCount(attachments) != 1) return FR_CAP_SOURCE_INVALID;
    CFTypeRef attachment = CFArrayGetValueAtIndex(attachments, 0);
    if (!attachment || CFGetTypeID(attachment) != CFDictionaryGetTypeID())
        return FR_CAP_SOURCE_INVALID;
    NSDictionary *info = (__bridge NSDictionary *)attachment;
    NSNumber *status = info[SCStreamFrameInfoStatus];
    NSNumber *time = info[SCStreamFrameInfoDisplayTime];
    if (![status isKindOfClass:[NSNumber class]] || ![time isKindOfClass:[NSNumber class]])
        return FR_CAP_SOURCE_INVALID;
    *displayed = time.unsignedLongLongValue;
    if (status.integerValue == SCFrameStatusIdle) return FR_CAP_SOURCE_IDLE;
    if (status.integerValue != SCFrameStatusComplete) return FR_CAP_SOURCE_INVALID;
    *picture = sample_picture(sample, width, height);
    return *picture ? FR_CAP_SOURCE_COMPLETE : FR_CAP_SOURCE_INVALID;
}

// How far a frame's scheduled display time may be ahead of its delivery: a few
// refresh intervals of compositor schedule, in mach absolute ticks.
#define FR_CAP_DISPLAY_LEAD_MS 50
static uint64_t display_lead_ticks(void) {
    static uint64_t ticks;
    static dispatch_once_t once;
    dispatch_once(&once, ^{
      mach_timebase_info_data_t base;
      uint64_t ns = (uint64_t)FR_CAP_DISPLAY_LEAD_MS * NSEC_PER_MSEC;
      ticks = mach_timebase_info(&base) == KERN_SUCCESS && base.numer != 0
                  ? ns * base.denom / base.numer
                  : 0;
    });
    return ticks;
}

// Takes no ownership of picture. Retaining a selected surface before the
// callback returns keeps its backing pool slot alive; there is one latest
// slot and, while VT runs, one separately retained submitted surface.
static void observe(FrCapture *c, enum FrCapSourceEvent event, uint64_t displayed,
                    uint64_t received, CVPixelBufferRef picture) {
    CVPixelBufferRef previous = NULL;
    os_unfair_lock_lock(&c->lock);
    if (c->stopped) {
        os_unfair_lock_unlock(&c->lock);
        return;
    }
    enum FrCapSourceUpdate update =
        fr_cap_source_observe(&c->source, event, displayed, received, display_lead_ticks());
    if (update == FR_CAP_SOURCE_REPLACED) {
        previous = c->latest;
        c->latest = CVPixelBufferRetain(picture);
    } else if (update == FR_CAP_SOURCE_INVALIDATED) {
        previous = c->latest;
        c->latest = NULL;
    }
    os_unfair_lock_unlock(&c->lock);
    if (previous) CVPixelBufferRelease(previous);
}

@implementation FrCapture
- (void)stream:(SCStream *)s didOutputSampleBuffer:(CMSampleBufferRef)sample
        ofType:(SCStreamOutputType)type {
    (void)s;
    if (type != SCStreamOutputTypeScreen) return;
    uint64_t displayed = 0;
    CVPixelBufferRef picture = NULL;
    enum FrCapSourceEvent event = sample_event(sample, width, height, &displayed, &picture);
    observe(self, event, displayed, mach_absolute_time(), picture);
}
- (void)stream:(SCStream *)s didStopWithError:(NSError *)error {
    (void)s;
    (void)error;
    os_unfair_lock_lock(&lock);
    stopped = YES;
    fr_cap_source_invalidate(&source, mach_absolute_time());
    CVPixelBufferRef previous = latest;
    latest = NULL;
    os_unfair_lock_unlock(&lock);
    if (previous) CVPixelBufferRelease(previous);
}
@end

// A screenshot callback owns only this one-result box, never FrCapture or the
// next request. An expired caller closes admission before dropping its owner;
// a late callback releases its own surface without publishing it anywhere.
@interface FrCapVerification : NSObject {
  @public
    os_unfair_lock lock;
    FrCapOneShot request;
    CVPixelBufferRef picture;
    int result;
    dispatch_semaphore_t done;
    SCContentFilter *filter;
    SCStreamConfiguration *configuration;
}
@end
@implementation FrCapVerification
- (void)dealloc {
    if (picture) CVPixelBufferRelease(picture);
}
@end

// No stream callback within this window is UNKNOWN, not unchanged. On macOS
// 14+ a real screenshot verifies current source pixels. This is a deliberate
// bounded refresh cost (at most 20 requests/s under an eager caller), not a
// claim of zero idle encoding. The broker's original deadline remains in force.
#define FR_CAP_VERIFY_WAIT_MS 50

static int verify_source(FrCapture *c, dispatch_time_t deadline, uint64_t deadline_ns,
                          CVPixelBufferRef *out, uint64_t *source_epoch) {
    *out = NULL;
    if (@available(macOS 14.0, *)) {
        if (clock_gettime_nsec_np(CLOCK_UPTIME_RAW) >= deadline_ns) return FR_CAP_TIMEOUT;
        if (c->verification_requests == UINT64_MAX) return FR_CAP_UNAVAILABLE;
        FrCapVerification *verification = [FrCapVerification new];
        verification->lock = OS_UNFAIR_LOCK_INIT;
        verification->request.accepting = true;
        verification->result = FR_CAP_UNAVAILABLE;
        verification->done = dispatch_semaphore_create(0);
        verification->filter = c->filter;
        verification->configuration = c->configuration;
        uint32_t width = c->width, height = c->height;
        // An earlier gap may be repaired by this new capture. Any invalidation
        // after this exact operation starts fences its eventual result, even
        // if the stream subsequently supplies a new complete picture.
        os_unfair_lock_lock(&c->lock);
        BOOL stopped = c->stopped || c->source.exhausted;
        *source_epoch = c->source.invalidation;
        os_unfair_lock_unlock(&c->lock);
        if (stopped) return FR_CAP_GEOMETRY;
        if (clock_gettime_nsec_np(CLOCK_UPTIME_RAW) >= deadline_ns) return FR_CAP_TIMEOUT;
        c->verification_requests++;
        // Apple documents a distinct one-shot capture, not an SCStream poll.
        // Its sample is NOT guaranteed to contain SCStreamFrameInfo. Success
        // is a serviced post-request capture of this exact filter/configuration.
        // The timestamp stays the broker's pre-request lower bound; callback
        // arrival is never recorded as a source/display timestamp.
        [SCScreenshotManager captureSampleBufferWithFilter:verification->filter
                                            configuration:verification->configuration
                                        completionHandler:^(CMSampleBufferRef sample, NSError *error) {
          @autoreleasepool {
              CVPixelBufferRef picture = error ? NULL : sample_picture(sample, width, height);
              if (picture) CVPixelBufferRetain(picture);
              os_unfair_lock_lock(&verification->lock);
              BOOL accepted = fr_cap_one_shot_complete(&verification->request);
              if (accepted) {
                  verification->result = picture ? FR_CAP_OK : FR_CAP_UNAVAILABLE;
                  verification->picture = picture;
                  picture = NULL;
              }
              os_unfair_lock_unlock(&verification->lock);
              if (picture) CVPixelBufferRelease(picture);
              if (accepted) dispatch_semaphore_signal(verification->done);
          }
        }];
        BOOL timely = dispatch_semaphore_wait(verification->done, deadline) == 0 &&
                      clock_gettime_nsec_np(CLOCK_UPTIME_RAW) < deadline_ns;
        os_unfair_lock_lock(&verification->lock);
        fr_cap_one_shot_close(&verification->request);
        int result = timely ? verification->result : FR_CAP_TIMEOUT;
        CVPixelBufferRef picture = verification->picture;
        verification->picture = NULL;
        os_unfair_lock_unlock(&verification->lock);
        if (result == FR_CAP_OK && picture) {
            *out = picture;
        } else if (picture) {
            CVPixelBufferRelease(picture);
        }
        return result;
    }
    return FR_CAP_UNAVAILABLE;
}

static BOOL append_nal(NSMutableData *unit, const uint8_t *nal, size_t len, size_t limit) {
    if (!nal || !fr_cap_nal_fits(unit.length, len, limit)) return NO;
    uint32_t length = (uint32_t)len; // checked by fr_cap_nal_fits before conversion
    const uint8_t prefix[4] = {
        (uint8_t)(length >> 24), (uint8_t)(length >> 16), (uint8_t)(length >> 8), (uint8_t)length,
    };
    [unit appendBytes:prefix length:sizeof prefix];
    [unit appendBytes:nal length:len];
    return YES;
}

static void encoded(void *refcon, void *frame, OSStatus status, VTEncodeInfoFlags flags,
                    CMSampleBufferRef sample) {
    (void)frame;
    @autoreleasepool {
        FrCapture *c = (__bridge FrCapture *)refcon;
        c->unit = nil;
        c->result = status;
        c->dropped = status == noErr && (flags & kVTEncodeInfo_FrameDropped);
        if (status == noErr && (!sample || !CMSampleBufferIsValid(sample) ||
                              !CMSampleBufferDataIsReady(sample) || c->dropped))
            c->result = -1;
        if (c->result == noErr) {
            BOOL sync = YES;
            CFArrayRef attachments = CMSampleBufferGetSampleAttachmentsArray(sample, false);
            if (attachments && CFArrayGetCount(attachments) > 0) {
                CFDictionaryRef a = CFArrayGetValueAtIndex(attachments, 0);
                CFBooleanRef not_sync = CFDictionaryGetValue(a, kCMSampleAttachmentKey_NotSync);
                sync = !(not_sync && CFBooleanGetValue(not_sync));
            }
            NSMutableData *unit = [NSMutableData data];
            if (!unit) c->result = -1;
            CMFormatDescriptionRef format = CMSampleBufferGetFormatDescription(sample);
            size_t sets = 0;
            int prefix_bytes = 0;
            // Retain the native length framing; refuse a different prefix size
            // before parsing or exporting a picture under the canonical ABI.
            if (c->result == noErr &&
                (!format ||
                 CMVideoFormatDescriptionGetHEVCParameterSetAtIndex(
                     format, 0, NULL, NULL, &sets, &prefix_bytes) != noErr ||
                 prefix_bytes != 4))
                c->result = -1;
            if (sync && c->result == noErr) {
                if (sets != 3) c->result = -1;
                for (size_t i = 0; c->result == noErr && i < sets; i++) {
                    const uint8_t *p = NULL;
                    size_t len = 0;
                    if (CMVideoFormatDescriptionGetHEVCParameterSetAtIndex(
                            format, i, &p, &len, NULL, NULL) != noErr ||
                        !append_nal(unit, p, len, c->unit_limit))
                        c->result = -1;
                }
            }
            CMBlockBufferRef block = CMSampleBufferGetDataBuffer(sample);
            size_t total = block ? CMBlockBufferGetDataLength(block) : 0;
            if (c->result == noErr &&
                (total == 0 || unit.length > c->unit_limit ||
                 total > c->unit_limit - unit.length))
                c->result = -1;
            // CMBlockBuffer offsets are contiguous; its underlying memory need
            // not be. CopyDataBytes handles fragmented buffers without reading
            // past the first memory block or allocating an unbounded scratch.
            for (size_t off = 0; c->result == noErr && off < total;) {
                uint8_t size[4];
                if (total - off < sizeof size ||
                    CMBlockBufferCopyDataBytes(block, off, sizeof size, size) != kCMBlockBufferNoErr) {
                    c->result = -1;
                    break;
                }
                uint32_t len = ((uint32_t)size[0] << 24) | ((uint32_t)size[1] << 16) |
                               ((uint32_t)size[2] << 8) | (uint32_t)size[3];
                off += sizeof size;
                if (len > total - off || !fr_cap_nal_fits(unit.length, len, c->unit_limit)) {
                    c->result = -1;
                    break;
                }
                size_t prefix = unit.length;
                [unit increaseLengthBy:4 + (size_t)len];
                uint8_t *destination = unit.mutableBytes;
                memcpy(destination + prefix, size, sizeof size);
                if (CMBlockBufferCopyDataBytes(block, off, len, destination + prefix + 4) !=
                    kCMBlockBufferNoErr) {
                    c->result = -1;
                    break;
                }
                off += len;
            }
            if (c->result == noErr) {
                c->unit = unit;
                c->keyframe = sync;
            }
        }
        dispatch_semaphore_signal(c->output);
    }
}

static BOOL set(VTCompressionSessionRef s, CFStringRef key, CFTypeRef value) {
    return VTSessionSetProperty(s, key, value) == noErr;
}

static void close_session(FrCapture *c) {
    if (!c->session) return;
    VTCompressionSessionInvalidate(c->session);
    CFRelease(c->session);
    c->session = NULL;
}

// This owner exists before the stream can call back. Its deliberate retained
// self-reference is released only by successful transfer or positive native
// retirement, not by the caller's timeout. A stuck stop/removal/encoder keeps
// this one capture and setup lease owned until the worker process is reaped.
@interface FrCapOpening : NSObject {
  @public
    os_unfair_lock lock;
    FrCapOpeningPolicy policy;
    FrCapture *capture;
    FrCapSetupLease *lease;
    FrCapOpening *retention;
    dispatch_semaphore_t done;
    BOOL output_registered;
}
@end
@implementation FrCapOpening
@end

static void opening_release(FrCapOpening *opening) {
    os_unfair_lock_lock(&opening->lock);
    BOOL release = fr_cap_opening_release(&opening->policy);
    os_unfair_lock_unlock(&opening->lock);
    if (release) {
        // Exactly one actor may clear these fields, and no native operation
        // still needs them after both retirement acknowledgements. Destruction
        // and descriptor close happen outside the policy lock.
        opening->capture = nil;
        opening->lease = nil;
        opening->retention = nil;
    }
}

static void opening_retired(FrCapOpening *opening, BOOL codec) {
    os_unfair_lock_lock(&opening->lock);
    if (codec)
        opening->policy.codec_retired = true;
    else
        opening->policy.stream_retired = true;
    os_unfair_lock_unlock(&opening->lock);
    opening_release(opening);
}

static void opening_remove_output(FrCapOpening *opening) {
    FrCapture *c = opening->capture;
    if (opening->output_registered) {
        NSError *error = nil;
        if (![c->stream removeStreamOutput:c type:SCStreamOutputTypeScreen error:&error])
            return; // Removal was not confirmed: retain the original owner.
        opening->output_registered = NO;
    }
    // Removal prevents further delivery to this output. The marker runs after
    // already queued callbacks on the ORIGINAL serial sample queue; none can
    // repopulate pixels because failed-open admission was fenced first.
    dispatch_async(c->queue, ^{
      @autoreleasepool {
          opening_retired(opening, NO);
      }
    });
}

// Called by the unique actor that claimed retirement under opening->lock.
// VT remains caller-owned; asynchronous SCK cleanup never accesses session.
static void opening_retire(FrCapOpening *opening) {
    if (opening->policy.started) {
        [opening->capture->stream stopCaptureWithCompletionHandler:^(NSError *error) {
          @autoreleasepool {
              if (!error) opening_remove_output(opening);
              // Error or no completion is not drained. Keep the setup lease
              // and capture owned for the existing process supervisor.
          }
        }];
    } else {
        opening_remove_output(opening);
    }
}

static int opening_abandon(FrCapOpening *opening, const FrCapOpenDeadline *deadline, int status) {
    FrCapture *c = opening->capture;
    os_unfair_lock_lock(&c->lock);
    c->stopped = YES;
    fr_cap_source_invalidate(&c->source, mach_absolute_time());
    CVPixelBufferRef previous = c->latest;
    c->latest = NULL;
    os_unfair_lock_unlock(&c->lock);
    if (previous) CVPixelBufferRelease(previous);

    os_unfair_lock_lock(&opening->lock);
    fr_cap_opening_close(&opening->policy);
    BOOL retire = fr_cap_opening_retire(&opening->policy);
    os_unfair_lock_unlock(&opening->lock);
    if (retire) opening_retire(opening);
    // No frames have been submitted to VT during open. Invalidation may still
    // block in the framework: retaining ownership and the outer process bound
    // are required; a timeout is never reported as native drain completion.
    close_session(c);
    opening_retired(opening, YES);
    return open_live(deadline) ? status : FR_CAP_TIMEOUT;
}

static BOOL set_open(VTCompressionSessionRef session, CFStringRef key, CFTypeRef value,
                     const FrCapOpenDeadline *deadline) {
    if (!open_live(deadline)) return NO;
    BOOL accepted = set(session, key, value);
    return open_live(deadline) && accepted;
}

// Hardware only, low-latency rate control: a two-picture DPB with one
// previous reference. The default mode keeps four references.
static int open_session(FrCapture *c, uint32_t fps, uint32_t bitrate,
                         const FrCapOpenDeadline *deadline) {
    if (!open_live(deadline)) return FR_CAP_TIMEOUT;
    NSDictionary *spec = @{
        (__bridge NSString *)kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder : @YES,
        (__bridge NSString *)kVTVideoEncoderSpecification_EnableLowLatencyRateControl : @YES,
    };
    VTCompressionSessionRef session = NULL;
    OSStatus created = VTCompressionSessionCreate(
        NULL, (int32_t)c->width, (int32_t)c->height, kCMVideoCodecType_HEVC,
        (__bridge CFDictionaryRef)spec, NULL, NULL, encoded, (__bridge void *)c, &session);
    // Even a late/error return must leave any native allocation with its
    // original owner. opening_abandon performs and records its retirement.
    c->session = session;
    if (!open_live(deadline)) return FR_CAP_TIMEOUT;
    if (created != noErr || !session) return FR_CAP_UNAVAILABLE;
    if (!set_open(session, kVTCompressionPropertyKey_RealTime, kCFBooleanTrue, deadline) ||
        !set_open(session, kVTCompressionPropertyKey_AllowFrameReordering, kCFBooleanFalse, deadline) ||
        !set_open(session, kVTCompressionPropertyKey_ProfileLevel, kVTProfileLevel_HEVC_Main_AutoLevel,
                  deadline) ||
        !set_open(session, kVTCompressionPropertyKey_ExpectedFrameRate, (__bridge CFTypeRef) @(fps),
                  deadline) ||
        !set_open(session, kVTCompressionPropertyKey_AverageBitRate, (__bridge CFTypeRef) @(bitrate),
                  deadline) ||
        // IDRs come only on request (start, recovery).
        !set_open(session, kVTCompressionPropertyKey_MaxKeyFrameInterval,
                  (__bridge CFTypeRef) @(INT32_MAX), deadline) ||
        !set_open(session, kVTCompressionPropertyKey_ColorPrimaries,
                  kCVImageBufferColorPrimaries_ITU_R_709_2, deadline) ||
        !set_open(session, kVTCompressionPropertyKey_TransferFunction,
                  kCVImageBufferTransferFunction_ITU_R_709_2, deadline) ||
        !set_open(session, kVTCompressionPropertyKey_YCbCrMatrix,
                  kCVImageBufferYCbCrMatrix_ITU_R_709_2, deadline) ||
        !set_open(session, kVTCompressionPropertyKey_ReferenceBufferCount, (__bridge CFTypeRef) @1,
                  deadline))
        return open_live(deadline) ? FR_CAP_UNAVAILABLE : FR_CAP_TIMEOUT;
    if (!open_live(deadline)) return FR_CAP_TIMEOUT;
    OSStatus prepared = VTCompressionSessionPrepareToEncodeFrames(session);
    if (!open_live(deadline)) return FR_CAP_TIMEOUT;
    if (prepared != noErr) return FR_CAP_UNAVAILABLE;
    return FR_CAP_OK;
}

static int open_capture(uint32_t display_id, uint32_t width, uint32_t height, uint32_t fps,
                        uint32_t bitrate, const FrCapOpenDeadline *deadline,
                        FrCapSetupLease *lease, void **out) {
    @autoreleasepool {
        SCShareableContent *content = nil;
        int status = shareable(deadline, lease, &content);
        if (status) return status;
        SCDisplay *display = nil;
        for (SCDisplay *d in content.displays)
            if (d.displayID == display_id) display = d;
        if (!open_live(deadline)) return FR_CAP_TIMEOUT;
        if (!display) return FR_CAP_GEOMETRY;

        FrCapture *c = [FrCapture new];
        if (!c) return FR_CAP_ALLOCATION;
        c->lock = OS_UNFAIR_LOCK_INIT;
        status = describe(display_id, &c->display);
        if (!open_live(deadline)) return FR_CAP_TIMEOUT;
        if (status) return status;
        c->width = width;
        c->height = height;
        c->output = dispatch_semaphore_create(0);
        if (!c->output) return FR_CAP_ALLOCATION;
        c->interval = 1000000 / (int64_t)fps;
        c->last_pts = INT64_MIN;

        SCContentFilter *filter = [[SCContentFilter alloc] initWithDisplay:display excludingWindows:@[]];
        SCStreamConfiguration *config = [SCStreamConfiguration new];
        if (!open_live(deadline)) return FR_CAP_TIMEOUT;
        if (!filter || !config) return FR_CAP_ALLOCATION;
        config.width = width;
        config.height = height;
        config.pixelFormat = kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange;
        config.colorMatrix = kCGDisplayStreamYCbCrMatrix_ITU_R_709_2;
        config.colorSpaceName = kCGColorSpaceITUR_709;
        config.minimumFrameInterval = CMTimeMake(1, (int32_t)fps);
        config.queueDepth = 4;
        // The pointer is drawn into the picture until a client-rendered
        // cursor is captured separately (ReadCursor reports Unsupported).
        config.showsCursor = YES;
        c->queue = dispatch_queue_create("fr.capture", DISPATCH_QUEUE_SERIAL);
        if (!open_live(deadline)) return FR_CAP_TIMEOUT;
        if (!c->queue) return FR_CAP_ALLOCATION;
        c->filter = filter;
        c->configuration = config;

        FrCapOpening *opening = [FrCapOpening new];
        if (!opening) return FR_CAP_ALLOCATION;
        opening->lock = OS_UNFAIR_LOCK_INIT;
        opening->policy.accepting = true;
        opening->capture = c;
        opening->lease = lease;
        opening->done = dispatch_semaphore_create(0);
        if (!opening->done) return FR_CAP_ALLOCATION;
        opening->retention = opening; // Deliberate original-operation ownership.
        if (!open_live(deadline)) return opening_abandon(opening, deadline, FR_CAP_TIMEOUT);
        c->stream = [[SCStream alloc] initWithFilter:filter configuration:config delegate:c];
        if (!open_live(deadline)) return opening_abandon(opening, deadline, FR_CAP_TIMEOUT);
        if (!c->stream) return opening_abandon(opening, deadline, FR_CAP_UNAVAILABLE);
        NSError *error = nil;
        opening->output_registered =
            [c->stream addStreamOutput:c type:SCStreamOutputTypeScreen sampleHandlerQueue:c->queue
                                 error:&error];
        if (!open_live(deadline)) return opening_abandon(opening, deadline, FR_CAP_TIMEOUT);
        if (!opening->output_registered)
            return opening_abandon(opening, deadline, FR_CAP_UNAVAILABLE);
        // Start capturing first and set up the encoder while the capture
        // system starts: the two are independent until the first encode, and
        // both spend the same original setup budget.
        os_unfair_lock_lock(&opening->lock);
        BOOL requested = open_live(deadline) && fr_cap_opening_request(&opening->policy);
        os_unfair_lock_unlock(&opening->lock);
        if (!requested) return opening_abandon(opening, deadline, FR_CAP_TIMEOUT);
        [c->stream startCaptureWithCompletionHandler:^(NSError *e) {
          @autoreleasepool {
              os_unfair_lock_lock(&opening->lock);
              BOOL completed = fr_cap_opening_complete(&opening->policy, e == nil);
              BOOL retire = completed && fr_cap_opening_retire(&opening->policy);
              os_unfair_lock_unlock(&opening->lock);
              if (completed) dispatch_semaphore_signal(opening->done);
              if (retire) opening_retire(opening);
          }
        }];
        status = open_session(c, fps, bitrate, deadline);
        if (status) return opening_abandon(opening, deadline, status);
        BOOL timely = dispatch_semaphore_wait(opening->done, deadline->wait) == 0 &&
                      open_live(deadline);
        if (!timely) return opening_abandon(opening, deadline, FR_CAP_TIMEOUT);
        // A successful start callback is not proof the stream stayed live
        // while the encoder configured. Fence final publication against the
        // same stopped/exhausted state used by native source callbacks.
        // This is the only nested lock site: capture, then opening.
        os_unfair_lock_lock(&c->lock);
        BOOL live = !c->stopped && !c->source.exhausted;
        os_unfair_lock_lock(&opening->lock);
        BOOL started = opening->policy.started;
        BOOL published = live && fr_cap_opening_publish(
            &opening->policy, &deadline->clock, clock_gettime_nsec_np(CLOCK_UPTIME_RAW));
        if (published) *out = (__bridge_retained void *)c;
        os_unfair_lock_unlock(&opening->lock);
        os_unfair_lock_unlock(&c->lock);
        if (!published)
            return opening_abandon(opening, deadline,
                                   !live ? FR_CAP_GEOMETRY : started ? FR_CAP_TIMEOUT : FR_CAP_PERMISSION);
        opening_release(opening);
    }
    return FR_CAP_OK;
}

int fr_cap_open(uint32_t display_id, uint32_t width, uint32_t height, uint32_t fps, uint32_t bitrate,
                void **out) {
    if (!out || width < 16 || height < 16 || (width | height) & 1 || fps < 1 || fps > 240 ||
        bitrate == 0)
        return FR_CAP_INVALID;
    *out = NULL;
    FrCapOpenDeadline deadline;
    if (!fr_cap_deadline_init(&deadline.clock, clock_gettime_nsec_np(CLOCK_UPTIME_RAW),
                              (uint64_t)FR_CAP_OPEN_MS * NSEC_PER_MSEC))
        return FR_CAP_TIMEOUT;
    deadline.wait = after_ms(FR_CAP_OPEN_MS);
    // A live static source needs the bounded verification path. Never claim
    // that an older runtime can prove freshness by retaining the last pixels.
    if (@available(macOS 14.0, *)) {
    } else {
        return FR_CAP_UNAVAILABLE;
    }
    FrCapSetupLease *lease = nil;
    int status = setup_lock(&deadline, &lease);
    if (status) return status;
    return open_capture(display_id, width, height, fps, bitrate, &deadline, lease, out);
}

// Low-latency rate control skips a picture that would overrun its budget,
// typically the first after a large IDR. Each resubmission one interval
// later releases one frame's budget; past this many the request fails.
#define FR_CAP_RESUBMITS 8

// Asks for Screen Recording on behalf of this process's responsible process
// (frd when launchd runs it). macOS shows its prompt only while no decision
// is recorded; otherwise this only reports. Called by the startup probe only.
int fr_cap_request_access(void) { return CGRequestScreenCaptureAccess() ? 1 : 0; }

static int capture_live(FrCapture *c) {
    os_unfair_lock_lock(&c->lock);
    BOOL stopped = c->stopped || c->source.exhausted;
    os_unfair_lock_unlock(&c->lock);
    if (stopped) return FR_CAP_GEOMETRY;
    if (!CGPreflightScreenCaptureAccess()) return FR_CAP_PERMISSION;
    return fr_cap_display_check(&c->display);
}

static BOOL source_current(FrCapture *c, uint64_t source_epoch) {
    os_unfair_lock_lock(&c->lock);
    BOOL current = !c->stopped && fr_cap_source_current(&c->source, source_epoch);
    os_unfair_lock_unlock(&c->lock);
    return current;
}

// A reply may use the broker's pre-request timestamp only after positive
// post-request source evidence. Cached pixels and stream silence cannot mint
// Unchanged or a fresh IDR. One absolute deadline includes source verification,
// native encoding and all bounded rate-control resubmissions.
int fr_cap_encode(void *handle, int force_idr, int only_if_changed, uint32_t timeout_ms, uint8_t *buffer,
                  size_t capacity, size_t *length, int *idr) {
    if (!handle || !buffer || !length || !idr || capacity == 0 || timeout_ms == 0)
        return FR_CAP_INVALID;
    uint64_t requested = mach_absolute_time();
    uint64_t entered_ns = clock_gettime_nsec_np(CLOCK_UPTIME_RAW);
    uint64_t timeout_ns = (uint64_t)timeout_ms * NSEC_PER_MSEC;
    if (entered_ns > UINT64_MAX - timeout_ns) return FR_CAP_INVALID;
    uint64_t deadline_ns = entered_ns + timeout_ns;
    dispatch_time_t deadline = after_ms(timeout_ms);
    FrCapture *c = (__bridge FrCapture *)handle;
    *length = 0;
    *idr = 0;
    if (!c->session) return FR_CAP_CLOSED;
    int live = capture_live(c);
    if (live) return live;
    @autoreleasepool {
        CVPixelBufferRef picture = NULL;
        uint64_t selected = 0;
        uint64_t source_epoch = 0;
        BOOL verification = NO;
        for (;;) {
            uint64_t now_ns = clock_gettime_nsec_np(CLOCK_UPTIME_RAW);
            if (now_ns < entered_ns || now_ns >= deadline_ns) return FR_CAP_TIMEOUT;
            os_unfair_lock_lock(&c->lock);
            BOOL stopped = c->stopped || c->source.exhausted;
            if (!stopped && c->latest && fr_cap_source_ready(&c->source, requested)) {
                picture = CVPixelBufferRetain(c->latest);
                selected = c->source.picture;
                source_epoch = c->source.invalidation;
            }
            os_unfair_lock_unlock(&c->lock);
            if (stopped) return FR_CAP_GEOMETRY;
            if (picture) break;
            if (now_ns - entered_ns >= (uint64_t)FR_CAP_VERIFY_WAIT_MS * NSEC_PER_MSEC) {
                live = capture_live(c);
                if (live) return live;
                int result = verify_source(c, deadline, deadline_ns, &picture, &source_epoch);
                if (result) return result;
                if (!picture) return FR_CAP_UNAVAILABLE;
                verification = YES;
                // This image is not the stream's last complete picture.
                // Never use a later stream Idle event as an unchanged proof
                // of this different capture. A stream image must first encode.
                selected = 0;
                break;
            }
            usleep(1000);
        }
        live = capture_live(c);
        if (!live && !source_current(c, source_epoch)) live = FR_CAP_UNAVAILABLE;
        if (live || clock_gettime_nsec_np(CLOCK_UPTIME_RAW) >= deadline_ns) {
            CVPixelBufferRelease(picture);
            return live ? live : FR_CAP_TIMEOUT;
        }
        if (!verification && only_if_changed && !force_idr && selected == c->encoded) {
            CVPixelBufferRelease(picture);
            os_unfair_lock_lock(&c->lock);
            BOOL current = !c->stopped && fr_cap_source_current(&c->source, source_epoch);
            BOOL timely = clock_gettime_nsec_np(CLOCK_UPTIME_RAW) < deadline_ns;
            int result = !current ? FR_CAP_UNAVAILABLE
                                 : timely ? FR_CAP_UNCHANGED : FR_CAP_TIMEOUT;
            os_unfair_lock_unlock(&c->lock);
            return result;
        }
        if (verification && c->verification_encoded == UINT64_MAX) {
            CVPixelBufferRelease(picture);
            return FR_CAP_UNAVAILABLE;
        }
        NSDictionary *options =
            force_idr ? @{(__bridge NSString *)kVTEncodeFrameOptionKey_ForceKeyFrame : @YES} : nil;
        int64_t now = (int64_t)(clock_gettime_nsec_np(CLOCK_UPTIME_RAW) / 1000);
        if (c->last_pts != INT64_MIN && c->last_pts > INT64_MAX - c->interval) {
            CVPixelBufferRelease(picture);
            return FR_CAP_CODEC;
        }
        int64_t pts = c->last_pts == INT64_MIN || now > c->last_pts + c->interval
                          ? now
                          : c->last_pts + c->interval;
        c->unit_limit = capacity;
        for (int attempt = 0;; attempt++) {
            if (!source_current(c, source_epoch)) {
                CVPixelBufferRelease(picture);
                return FR_CAP_UNAVAILABLE;
            }
            if (clock_gettime_nsec_np(CLOCK_UPTIME_RAW) >= deadline_ns) {
                CVPixelBufferRelease(picture);
                return FR_CAP_TIMEOUT;
            }
            c->unit = nil;
            c->dropped = NO;
            OSStatus submitted =
                VTCompressionSessionEncodeFrame(c->session, picture, CMTimeMake(pts, 1000000),
                                                kCMTimeInvalid, (__bridge CFDictionaryRef)options,
                                                NULL, NULL);
            if (submitted != noErr) {
                CVPixelBufferRelease(picture);
                return FR_CAP_CODEC;
            }
            if (dispatch_semaphore_wait(c->output, deadline)) {
                // The original worker supervisor can kill a hung native drain.
                // Releasing the submitted surface before this callback is unsafe.
                VTCompressionSessionCompleteFrames(c->session, kCMTimeInvalid);
                dispatch_semaphore_wait(c->output, DISPATCH_TIME_FOREVER);
                CVPixelBufferRelease(picture);
                return FR_CAP_TIMEOUT;
            }
            c->last_pts = pts;
            if (!c->dropped || attempt == FR_CAP_RESUBMITS) break;
            if (pts > INT64_MAX - c->interval) {
                CVPixelBufferRelease(picture);
                return FR_CAP_CODEC;
            }
            pts += c->interval;
        }
        CVPixelBufferRelease(picture);
        if (c->result != noErr || !c->unit) return FR_CAP_CODEC;
        live = capture_live(c);
        if (!live && !source_current(c, source_epoch)) live = FR_CAP_UNAVAILABLE;
        if (live || clock_gettime_nsec_np(CLOCK_UPTIME_RAW) >= deadline_ns) {
            c->unit = nil;
            return live ? live : FR_CAP_TIMEOUT;
        }
        if (c->unit.length > capacity) return FR_CAP_TOO_LARGE;
        memcpy(buffer, c->unit.bytes, c->unit.length);
        // Publication shares the invalidation lock, so an invalid->complete
        // transition during native encoding/copy cannot revive these pixels.
        os_unfair_lock_lock(&c->lock);
        BOOL current = !c->stopped && fr_cap_source_current(&c->source, source_epoch);
        BOOL timely = clock_gettime_nsec_np(CLOCK_UPTIME_RAW) < deadline_ns;
        if (current && timely) {
            *length = c->unit.length;
            *idr = c->keyframe ? 1 : 0;
            c->encoded = selected;
            if (verification) c->verification_encoded++;
        }
        os_unfair_lock_unlock(&c->lock);
        c->unit = nil;
        if (!current) return FR_CAP_UNAVAILABLE;
        if (!timely) return FR_CAP_TIMEOUT;
    }
    return FR_CAP_OK;
}

// Content-free, cumulative work counters for the original native owner. These
// count API requests and encoded refreshes, not measured GPU time or copies.
int fr_cap_statistics(void *handle, uint64_t *requests, uint64_t *encoded_refreshes,
                       uint32_t *verification_wait_ms) {
    if (!handle || !requests || !encoded_refreshes || !verification_wait_ms)
        return FR_CAP_INVALID;
    FrCapture *c = (__bridge FrCapture *)handle;
    *requests = c->verification_requests;
    *encoded_refreshes = c->verification_encoded;
    *verification_wait_ms = FR_CAP_VERIFY_WAIT_MS;
    return FR_CAP_OK;
}

int fr_cap_set_bitrate(void *handle, uint32_t bitrate) {
    if (!handle || bitrate == 0) return FR_CAP_INVALID;
    FrCapture *c = (__bridge FrCapture *)handle;
    if (!c->session) return FR_CAP_CLOSED;
    return set(c->session, kVTCompressionPropertyKey_AverageBitRate, (__bridge CFTypeRef) @(bitrate))
               ? FR_CAP_OK
               : FR_CAP_CODEC;
}

// FR_CAP_OK while the stream runs on an unchanged display; FR_CAP_GEOMETRY
// once it stopped or the display's mode or placement changed (the capture
// system would otherwise go on scaling the new mode into the old size).
int fr_cap_check(void *handle) {
    if (!handle) return FR_CAP_INVALID;
    FrCapture *c = (__bridge FrCapture *)handle;
    return capture_live(c);
}

void fr_cap_free(void *handle) {
    if (!handle) return;
    @autoreleasepool {
        FrCapture *c = (__bridge_transfer FrCapture *)handle;
        dispatch_semaphore_t done = dispatch_semaphore_create(0);
        [c->stream stopCaptureWithCompletionHandler:^(NSError *e) {
          (void)e;
          dispatch_semaphore_signal(done);
        }];
        dispatch_semaphore_wait(done, after_ms(1000));
        if (c->session) {
            VTCompressionSessionCompleteFrames(c->session, kCMTimeInvalid);
            VTCompressionSessionInvalidate(c->session);
            CFRelease(c->session);
            c->session = NULL;
        }
        os_unfair_lock_lock(&c->lock);
        CVPixelBufferRef latest = c->latest;
        c->latest = NULL;
        os_unfair_lock_unlock(&c->lock);
        if (latest) CVPixelBufferRelease(latest);
    }
}
