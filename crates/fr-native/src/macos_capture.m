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

typedef struct {
    uint32_t display_id;
    int32_t x;
    int32_t y;
    uint32_t pixel_width;   // the display's own pixels
    uint32_t pixel_height;
    uint32_t point_width;   // its logical size
    uint32_t point_height;
} FrCapDisplay;

static dispatch_time_t after_ms(uint32_t ms) {
    return dispatch_time(DISPATCH_TIME_NOW, (int64_t)ms * (int64_t)NSEC_PER_MSEC);
}

// Capture-system setup is serialized across this user's processes. On macOS
// 27.0.1 a stream start hangs (well past any timeout) while another live
// process of the same user has queried shareable content but not yet started
// its stream; a running stream does not block anyone. So discovery never
// touches the capture system (Core Graphics only), and opening queries the
// content and starts the stream under this lock, as one step. The lock is
// advisory, in the user's private temporary directory, and the kernel drops
// it if the holder dies. Best effort: without it, setup proceeds anyway.
static int setup_lock(uint32_t timeout_ms) {
    char dir[PATH_MAX], path[PATH_MAX];
    size_t n = confstr(_CS_DARWIN_USER_TEMP_DIR, dir, sizeof dir);
    if (n == 0 || n > sizeof dir) return -1;
    if (snprintf(path, sizeof path, "%sfr-capture-setup.lock", dir) >= (int)sizeof path) return -1;
    int fd = open(path, O_RDWR | O_CREAT | O_CLOEXEC | O_NOFOLLOW, 0600);
    if (fd < 0) return -1;
    dispatch_time_t deadline = after_ms(timeout_ms);
    while (flock(fd, LOCK_EX | LOCK_NB)) {
        if (errno != EWOULDBLOCK || dispatch_time(DISPATCH_TIME_NOW, 0) >= deadline) {
            close(fd);
            return -1;
        }
        usleep(2000);
    }
    return fd;
}

// Closing the descriptor releases the lock.
static void setup_unlock(int fd) {
    if (fd >= 0) close(fd);
}

// Shareable content needs the Screen Recording grant; never prompt from here.
static int shareable(uint32_t timeout_ms, SCShareableContent **out) {
    if (!CGPreflightScreenCaptureAccess()) return FR_CAP_PERMISSION;
    __block SCShareableContent *content = nil;
    __block BOOL failed = NO;
    dispatch_semaphore_t done = dispatch_semaphore_create(0);
    [SCShareableContent getShareableContentWithCompletionHandler:^(SCShareableContent *c, NSError *e) {
      content = c;
      failed = e != nil;
      dispatch_semaphore_signal(done);
    }];
    if (dispatch_semaphore_wait(done, after_ms(timeout_ms))) return FR_CAP_TIMEOUT;
    if (failed || !content) return FR_CAP_PERMISSION;
    *out = content;
    return FR_CAP_OK;
}

// The display's current mode and placement, from Core Graphics alone (no
// capture-system round trip), so it is cheap enough for every input check.
static int describe(CGDirectDisplayID id, FrCapDisplay *out) {
    if (!CGDisplayIsActive(id)) return FR_CAP_GEOMETRY;
    CGDisplayModeRef mode = CGDisplayCopyDisplayMode(id);
    if (!mode) return FR_CAP_GEOMETRY;
    size_t pw = CGDisplayModeGetPixelWidth(mode), ph = CGDisplayModeGetPixelHeight(mode);
    CGDisplayModeRelease(mode);
    CGRect bounds = CGDisplayBounds(id);
    if (pw == 0 || ph == 0 || bounds.size.width < 1 || bounds.size.height < 1 || pw > UINT32_MAX ||
        ph > UINT32_MAX)
        return FR_CAP_UNAVAILABLE;
    double scale = (double)pw / bounds.size.width;
    out->display_id = id;
    out->x = (int32_t)lround(bounds.origin.x * scale);
    out->y = (int32_t)lround(bounds.origin.y * scale);
    out->pixel_width = (uint32_t)pw;
    out->pixel_height = (uint32_t)ph;
    out->point_width = (uint32_t)lround(bounds.size.width);
    out->point_height = (uint32_t)lround(bounds.size.height);
    return FR_CAP_OK;
}

static BOOL same_display(const FrCapDisplay *a, const FrCapDisplay *b) {
    return a->display_id == b->display_id && a->x == b->x && a->y == b->y &&
           a->pixel_width == b->pixel_width && a->pixel_height == b->pixel_height &&
           a->point_width == b->point_width && a->point_height == b->point_height;
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

// Takes no ownership of picture. Retaining a selected surface before the
// callback returns keeps its backing pool slot alive; there is one latest
// slot and, while VT runs, one separately retained submitted surface.
static void observe(FrCapture *c, enum FrCapSourceEvent event, uint64_t displayed,
                    uint64_t received, CVPixelBufferRef picture) {
    CVPixelBufferRef previous = NULL;
    os_unfair_lock_lock(&c->lock);
    enum FrCapSourceUpdate update =
        fr_cap_source_observe(&c->source, event, displayed, received);
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
    static const uint8_t start[4] = {0, 0, 0, 1};
    if (!nal || !fr_cap_nal_fits(unit.length, len, limit)) return NO;
    [unit appendBytes:start length:4];
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
            if (sync && c->result == noErr) {
                CMFormatDescriptionRef format = CMSampleBufferGetFormatDescription(sample);
                size_t sets = 0;
                if (!format ||
                    CMVideoFormatDescriptionGetHEVCParameterSetAtIndex(
                        format, 0, NULL, NULL, &sets, NULL) != noErr ||
                    sets != 3)
                    c->result = -1;
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
                destination[prefix] = 0;
                destination[prefix + 1] = 0;
                destination[prefix + 2] = 0;
                destination[prefix + 3] = 1;
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

// Hardware only, low-latency rate control: a two-picture DPB with one
// previous reference. The default mode keeps four references.
static int open_session(FrCapture *c, uint32_t fps, uint32_t bitrate) {
    NSDictionary *spec = @{
        (__bridge NSString *)kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder : @YES,
        (__bridge NSString *)kVTVideoEncoderSpecification_EnableLowLatencyRateControl : @YES,
    };
    VTCompressionSessionRef session = NULL;
    if (VTCompressionSessionCreate(NULL, (int32_t)c->width, (int32_t)c->height, kCMVideoCodecType_HEVC,
                                   (__bridge CFDictionaryRef)spec, NULL, NULL, encoded,
                                   (__bridge void *)c, &session) != noErr)
        return FR_CAP_UNAVAILABLE;
    c->session = session;
    if (!set(session, kVTCompressionPropertyKey_RealTime, kCFBooleanTrue) ||
        !set(session, kVTCompressionPropertyKey_AllowFrameReordering, kCFBooleanFalse) ||
        !set(session, kVTCompressionPropertyKey_ProfileLevel, kVTProfileLevel_HEVC_Main_AutoLevel) ||
        !set(session, kVTCompressionPropertyKey_ExpectedFrameRate, (__bridge CFTypeRef) @(fps)) ||
        !set(session, kVTCompressionPropertyKey_AverageBitRate, (__bridge CFTypeRef) @(bitrate)) ||
        // IDRs come only on request (start, recovery).
        !set(session, kVTCompressionPropertyKey_MaxKeyFrameInterval, (__bridge CFTypeRef) @(INT32_MAX)) ||
        !set(session, kVTCompressionPropertyKey_ColorPrimaries, kCVImageBufferColorPrimaries_ITU_R_709_2) ||
        !set(session, kVTCompressionPropertyKey_TransferFunction, kCVImageBufferTransferFunction_ITU_R_709_2) ||
        !set(session, kVTCompressionPropertyKey_YCbCrMatrix, kCVImageBufferYCbCrMatrix_ITU_R_709_2) ||
        !set(session, kVTCompressionPropertyKey_ReferenceBufferCount, (__bridge CFTypeRef) @1) ||
        VTCompressionSessionPrepareToEncodeFrames(session) != noErr) {
        close_session(c);
        return FR_CAP_UNAVAILABLE;
    }
    return FR_CAP_OK;
}

static int open_capture(uint32_t display_id, uint32_t width, uint32_t height, uint32_t fps,
                        uint32_t bitrate, void **out) {
    @autoreleasepool {
        SCShareableContent *content = nil;
        int status = shareable(1500, &content);
        if (status) return status;
        SCDisplay *display = nil;
        for (SCDisplay *d in content.displays)
            if (d.displayID == display_id) display = d;
        if (!display) return FR_CAP_GEOMETRY;

        FrCapture *c = [FrCapture new];
        c->lock = OS_UNFAIR_LOCK_INIT;
        status = describe(display_id, &c->display);
        if (status) return status;
        c->width = width;
        c->height = height;
        c->output = dispatch_semaphore_create(0);
        c->interval = 1000000 / (int64_t)fps;
        c->last_pts = INT64_MIN;

        SCContentFilter *filter = [[SCContentFilter alloc] initWithDisplay:display excludingWindows:@[]];
        SCStreamConfiguration *config = [SCStreamConfiguration new];
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
        c->filter = filter;
        c->configuration = config;
        c->stream = [[SCStream alloc] initWithFilter:filter configuration:config delegate:c];
        NSError *error = nil;
        if (![c->stream addStreamOutput:c type:SCStreamOutputTypeScreen sampleHandlerQueue:c->queue
                                  error:&error])
            return FR_CAP_UNAVAILABLE;
        // Start capturing first and set up the encoder while the capture
        // system starts: the two are independent until the first encode.
        __block BOOL started = NO;
        dispatch_semaphore_t done = dispatch_semaphore_create(0);
        [c->stream startCaptureWithCompletionHandler:^(NSError *e) {
          started = e == nil;
          dispatch_semaphore_signal(done);
        }];
        status = open_session(c, fps, bitrate);
        BOOL waited = dispatch_semaphore_wait(done, after_ms(1500)) == 0;
        if (status || !waited || !started) {
            if (waited && started) {
                dispatch_semaphore_t stopped = dispatch_semaphore_create(0);
                [c->stream stopCaptureWithCompletionHandler:^(NSError *e) {
                  (void)e;
                  dispatch_semaphore_signal(stopped);
                }];
                dispatch_semaphore_wait(stopped, after_ms(1000));
            }
            close_session(c);
            if (status) return status;
            return waited ? FR_CAP_PERMISSION : FR_CAP_TIMEOUT;
        }
        *out = (__bridge_retained void *)c;
    }
    return FR_CAP_OK;
}

int fr_cap_open(uint32_t display_id, uint32_t width, uint32_t height, uint32_t fps, uint32_t bitrate,
                void **out) {
    if (!out || width < 16 || height < 16 || (width | height) & 1 || fps < 1 || fps > 240 ||
        bitrate == 0)
        return FR_CAP_INVALID;
    *out = NULL;
    // A live static source needs the bounded verification path. Never claim
    // that an older runtime can prove freshness by retaining the last pixels.
    if (@available(macOS 14.0, *)) {
    } else {
        return FR_CAP_UNAVAILABLE;
    }
    int lock = setup_lock(2000);
    int status = open_capture(display_id, width, height, fps, bitrate, out);
    setup_unlock(lock);
    return status;
}

// Low-latency rate control skips a picture that would overrun its budget,
// typically the first after a large IDR. Each resubmission one interval
// later releases one frame's budget; past this many the request fails.
#define FR_CAP_RESUBMITS 8

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
