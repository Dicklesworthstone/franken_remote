// Shared VideoToolbox output custody for the desktop and mobile Apple decoders.
// Include after CoreMedia, CoreVideo, IOSurface and VideoToolbox. The production
// callback is also exercised by portable tests with explicit native API doubles.
//
// DecodeFrame is called with both asynchronous and temporal-processing flags
// clear: Apple guarantees that its callback completes before the call returns.
// The persistent owner and its lock also refuse unexpected late/extra callbacks
// without a stack refcon, an image overwrite, or a race with retirement. Close
// admission, drain/invalidate the session, THEN destroy this owner. Never hold
// this lock across decode, draining, presentation or a final image release.
#ifndef FR_APPLE_DECODE_OUTPUT_H
#define FR_APPLE_DECODE_OUTPUT_H

#include <pthread.h>
#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

enum FrAppleOutputError {
    FR_APPLE_OUTPUT_OK = 0,
    FR_APPLE_OUTPUT_CODEC = 1,
    FR_APPLE_OUTPUT_GEOMETRY = 2,
};

typedef struct {
    pthread_mutex_t lock;
    bool initialized;
    bool active;
    bool received;
    int64_t token;
    uint32_t width;
    uint32_t height;
    OSType format;
    size_t max_stride;
    size_t max_bytes;
    enum FrAppleOutputError error;
    CVPixelBufferRef image;
} FrAppleDecodeOutput;

// The owner must be zero-initialized and is initialized only once per session.
// The admitted image may use 512-byte rows and 64-line native padding, plus
// at most 64 KiB of IOSurface alignment/metadata. A larger native allocation
// requires an explicitly budgeted backend contract, not an unchecked retain.
static inline bool fr_apple_output_init(FrAppleDecodeOutput *output, uint32_t width,
                                        uint32_t height, bool full_range) {
    if (!width || !height || (width & 1) || (height & 1) ||
        (uint64_t)width + 511 > SIZE_MAX || (uint64_t)height + 63 > SIZE_MAX)
        return false;
    size_t stride = ((size_t)width + 511) & ~(size_t)511;
    size_t luma_rows = ((size_t)height + 63) & ~(size_t)63;
    size_t chroma_rows = ((size_t)height / 2 + 63) & ~(size_t)63;
    // Each plane may be padded independently. Halving the padded luma height
    // can underbudget a valid chroma plane (for example, 3840 x 1080).
    if (luma_rows > SIZE_MAX - chroma_rows) return false;
    size_t rows = luma_rows + chroma_rows;
    if (rows > (SIZE_MAX - 65536) / stride) return false;
    if (pthread_mutex_init(&output->lock, NULL) != 0) return false;
    output->initialized = true;
    output->width = width;
    output->height = height;
    output->format = full_range ? kCVPixelFormatType_420YpCbCr8BiPlanarFullRange
                                : kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange;
    output->max_stride = stride;
    output->max_bytes = stride * rows + 65536;
    return true;
}

static inline bool fr_apple_output_begin(FrAppleDecodeOutput *output, int64_t token) {
    if (!output->initialized) return false;
    pthread_mutex_lock(&output->lock);
    bool accepted = output->error == FR_APPLE_OUTPUT_OK && !output->active &&
                    token > output->token && token > 0;
    if (accepted) {
        output->token = token;
        output->active = true;
        output->received = false;
    } else {
        output->error = FR_APPLE_OUTPUT_CODEC;
    }
    pthread_mutex_unlock(&output->lock);
    return accepted;
}

static inline bool fr_apple_output_flags(VTDecodeInfoFlags flags) {
    // Modifiable is permission, not an instruction; decoded pixels remain
    // immutable. Dropped, asynchronous, leading-frame and unknown flags fail.
    return (flags & ~(VTDecodeInfoFlags)kVTDecodeInfo_ImageBufferModifiable) == 0;
}

static inline bool fr_apple_output_geometry(const FrAppleDecodeOutput *output,
                                            CVPixelBufferRef image) {
    if (CVPixelBufferGetWidth(image) != output->width ||
        CVPixelBufferGetHeight(image) != output->height ||
        CVPixelBufferGetPixelFormatType(image) != output->format ||
        !CVPixelBufferIsPlanar(image) || CVPixelBufferGetPlaneCount(image) != 2)
        return false;
    IOSurfaceRef surface = CVPixelBufferGetIOSurface(image);
    if (!surface) return false;
    size_t bytes = IOSurfaceGetAllocSize(surface);
    if (!bytes || bytes > output->max_bytes) return false;
    size_t required = 0;
    for (size_t plane = 0; plane < 2; plane++) {
        size_t divisor = plane == 0 ? 1 : 2;
        size_t stride = CVPixelBufferGetBytesPerRowOfPlane(image, plane);
        if (CVPixelBufferGetWidthOfPlane(image, plane) != output->width / divisor ||
            CVPixelBufferGetHeightOfPlane(image, plane) != output->height / divisor ||
            stride < output->width || stride > output->max_stride || (stride & 1))
            return false;
        size_t rows = output->height / divisor;
        if (rows > (bytes - required) / stride) return false;
        required += rows * stride;
    }
    return true;
}

// `refcon` and the expected `frame` both name this session-owned object. The
// original frame's positive, strictly increasing token is carried in CMTime;
// equal capture timestamps cannot alias it. No pointer is made from an integer.
static inline void fr_apple_output_callback(void *refcon, void *frame, OSStatus status,
                                             VTDecodeInfoFlags flags, CVImageBufferRef image,
                                             CMTime pts, CMTime duration) {
    (void)duration;
    FrAppleDecodeOutput *output = (FrAppleDecodeOutput *)refcon;
    bool usable = status == noErr && image && fr_apple_output_flags(flags);
    bool geometry = usable && fr_apple_output_geometry(output, image);
    pthread_mutex_lock(&output->lock);
    if (!output->active || output->received || frame != output || !usable ||
        !CMTIME_IS_NUMERIC(pts) || pts.epoch != 0 ||
        (pts.flags & kCMTimeFlags_HasBeenRounded) != 0 ||
        CMTimeCompare(pts, CMTimeMake(output->token, 1)) != 0) {
        output->error = FR_APPLE_OUTPUT_CODEC;
    } else if (!geometry) {
        output->error = FR_APPLE_OUTPUT_GEOMETRY;
    } else if (output->error == FR_APPLE_OUTPUT_OK) {
        output->image = CVPixelBufferRetain(image);
    }
    // The first failure is sticky for this decoder. A later successful
    // callback never repairs it or replaces a picture already retained.
    output->received = true;
    pthread_mutex_unlock(&output->lock);
}

// Transfer exactly one picture only after the original synchronous call has
// completed successfully. Refused images are detached under the lock and
// released after unlocking, including success followed by an extra callback.
static inline enum FrAppleOutputError fr_apple_output_take(FrAppleDecodeOutput *output,
                                                           OSStatus status,
                                                           VTDecodeInfoFlags flags,
                                                           CVPixelBufferRef *image) {
    *image = NULL;
    pthread_mutex_lock(&output->lock);
    if (!output->active || !output->received || status != noErr ||
        !fr_apple_output_flags(flags))
        output->error = FR_APPLE_OUTPUT_CODEC;
    if (!output->image && output->error == FR_APPLE_OUTPUT_OK)
        output->error = FR_APPLE_OUTPUT_CODEC;
    output->active = false;
    CVPixelBufferRef retained = output->image;
    output->image = NULL;
    enum FrAppleOutputError error = output->error;
    pthread_mutex_unlock(&output->lock);
    if (error == FR_APPLE_OUTPUT_OK) {
        *image = retained;
    } else if (retained) {
        CVPixelBufferRelease(retained);
    }
    return error;
}

static inline void fr_apple_output_close(FrAppleDecodeOutput *output) {
    if (!output->initialized) return;
    pthread_mutex_lock(&output->lock);
    output->active = false;
    output->error = FR_APPLE_OUTPUT_CODEC;
    CVPixelBufferRef image = output->image;
    output->image = NULL;
    pthread_mutex_unlock(&output->lock);
    if (image) CVPixelBufferRelease(image);
}

// Only after VideoToolbox has drained and the session has been invalidated.
static inline void fr_apple_output_destroy(FrAppleDecodeOutput *output) {
    if (!output->initialized) return;
    fr_apple_output_close(output);
    pthread_mutex_destroy(&output->lock);
    output->initialized = false;
}

#endif
