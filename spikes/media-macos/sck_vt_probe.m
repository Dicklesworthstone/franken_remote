// Phase 0 probe (fr-p0-media-macos-4mj): ScreenCaptureKit display capture ->
// VideoToolbox HEVC with the settings the macOS media worker needs.
//
//   /usr/bin/cc -fobjc-arc -O2 -o sck_vt_probe sck_vt_probe.m -framework Foundation \
//     -framework ScreenCaptureKit -framework VideoToolbox -framework CoreMedia \
//     -framework CoreVideo -framework CoreGraphics -framework ApplicationServices
//   ./sck_vt_probe OUT.hevc [WIDTH HEIGHT FRAMES IDR_AT MBPS]
//
// OUT.hevc is Annex B with an access unit delimiter per picture (type 0 for
// IDR, 1 for P, as admission requires) and VPS/SPS/PPS before each IDR, so it
// plays in `cargo run -p fr-native --no-default-features --features
// macos-desktop --example macos_present -- OUT.hevc --fps 60`, and FFmpeg
// decodes it. Pictures are encoded from the latest complete capture at 60 Hz,
// so a still screen still yields P pictures. IDR_AT forces a second IDR at
// that frame (0 = none). Prints property statuses, effective readback, time to
// first packet and encode latency. OUT.hevc.nv12 receives the last submitted
// source picture (tightly packed NV12), for a PSNR against the decoded last
// frame.
#import <ApplicationServices/ApplicationServices.h>
#import <CoreMedia/CoreMedia.h>
#import <Foundation/Foundation.h>
#import <ScreenCaptureKit/ScreenCaptureKit.h>
#import <VideoToolbox/VideoToolbox.h>
#include <mach/mach_time.h>
#include <stdio.h>

static FILE *out;
static int target_frames, submitted, written, idrs, failed_outputs;
static uint64_t submit_at[100000], capture_started_at;
static CVPixelBufferRef last_submitted;
static double lat_sum, lat_max, first_packet_ms = -1;
static size_t bytes_sum, bytes_max, idr_bytes_max;
static mach_timebase_info_data_t tb;
static dispatch_semaphore_t done;

static double ms(uint64_t ticks) { return (double)ticks * tb.numer / tb.denom / 1e6; }

static void put(NSMutableData *au, const uint8_t *nal, size_t len) {
    static const uint8_t sc[4] = {0, 0, 0, 1};
    [au appendBytes:sc length:4];
    [au appendBytes:nal length:len];
}

static void encoded(void *ref, void *frame, OSStatus status, VTEncodeInfoFlags flags, CMSampleBufferRef sb) {
    (void)ref;
    int n = (int)(intptr_t)frame;
    uint64_t now = mach_absolute_time();
    if (status != noErr || !sb || (flags & kVTEncodeInfo_FrameDropped)) {
        failed_outputs++;
        return;
    }
    if (first_packet_ms < 0) first_packet_ms = ms(now - capture_started_at);
    double latency = ms(now - submit_at[n]);
    CFArrayRef attachments = CMSampleBufferGetSampleAttachmentsArray(sb, false);
    bool sync = true;
    if (attachments && CFArrayGetCount(attachments) > 0) {
        CFDictionaryRef a = CFArrayGetValueAtIndex(attachments, 0);
        CFBooleanRef notSync = CFDictionaryGetValue(a, kCMSampleAttachmentKey_NotSync);
        sync = !(notSync && CFBooleanGetValue(notSync));
    }
    NSMutableData *au = [NSMutableData data];
    const uint8_t aud[3] = {0x46, 0x01, sync ? 0x10 : 0x30};
    put(au, aud, sizeof aud);
    if (sync) {
        CMFormatDescriptionRef fmt = CMSampleBufferGetFormatDescription(sb);
        size_t count = 0;
        CMVideoFormatDescriptionGetHEVCParameterSetAtIndex(fmt, 0, NULL, NULL, &count, NULL);
        for (size_t i = 0; i < count; i++) {
            const uint8_t *p = NULL;
            size_t len = 0;
            if (CMVideoFormatDescriptionGetHEVCParameterSetAtIndex(fmt, i, &p, &len, NULL, NULL) == noErr) put(au, p, len);
        }
    }
    CMBlockBufferRef block = CMSampleBufferGetDataBuffer(sb);
    size_t total = 0;
    char *data = NULL;
    if (CMBlockBufferGetDataPointer(block, 0, NULL, &total, &data) != noErr) {
        failed_outputs++;
        return;
    }
    for (size_t off = 0; off + 4 <= total;) {
        uint32_t len = ((uint32_t)(uint8_t)data[off] << 24) | ((uint32_t)(uint8_t)data[off + 1] << 16) |
                       ((uint32_t)(uint8_t)data[off + 2] << 8) | (uint8_t)data[off + 3];
        off += 4;
        if (off + len > total) break;
        put(au, (const uint8_t *)data + off, len);
        off += len;
    }
    fwrite(au.bytes, 1, au.length, out);
    written++;
    idrs += sync;
    lat_sum += latency;
    if (latency > lat_max) lat_max = latency;
    bytes_sum += au.length;
    if (au.length > bytes_max) bytes_max = au.length;
    if (sync && au.length > idr_bytes_max) idr_bytes_max = au.length;
}

@interface Sink : NSObject <SCStreamOutput>
@property(atomic) CVPixelBufferRef latest;
@property(atomic) int complete;
@end
@implementation Sink
- (void)stream:(SCStream *)stream didOutputSampleBuffer:(CMSampleBufferRef)sb ofType:(SCStreamOutputType)type {
    (void)stream;
    if (type != SCStreamOutputTypeScreen) return;
    CFArrayRef attachments = CMSampleBufferGetSampleAttachmentsArray(sb, false);
    if (!attachments || CFArrayGetCount(attachments) == 0) return;
    CFDictionaryRef a = CFArrayGetValueAtIndex(attachments, 0);
    NSNumber *status = ((__bridge NSDictionary *)a)[SCStreamFrameInfoStatus];
    if (status.integerValue != SCFrameStatusComplete) return;
    CVPixelBufferRef pb = CMSampleBufferGetImageBuffer(sb);
    if (!pb) return;
    CVPixelBufferRetain(pb);
    CVPixelBufferRef old = self.latest;
    self.latest = pb;
    if (old) CVPixelBufferRelease(old);
    self.complete++;
}
@end

static void set(VTCompressionSessionRef s, CFStringRef key, CFTypeRef value, const char *name) {
    OSStatus st = VTSessionSetProperty(s, key, value);
    printf("property %-28s %s (%d)\n", name, st == noErr ? "ok" : "REFUSED", (int)st);
}

static void readback(VTCompressionSessionRef s, CFStringRef key, const char *name) {
    CFTypeRef value = NULL;
    OSStatus st = VTSessionCopyProperty(s, key, kCFAllocatorDefault, &value);
    printf("readback %-28s %s\n", name, st == noErr && value ? [[(__bridge id)value description] UTF8String] : "unavailable");
    if (value) CFRelease(value);
}

int main(int argc, char **argv) {
    @autoreleasepool {
        if (argc < 2) {
            fprintf(stderr, "usage: sck_vt_probe OUT.hevc [WIDTH HEIGHT FRAMES IDR_AT]\n");
            return 2;
        }
        mach_timebase_info(&tb);
        out = fopen(argv[1], "wb");
        if (!out) return 2;
        int w = argc > 2 ? atoi(argv[2]) : 1920, h = argc > 3 ? atoi(argv[3]) : 1080;
        target_frames = argc > 4 ? atoi(argv[4]) : 240;
        int idr_at = argc > 5 ? atoi(argv[5]) : 0;
        int mbps = argc > 6 ? atoi(argv[6]) : 8;
        if (target_frames < 1 || target_frames > 100000) return 2;
        printf("screen_recording_preflight=%d accessibility_trusted=%d\n", CGPreflightScreenCaptureAccess(), AXIsProcessTrusted());
        done = dispatch_semaphore_create(0);

        __block SCDisplay *display = nil;
        dispatch_semaphore_t got = dispatch_semaphore_create(0);
        [SCShareableContent getShareableContentWithCompletionHandler:^(SCShareableContent *content, NSError *error) {
            if (error) printf("shareable content error: %s\n", error.localizedDescription.UTF8String);
            for (SCDisplay *d in content.displays)
                if (d.displayID == CGMainDisplayID()) display = d;
            dispatch_semaphore_signal(got);
        }];
        dispatch_semaphore_wait(got, DISPATCH_TIME_FOREVER);
        if (!display) {
            printf("no main display\n");
            return 1;
        }
        printf("main display %u: %ld x %ld points, encoding %d x %d\n", display.displayID, (long)display.width, (long)display.height, w, h);

        // Low-latency rate control is what yields a two-picture DPB with one
        // previous reference; the default HEVC mode refuses
        // ReferenceBufferCount and keeps four references (refused: DpbLimit).
        NSDictionary *spec = @{
            (__bridge NSString *)kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder: @YES,
            (__bridge NSString *)kVTVideoEncoderSpecification_EnableLowLatencyRateControl: @YES,
        };
        VTCompressionSessionRef session = NULL;
        OSStatus st = VTCompressionSessionCreate(NULL, w, h, kCMVideoCodecType_HEVC, (__bridge CFDictionaryRef)spec, NULL, NULL, encoded, NULL, &session);
        printf("VTCompressionSessionCreate = %d\n", (int)st);
        if (st != noErr) return 1;
        set(session, kVTCompressionPropertyKey_RealTime, kCFBooleanTrue, "RealTime");
        set(session, kVTCompressionPropertyKey_AllowFrameReordering, kCFBooleanFalse, "AllowFrameReordering=false");
        set(session, kVTCompressionPropertyKey_ProfileLevel, kVTProfileLevel_HEVC_Main_AutoLevel, "ProfileLevel=Main");
        set(session, kVTCompressionPropertyKey_ExpectedFrameRate, (__bridge CFTypeRef)@60, "ExpectedFrameRate=60");
        set(session, kVTCompressionPropertyKey_AverageBitRate, (__bridge CFTypeRef)@(mbps * 1000000), "AverageBitRate");
        set(session, kVTCompressionPropertyKey_MaxKeyFrameInterval, (__bridge CFTypeRef)@3600, "MaxKeyFrameInterval=3600");
        set(session, kVTCompressionPropertyKey_ColorPrimaries, kCVImageBufferColorPrimaries_ITU_R_709_2, "ColorPrimaries=709");
        set(session, kVTCompressionPropertyKey_TransferFunction, kCVImageBufferTransferFunction_ITU_R_709_2, "TransferFunction=709");
        set(session, kVTCompressionPropertyKey_YCbCrMatrix, kCVImageBufferYCbCrMatrix_ITU_R_709_2, "YCbCrMatrix=709");
        set(session, kVTCompressionPropertyKey_ReferenceBufferCount, (__bridge CFTypeRef)@1, "ReferenceBufferCount=1");
        // SPEED=1 in the environment: trade encoder effort for latency.
        if (getenv("SPEED") && atoi(getenv("SPEED"))) {
            set(session, kVTCompressionPropertyKey_PrioritizeEncodingSpeedOverQuality, kCFBooleanTrue, "PrioritizeSpeed");
            set(session, kVTCompressionPropertyKey_MaximizePowerEfficiency, kCFBooleanFalse, "MaximizePowerEfficiency=false");
        }
        VTCompressionSessionPrepareToEncodeFrames(session);
        readback(session, kVTCompressionPropertyKey_UsingHardwareAcceleratedVideoEncoder, "UsingHardwareEncoder");
        readback(session, kVTCompressionPropertyKey_ReferenceBufferCount, "ReferenceBufferCount");
        readback(session, kVTCompressionPropertyKey_AllowFrameReordering, "AllowFrameReordering");

        SCContentFilter *filter = [[SCContentFilter alloc] initWithDisplay:display excludingWindows:@[]];
        SCStreamConfiguration *cfg = [SCStreamConfiguration new];
        cfg.width = w;
        cfg.height = h;
        cfg.pixelFormat = kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange;
        cfg.colorMatrix = kCGDisplayStreamYCbCrMatrix_ITU_R_709_2;
        cfg.colorSpaceName = kCGColorSpaceITUR_709;
        cfg.minimumFrameInterval = CMTimeMake(1, 60);
        cfg.queueDepth = 4;
        cfg.showsCursor = NO;
        Sink *sink = [Sink new];
        SCStream *stream = [[SCStream alloc] initWithFilter:filter configuration:cfg delegate:nil];
        dispatch_queue_t q = dispatch_queue_create("capture", DISPATCH_QUEUE_SERIAL);
        NSError *err = nil;
        [stream addStreamOutput:sink type:SCStreamOutputTypeScreen sampleHandlerQueue:q error:&err];
        if (err) {
            printf("addStreamOutput: %s\n", err.localizedDescription.UTF8String);
            return 1;
        }
        __block NSError *startError = nil;
        dispatch_semaphore_t started = dispatch_semaphore_create(0);
        capture_started_at = mach_absolute_time();
        [stream startCaptureWithCompletionHandler:^(NSError *e) {
            startError = e;
            dispatch_semaphore_signal(started);
        }];
        dispatch_semaphore_wait(started, DISPATCH_TIME_FOREVER);
        if (startError) {
            printf("startCapture: %s (code %ld)\n", startError.localizedDescription.UTF8String, (long)startError.code);
            return 1;
        }
        printf("capture started in %.1f ms\n", ms(mach_absolute_time() - capture_started_at));

        dispatch_queue_t eq = dispatch_queue_create("encode", DISPATCH_QUEUE_SERIAL);
        dispatch_source_t timer = dispatch_source_create(DISPATCH_SOURCE_TYPE_TIMER, 0, 0, eq);
        dispatch_source_set_timer(timer, dispatch_time(DISPATCH_TIME_NOW, 0), NSEC_PER_SEC / 60, NSEC_PER_MSEC);
        dispatch_source_set_event_handler(timer, ^{
            CVPixelBufferRef pb = sink.latest;
            if (!pb || submitted >= target_frames) return;
            CVPixelBufferRetain(pb);
            int n = submitted++;
            NSDictionary *opts = (n == 0 || (idr_at > 0 && n == idr_at)) ? @{(__bridge NSString *)kVTEncodeFrameOptionKey_ForceKeyFrame: @YES} : nil;
            submit_at[n] = mach_absolute_time();
            VTCompressionSessionEncodeFrame(session, pb, CMTimeMake(n, 60), kCMTimeInvalid, (__bridge CFDictionaryRef)opts, (void *)(intptr_t)n, NULL);
            if (last_submitted) CVPixelBufferRelease(last_submitted);
            last_submitted = pb;
            if (submitted >= target_frames) {
                dispatch_source_cancel(timer);
                dispatch_semaphore_signal(done);
            }
        });
        dispatch_resume(timer);
        dispatch_semaphore_wait(done, dispatch_time(DISPATCH_TIME_NOW, (int64_t)(target_frames / 30 + 10) * (int64_t)NSEC_PER_SEC));
        VTCompressionSessionCompleteFrames(session, kCMTimeInvalid);
        dispatch_semaphore_t stopped = dispatch_semaphore_create(0);
        [stream stopCaptureWithCompletionHandler:^(NSError *e) {
            (void)e;
            dispatch_semaphore_signal(stopped);
        }];
        dispatch_semaphore_wait(stopped, dispatch_time(DISPATCH_TIME_NOW, 5 * NSEC_PER_SEC));
        fclose(out);
        if (last_submitted) {
            NSString *raw = [NSString stringWithFormat:@"%s.nv12", argv[1]];
            FILE *f = fopen(raw.UTF8String, "wb");
            CVPixelBufferLockBaseAddress(last_submitted, kCVPixelBufferLock_ReadOnly);
            for (size_t plane = 0; f && plane < 2; plane++) {
                const uint8_t *base = CVPixelBufferGetBaseAddressOfPlane(last_submitted, plane);
                size_t stride = CVPixelBufferGetBytesPerRowOfPlane(last_submitted, plane);
                size_t rows = CVPixelBufferGetHeightOfPlane(last_submitted, plane);
                size_t row_bytes = CVPixelBufferGetWidthOfPlane(last_submitted, plane) * (plane ? 2 : 1);
                for (size_t y = 0; y < rows; y++) fwrite(base + y * stride, 1, row_bytes, f);
            }
            CVPixelBufferUnlockBaseAddress(last_submitted, kCVPixelBufferLock_ReadOnly);
            if (f) fclose(f);
        }
        printf("captured_complete=%d submitted=%d written=%d idr=%d failed=%d\n", sink.complete, submitted, written, idrs, failed_outputs);
        if (written) {
            printf("first packet %.1f ms after capture start\n", first_packet_ms);
            printf("encode latency avg %.2f ms max %.2f ms; AU avg %zu B max %zu B, IDR max %zu B\n", lat_sum / written, lat_max, bytes_sum / written, bytes_max, idr_bytes_max);
        }
        return written == target_frames && failed_outputs == 0 ? 0 : 1;
    }
}
