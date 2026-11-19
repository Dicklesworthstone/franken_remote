// Runs the exact production output callback. Apple APIs below are explicit
// doubles: these tests establish custody, not native decode/presentation.
#include <assert.h>
#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>
#include <stdatomic.h>
#include <stdio.h>

typedef int32_t OSStatus;
typedef uint32_t OSType;
typedef uint32_t VTDecodeInfoFlags;
enum {
    noErr = 0,
    kVTDecodeInfo_Asynchronous = 1,
    kVTDecodeInfo_FrameDropped = 2,
    kVTDecodeInfo_ImageBufferModifiable = 4,
    kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange = 0x34323076,
    kCVPixelFormatType_420YpCbCr8BiPlanarFullRange = 0x34323066,
    kCMTimeFlags_Valid = 1,
    kCMTimeFlags_HasBeenRounded = 2,
};
typedef struct {
    int64_t value;
    int32_t timescale;
    uint32_t flags;
    int64_t epoch;
} CMTime;
#define CMTIME_IS_NUMERIC(t) (((t).flags & 1) && !((t).flags & 28) && (t).timescale > 0)
static const CMTime kCMTimeInvalid = {0};
static CMTime CMTimeMake(int64_t value, int32_t scale) {
    return (CMTime){value, scale, kCMTimeFlags_Valid, 0};
}
static int CMTimeCompare(CMTime left, CMTime right) {
    __int128 a = (__int128)left.value * right.timescale;
    __int128 b = (__int128)right.value * left.timescale;
    return (a > b) - (a < b);
}

typedef struct {
    atomic_int references;
    size_t width, height, planes;
    OSType format;
    bool planar, surface;
    size_t plane_width[2], plane_height[2], stride[2], bytes;
} TestImage;
typedef TestImage *CVPixelBufferRef;
typedef TestImage *CVImageBufferRef;
typedef TestImage *IOSurfaceRef;

static size_t CVPixelBufferGetWidth(CVPixelBufferRef image) { return image->width; }
static size_t CVPixelBufferGetHeight(CVPixelBufferRef image) { return image->height; }
static OSType CVPixelBufferGetPixelFormatType(CVPixelBufferRef image) { return image->format; }
static bool CVPixelBufferIsPlanar(CVPixelBufferRef image) { return image->planar; }
static size_t CVPixelBufferGetPlaneCount(CVPixelBufferRef image) { return image->planes; }
static IOSurfaceRef CVPixelBufferGetIOSurface(CVPixelBufferRef image) {
    return image->surface ? image : NULL;
}
static size_t IOSurfaceGetAllocSize(IOSurfaceRef surface) { return surface->bytes; }
static size_t CVPixelBufferGetBytesPerRowOfPlane(CVPixelBufferRef image, size_t plane) {
    assert(plane < image->planes && plane < 2);
    return image->stride[plane];
}
static size_t CVPixelBufferGetWidthOfPlane(CVPixelBufferRef image, size_t plane) {
    assert(plane < image->planes && plane < 2);
    return image->plane_width[plane];
}
static size_t CVPixelBufferGetHeightOfPlane(CVPixelBufferRef image, size_t plane) {
    assert(plane < image->planes && plane < 2);
    return image->plane_height[plane];
}
static CVPixelBufferRef CVPixelBufferRetain(CVPixelBufferRef image) {
    assert(atomic_fetch_add(&image->references, 1) >= 1);
    return image;
}
static void CVPixelBufferRelease(CVPixelBufferRef image) {
    // The fixture retains its original reference for the entire test.
    assert(atomic_fetch_sub(&image->references, 1) > 1);
}

#include "../../fr-native/src/apple_decode_output.h"

static TestImage picture(void) {
    return (TestImage){
        .references = 1, .width = 128, .height = 128, .planes = 2,
        .format = kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
        .planar = true, .surface = true,
        .plane_width = {128, 64}, .plane_height = {128, 64},
        .stride = {128, 128}, .bytes = 128 * 128 * 3 / 2,
    };
}
static void callback(FrAppleDecodeOutput *output, TestImage *image, int64_t token) {
    fr_apple_output_callback(output, output, noErr, 0, image, CMTimeMake(token, 1),
                             kCMTimeInvalid);
}
static void refused(FrAppleDecodeOutput *output, enum FrAppleOutputError expected) {
    CVPixelBufferRef image = NULL;
    assert(fr_apple_output_take(output, noErr, 0, &image) == expected);
    assert(image == NULL);
    assert(!fr_apple_output_begin(output, INT64_MAX));
    fr_apple_output_destroy(output);
}

static void exactly_one_original_picture_transfers_once(void) {
    FrAppleDecodeOutput output = {0};
    TestImage images[3] = {picture(), picture(), picture()};
    CVPixelBufferRef retained[3] = {NULL};
    assert(fr_apple_output_init(&output, 128, 128, false));
    for (int64_t token = 1; token <= 3; token++) {
        size_t index = (size_t)token - 1;
        assert(fr_apple_output_begin(&output, token));
        callback(&output, &images[index], token);
        assert(atomic_load(&images[index].references) == 2);
        assert(fr_apple_output_take(&output, noErr, 0, &retained[index]) == FR_APPLE_OUTPUT_OK);
        assert(retained[index] == &images[index]);
        // A renderer may still own each earlier output while the next decode
        // runs. Neither a new input nor decoder retirement releases its hold.
        for (size_t earlier = 0; earlier <= index; earlier++)
            assert(atomic_load(&images[earlier].references) == 2);
    }
    refused(&output, FR_APPLE_OUTPUT_CODEC); // Taking twice cannot publish twice.
    for (size_t i = 0; i < 3; i++) {
        assert(atomic_load(&images[i].references) == 2);
        CVPixelBufferRelease(retained[i]);
        assert(atomic_load(&images[i].references) == 1);
    }
}

static void extra_output_cannot_replace_the_original_picture(void) {
    FrAppleDecodeOutput output = {0};
    TestImage first = picture(), extra = picture();
    assert(fr_apple_output_init(&output, 128, 128, false));
    assert(fr_apple_output_begin(&output, 1));
    callback(&output, &first, 1);
    callback(&output, &extra, 1);
    assert(output.image == &first);
    assert(atomic_load(&first.references) == 2);
    assert(atomic_load(&extra.references) == 1);
    refused(&output, FR_APPLE_OUTPUT_CODEC);
    assert(atomic_load(&first.references) == 1);
}

static void source_identity_and_timestamp_are_not_approximate(void) {
    for (unsigned n = 0; n < 8; n++) {
        FrAppleDecodeOutput output = {0}, other = {0};
        TestImage image = picture();
        assert(fr_apple_output_init(&output, 128, 128, false));
        assert(fr_apple_output_begin(&output, 2));
        void *owner = &output;
        CMTime pts = CMTimeMake(2, 1);
        switch (n) {
            case 0: owner = &other; break;
            case 1: owner = NULL; break;
            case 2: pts.value = 1; break; // Late output from the preceding input.
            case 3: pts.value = 3; break;
            case 4: pts = kCMTimeInvalid; break;
            case 5: pts.epoch = 1; break;
            case 6: pts.flags |= kCMTimeFlags_HasBeenRounded; break;
            case 7: pts.flags |= 16; break;
        }
        fr_apple_output_callback(&output, owner, noErr, 0, &image, pts, kCMTimeInvalid);
        assert(output.image == NULL && atomic_load(&image.references) == 1);
        callback(&output, &image, 2); // A later success cannot repair the failure.
        refused(&output, FR_APPLE_OUTPUT_CODEC);
        assert(atomic_load(&image.references) == 1);
    }
}

static void failure_and_dropped_or_asynchronous_results_are_not_pictures(void) {
    const VTDecodeInfoFlags bad_flags[] = {kVTDecodeInfo_Asynchronous,
                                           kVTDecodeInfo_FrameDropped, 8, UINT32_MAX};
    for (unsigned returned = 0; returned < 2; returned++) {
        for (unsigned n = 0; n < 5; n++) {
            FrAppleDecodeOutput output = {0};
            TestImage image = picture();
            assert(fr_apple_output_init(&output, 128, 128, false));
            assert(fr_apple_output_begin(&output, 1));
            OSStatus status = n == 4 ? -1 : noErr;
            VTDecodeInfoFlags flags = n == 4 ? 0 : bad_flags[n];
            fr_apple_output_callback(&output, &output, returned ? noErr : status,
                                     returned ? 0 : flags, &image, CMTimeMake(1, 1),
                                     kCMTimeInvalid);
            CVPixelBufferRef retained = NULL;
            assert(fr_apple_output_take(&output, returned ? status : noErr,
                                        returned ? flags : 0, &retained) == FR_APPLE_OUTPUT_CODEC);
            assert(retained == NULL && atomic_load(&image.references) == 1);
            fr_apple_output_destroy(&output);
        }
    }
    FrAppleDecodeOutput output = {0};
    TestImage image = picture();
    assert(fr_apple_output_init(&output, 128, 128, false));
    assert(fr_apple_output_begin(&output, 1));
    fr_apple_output_callback(&output, &output, noErr, kVTDecodeInfo_ImageBufferModifiable,
                             &image, CMTimeMake(1, 1), kCMTimeInvalid);
    CVPixelBufferRef retained = NULL;
    assert(fr_apple_output_take(&output, noErr, kVTDecodeInfo_ImageBufferModifiable, &retained) ==
           FR_APPLE_OUTPUT_OK);
    CVPixelBufferRelease(retained);
    fr_apple_output_destroy(&output);
}

static void success_without_a_picture_never_advances(void) {
    for (unsigned calls = 0; calls < 2; calls++) {
        FrAppleDecodeOutput output = {0};
        assert(fr_apple_output_init(&output, 128, 128, false));
        assert(fr_apple_output_begin(&output, 1));
        if (calls) callback(&output, NULL, 1);
        refused(&output, FR_APPLE_OUTPUT_CODEC);
    }
}

static void geometry_and_allocations_are_bounded_before_retention(void) {
    for (unsigned n = 0; n < 17; n++) {
        FrAppleDecodeOutput output = {0};
        TestImage image = picture();
        assert(fr_apple_output_init(&output, 128, 128, false));
        assert(fr_apple_output_begin(&output, 1));
        switch (n) {
            case 0: image.width++; break;
            case 1: image.height++; break;
            case 2: image.format = kCVPixelFormatType_420YpCbCr8BiPlanarFullRange; break;
            case 3: image.format = 0; break;
            case 4: image.planar = false; break;
            case 5: image.planes = 0; break;
            case 6: image.planes = 3; break;
            case 7: image.surface = false; break;
            case 8: image.plane_width[0]--; break;
            case 9: image.plane_width[1]++; break;
            case 10: image.plane_height[1]--; break;
            case 11: image.stride[0]--; break;
            case 12: image.stride[1]++; break;
            case 13: image.stride[0] = output.max_stride + 2; break;
            case 14: image.bytes = 0; break;
            case 15: image.bytes--; break;
            case 16: image.bytes = output.max_bytes + 1; break;
        }
        callback(&output, &image, 1);
        refused(&output, FR_APPLE_OUTPUT_GEOMETRY);
        assert(atomic_load(&image.references) == 1);
    }
    FrAppleDecodeOutput full = {0};
    TestImage image = picture();
    image.format = kCVPixelFormatType_420YpCbCr8BiPlanarFullRange;
    assert(fr_apple_output_init(&full, 128, 128, true));
    assert(fr_apple_output_begin(&full, 1));
    callback(&full, &image, 1);
    CVPixelBufferRef retained = NULL;
    assert(fr_apple_output_take(&full, noErr, 0, &retained) == FR_APPLE_OUTPUT_OK);
    CVPixelBufferRelease(retained);
    fr_apple_output_destroy(&full);

    // Actual independent 64-line padding of each plane fits the configured
    // budget, including when the chroma plane needs more than half the padded Y.
    FrAppleDecodeOutput padded = {0};
    TestImage wide = {
        .references = 1, .width = 3840, .height = 1080, .planes = 2,
        .format = kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
        .planar = true, .surface = true,
        .plane_width = {3840, 1920}, .plane_height = {1080, 540},
        .stride = {4096, 4096}, .bytes = 4096 * (1088 + 576),
    };
    assert(fr_apple_output_init(&padded, 3840, 1080, false));
    assert(fr_apple_output_begin(&padded, 1));
    callback(&padded, &wide, 1);
    retained = NULL;
    assert(fr_apple_output_take(&padded, noErr, 0, &retained) == FR_APPLE_OUTPUT_OK);
    CVPixelBufferRelease(retained);
    fr_apple_output_destroy(&padded);
}

static void token_and_allocation_arithmetic_never_wrap_or_alias(void) {
    for (unsigned n = 0; n < 5; n++) {
        FrAppleDecodeOutput output = {0};
        TestImage image = picture();
        assert(fr_apple_output_init(&output, 128, 128, false));
        if (n < 2) {
            assert(!fr_apple_output_begin(&output, n == 0 ? 0 : -1));
        } else {
            int64_t token = n == 4 ? INT64_MAX : 2;
            assert(fr_apple_output_begin(&output, token));
            callback(&output, &image, token);
            CVPixelBufferRef retained = NULL;
            assert(fr_apple_output_take(&output, noErr, 0, &retained) == FR_APPLE_OUTPUT_OK);
            CVPixelBufferRelease(retained);
            assert(!fr_apple_output_begin(&output, n == 3 ? 1 : token));
        }
        fr_apple_output_destroy(&output);
        assert(atomic_load(&image.references) == 1);
    }
    const uint32_t dimensions[][2] = {{0, 128}, {128, 0}, {127, 128}, {128, 127},
                                      {UINT32_MAX - 1, UINT32_MAX - 1}};
    for (size_t i = 0; i < sizeof dimensions / sizeof dimensions[0]; i++) {
        FrAppleDecodeOutput output = {0};
        assert(!fr_apple_output_init(&output, dimensions[i][0], dimensions[i][1], false));
        fr_apple_output_destroy(&output);
    }
}

static void close_fences_late_output_and_releases_only_its_own_reference(void) {
    for (unsigned had_output = 0; had_output < 2; had_output++) {
        FrAppleDecodeOutput output = {0};
        TestImage first = picture(), late = picture();
        assert(fr_apple_output_init(&output, 128, 128, false));
        assert(fr_apple_output_begin(&output, 1));
        if (had_output) callback(&output, &first, 1);
        fr_apple_output_close(&output);
        callback(&output, &late, 1);
        assert(atomic_load(&first.references) == 1 && atomic_load(&late.references) == 1);
        refused(&output, FR_APPLE_OUTPUT_CODEC);
    }
}

typedef struct {
    FrAppleDecodeOutput *output;
    TestImage *image;
} CallbackThread;
static void *native_callback_thread(void *raw) {
    CallbackThread *thread = raw;
    callback(thread->output, thread->image, 1);
    return NULL;
}

static void concurrent_callbacks_cannot_overwrite_or_leak(void) {
    for (unsigned iteration = 0; iteration < 32; iteration++) {
        FrAppleDecodeOutput output = {0};
        TestImage images[2] = {picture(), picture()};
        assert(fr_apple_output_init(&output, 128, 128, false));
        assert(fr_apple_output_begin(&output, 1));
        CallbackThread callbacks[2] = {{&output, &images[0]}, {&output, &images[1]}};
        pthread_t threads[2];
        for (size_t i = 0; i < 2; i++)
            assert(pthread_create(&threads[i], NULL, native_callback_thread, &callbacks[i]) == 0);
        for (size_t i = 0; i < 2; i++) assert(pthread_join(threads[i], NULL) == 0);
        assert(atomic_load(&images[0].references) + atomic_load(&images[1].references) == 3);
        refused(&output, FR_APPLE_OUTPUT_CODEC);
        assert(atomic_load(&images[0].references) == 1 && atomic_load(&images[1].references) == 1);
    }
}

static void callback_and_retirement_use_the_same_gate(void) {
    for (unsigned iteration = 0; iteration < 32; iteration++) {
        FrAppleDecodeOutput output = {0};
        TestImage image = picture();
        assert(fr_apple_output_init(&output, 128, 128, false));
        assert(fr_apple_output_begin(&output, 1));
        CallbackThread callback_thread = {&output, &image};
        pthread_t thread;
        assert(pthread_create(&thread, NULL, native_callback_thread, &callback_thread) == 0);
        fr_apple_output_close(&output);
        assert(pthread_join(thread, NULL) == 0);
        assert(atomic_load(&image.references) == 1);
        refused(&output, FR_APPLE_OUTPUT_CODEC);
    }
}

int main(void) {
    exactly_one_original_picture_transfers_once();
    extra_output_cannot_replace_the_original_picture();
    source_identity_and_timestamp_are_not_approximate();
    failure_and_dropped_or_asynchronous_results_are_not_pictures();
    success_without_a_picture_never_advances();
    geometry_and_allocations_are_bounded_before_retention();
    token_and_allocation_arithmetic_never_wrap_or_alias();
    close_fences_late_output_and_releases_only_its_own_reference();
    concurrent_callbacks_cannot_overwrite_or_leak();
    callback_and_retirement_use_the_same_gate();
    puts("apple_decode_output: 10 portable native-boundary tests passed");
}
