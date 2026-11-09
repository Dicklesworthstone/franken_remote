// VideoToolbox HEVC decode into IOSurface-backed, Metal-compatible NV12
// pixel buffers, for the iOS viewer (and its macOS host tests). The decoder
// configuration and per-unit decode mirror fr-native's macOS viewer glue
// (macos_viewer.m): parameter sets already admitted by Rust's HEVC guard,
// synchronous decode, the stream's own range. Nothing is drawn here: the
// decoded buffer is handed back retained, and the app's Metal view draws it.
//
// Plain C over CoreFoundation/CoreMedia/VideoToolbox; no Objective-C runtime.
#include <CoreFoundation/CoreFoundation.h>
#include <CoreMedia/CoreMedia.h>
#include <CoreVideo/CoreVideo.h>
#include <VideoToolbox/VideoToolbox.h>
#include <stdint.h>
#include <stdlib.h>

enum {
    FR_VT_OK = 0,
    FR_VT_INVALID = -1,
    FR_VT_UNAVAILABLE = -2,
    FR_VT_CODEC = -3,
    FR_VT_ALLOCATION = -4,
    FR_VT_GEOMETRY = -5,
    FR_VT_SESSION_LOST = -6,
};

typedef struct {
    CMVideoFormatDescriptionRef format;
    VTDecompressionSessionRef session;
    OSStatus status;
    CVImageBufferRef image;
} FrVtDecoder;

static void fr_vt_decoded(void *refcon, void *frame, OSStatus status, VTDecodeInfoFlags flags,
                          CVImageBufferRef image, CMTime pts, CMTime duration) {
    (void)frame;
    (void)flags;
    (void)pts;
    (void)duration;
    FrVtDecoder *d = (FrVtDecoder *)refcon;
    d->status = status;
    if (d->image) CVPixelBufferRelease(d->image);
    d->image = status == noErr && image ? CVPixelBufferRetain(image) : NULL;
}

static CFDictionaryRef fr_vt_dictionary(const void **keys, const void **values, CFIndex count) {
    return CFDictionaryCreate(kCFAllocatorDefault, keys, values, count,
                              &kCFTypeDictionaryKeyCallBacks, &kCFTypeDictionaryValueCallBacks);
}

void fr_vt_decoder_free(void *handle) {
    FrVtDecoder *d = (FrVtDecoder *)handle;
    if (!d) return;
    if (d->session) {
        VTDecompressionSessionInvalidate(d->session);
        CFRelease(d->session);
    }
    if (d->format) CFRelease(d->format);
    if (d->image) CVPixelBufferRelease(d->image);
    free(d);
}

int fr_vt_decoder_new(const uint8_t *vps, size_t vps_len, const uint8_t *sps, size_t sps_len,
                      const uint8_t *pps, size_t pps_len, uint32_t width, uint32_t height,
                      int full_range, void **out) {
    if (!out || !vps || !sps || !pps || !vps_len || !sps_len || !pps_len) return FR_VT_INVALID;
    *out = NULL;
    FrVtDecoder *d = calloc(1, sizeof(FrVtDecoder));
    if (!d) return FR_VT_ALLOCATION;
    const uint8_t *sets[3] = {vps, sps, pps};
    const size_t sizes[3] = {vps_len, sps_len, pps_len};
    if (CMVideoFormatDescriptionCreateFromHEVCParameterSets(kCFAllocatorDefault, 3, sets, sizes, 4,
                                                            NULL, &d->format) != noErr) {
        fr_vt_decoder_free(d);
        return FR_VT_CODEC;
    }
    CMVideoDimensions dims = CMVideoFormatDescriptionGetDimensions(d->format);
    if ((uint32_t)dims.width != width || (uint32_t)dims.height != height) {
        fr_vt_decoder_free(d);
        return FR_VT_GEOMETRY;
    }
    int32_t pixel = full_range ? kCVPixelFormatType_420YpCbCr8BiPlanarFullRange
                               : kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange;
    CFNumberRef pixel_number = CFNumberCreate(kCFAllocatorDefault, kCFNumberSInt32Type, &pixel);
    CFDictionaryRef surface = fr_vt_dictionary(NULL, NULL, 0);
    const void *attribute_keys[3] = {kCVPixelBufferPixelFormatTypeKey,
                                     kCVPixelBufferMetalCompatibilityKey,
                                     kCVPixelBufferIOSurfacePropertiesKey};
    const void *attribute_values[3] = {pixel_number, kCFBooleanTrue, surface};
    CFDictionaryRef attributes = fr_vt_dictionary(attribute_keys, attribute_values, 3);
    // iOS always decodes HEVC in hardware where it decodes it at all; the
    // macOS key below asks for (does not require) the hardware decoder.
#if TARGET_OS_OSX
    const void *spec_keys[1] = {kVTVideoDecoderSpecification_EnableHardwareAcceleratedVideoDecoder};
    const void *spec_values[1] = {kCFBooleanTrue};
    CFDictionaryRef spec = fr_vt_dictionary(spec_keys, spec_values, 1);
#else
    CFDictionaryRef spec = NULL;
#endif
    VTDecompressionOutputCallbackRecord callback = {fr_vt_decoded, d};
    OSStatus created = VTDecompressionSessionCreate(kCFAllocatorDefault, d->format, spec, attributes,
                                                    &callback, &d->session);
    if (spec) CFRelease(spec);
    CFRelease(attributes);
    CFRelease(surface);
    CFRelease(pixel_number);
    if (created != noErr) {
        d->session = NULL;
        fr_vt_decoder_free(d);
        return FR_VT_UNAVAILABLE;
    }
    VTSessionSetProperty(d->session, kVTDecompressionPropertyKey_RealTime, kCFBooleanTrue);
    *out = d;
    return FR_VT_OK;
}

// 1: hardware decoder, 0: software, -1: VideoToolbox does not say.
int fr_vt_decoder_hardware(void *handle) {
    FrVtDecoder *d = (FrVtDecoder *)handle;
    if (!d) return -1;
    CFBooleanRef value = NULL;
    if (VTSessionCopyProperty(d->session,
                              kVTDecompressionPropertyKey_UsingHardwareAcceleratedVideoDecoder,
                              kCFAllocatorDefault, &value) != noErr ||
        !value)
        return -1;
    int hardware = CFBooleanGetValue(value) ? 1 : 0;
    CFRelease(value);
    return hardware;
}

// Decodes one length-prefixed access unit synchronously. With `image` set,
// the decoded picture is returned retained (+1) for presentation; without,
// it is released (a decode-only unit still advances the reference chain).
int fr_vt_decoder_decode(void *handle, const uint8_t *bytes, size_t len, int64_t pts,
                         void **image) {
    FrVtDecoder *d = (FrVtDecoder *)handle;
    if (!d || !bytes || !len) return FR_VT_INVALID;
    if (image) *image = NULL;
    CMBlockBufferRef block = NULL;
    CMSampleBufferRef sample = NULL;
    if (CMBlockBufferCreateWithMemoryBlock(kCFAllocatorDefault, NULL, len, kCFAllocatorDefault, NULL, 0,
                                           len, kCMBlockBufferAssureMemoryNowFlag,
                                           &block) != kCMBlockBufferNoErr ||
        CMBlockBufferReplaceDataBytes(bytes, block, 0, len) != kCMBlockBufferNoErr) {
        if (block) CFRelease(block);
        return FR_VT_ALLOCATION;
    }
    CMSampleTimingInfo timing = {kCMTimeInvalid, CMTimeMake(pts, 1000000), kCMTimeInvalid};
    const size_t sizes[1] = {len};
    if (CMSampleBufferCreateReady(kCFAllocatorDefault, block, d->format, 1, 1, &timing, 1, sizes,
                                  &sample) != noErr) {
        CFRelease(block);
        return FR_VT_ALLOCATION;
    }
    d->status = noErr;
    if (d->image) {
        CVPixelBufferRelease(d->image);
        d->image = NULL;
    }
    VTDecodeInfoFlags info = 0;
    OSStatus status = VTDecompressionSessionDecodeFrame(d->session, sample, 0, NULL, &info);
    CFRelease(sample);
    CFRelease(block);
    if (status == kVTInvalidSessionErr) return FR_VT_SESSION_LOST;
    if (status != noErr || d->status != noErr || !d->image || (info & kVTDecodeInfo_FrameDropped)) {
        if (d->image) {
            CVPixelBufferRelease(d->image);
            d->image = NULL;
        }
        return FR_VT_CODEC;
    }
    if (image) {
        *image = (void *)d->image;  // ownership moves to the caller
    } else {
        CVPixelBufferRelease(d->image);
    }
    d->image = NULL;
    return FR_VT_OK;
}

void *fr_vt_image_retain(void *image) {
    return image ? (void *)CVPixelBufferRetain((CVPixelBufferRef)image) : NULL;
}

void fr_vt_image_release(void *image) {
    if (image) CVPixelBufferRelease((CVPixelBufferRef)image);
}

// The picture's pixel size and format, for tests and the app's geometry.
int fr_vt_image_describe(void *image, uint32_t *width, uint32_t *height, uint32_t *format,
                         int *io_surface) {
    if (!image || !width || !height || !format || !io_surface) return FR_VT_INVALID;
    CVPixelBufferRef buffer = (CVPixelBufferRef)image;
    *width = (uint32_t)CVPixelBufferGetWidth(buffer);
    *height = (uint32_t)CVPixelBufferGetHeight(buffer);
    *format = (uint32_t)CVPixelBufferGetPixelFormatType(buffer);
    *io_surface = CVPixelBufferGetIOSurface(buffer) != NULL;
    return FR_VT_OK;
}

// Mean luma of the picture's first plane: a content-free check that a test
// decode produced a picture (not a blank buffer). Never logged by the app.
int fr_vt_image_mean_luma(void *image, double *mean) {
    if (!image || !mean) return FR_VT_INVALID;
    CVPixelBufferRef buffer = (CVPixelBufferRef)image;
    if (CVPixelBufferLockBaseAddress(buffer, kCVPixelBufferLock_ReadOnly) != kCVReturnSuccess)
        return FR_VT_UNAVAILABLE;
    const uint8_t *base = CVPixelBufferGetBaseAddressOfPlane(buffer, 0);
    size_t stride = CVPixelBufferGetBytesPerRowOfPlane(buffer, 0);
    size_t w = CVPixelBufferGetWidthOfPlane(buffer, 0);
    size_t h = CVPixelBufferGetHeightOfPlane(buffer, 0);
    double total = 0;
    for (size_t y = 0; base && y < h; y++)
        for (size_t x = 0; x < w; x++) total += base[y * stride + x];
    CVPixelBufferUnlockBaseAddress(buffer, kCVPixelBufferLock_ReadOnly);
    if (!base || !w || !h) return FR_VT_UNAVAILABLE;
    *mean = total / (double)(w * h);
    return FR_VT_OK;
}
