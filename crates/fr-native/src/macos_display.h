// One display's current mode and placement from Core Graphics alone (no
// capture-system round trip), shared by the capture glue and the input glue:
// the published desktop coordinates and the injected pointer must come from
// the same description. Cheap enough for every input check.
#ifndef FR_MACOS_DISPLAY_H
#define FR_MACOS_DISPLAY_H

#include <CoreGraphics/CoreGraphics.h>
#include <math.h>
#include <stdint.h>

typedef struct {
    uint32_t display_id;
    int32_t x;  // origin in the global desktop, in this display's pixels
    int32_t y;
    uint32_t pixel_width;  // the display's own pixels
    uint32_t pixel_height;
    uint32_t point_width;  // its logical size
    uint32_t point_height;
} FrDisplay;

enum { FR_DISPLAY_OK = 0, FR_DISPLAY_GONE = 1, FR_DISPLAY_UNUSABLE = 2 };

// `points` (optional) receives the display's bounds in global points.
static inline int fr_display_describe(CGDirectDisplayID id, FrDisplay *out, CGRect *points) {
    if (!CGDisplayIsActive(id)) return FR_DISPLAY_GONE;
    CGDisplayModeRef mode = CGDisplayCopyDisplayMode(id);
    if (!mode) return FR_DISPLAY_GONE;
    size_t pw = CGDisplayModeGetPixelWidth(mode), ph = CGDisplayModeGetPixelHeight(mode);
    CGDisplayModeRelease(mode);
    CGRect bounds = CGDisplayBounds(id);
    if (pw == 0 || ph == 0 || bounds.size.width < 1 || bounds.size.height < 1 || pw > UINT32_MAX ||
        ph > UINT32_MAX)
        return FR_DISPLAY_UNUSABLE;
    double scale = (double)pw / bounds.size.width;
    out->display_id = id;
    out->x = (int32_t)lround(bounds.origin.x * scale);
    out->y = (int32_t)lround(bounds.origin.y * scale);
    out->pixel_width = (uint32_t)pw;
    out->pixel_height = (uint32_t)ph;
    out->point_width = (uint32_t)lround(bounds.size.width);
    out->point_height = (uint32_t)lround(bounds.size.height);
    if (points) *points = bounds;
    return FR_DISPLAY_OK;
}

static inline int fr_display_same(const FrDisplay *a, const FrDisplay *b) {
    return a->display_id == b->display_id && a->x == b->x && a->y == b->y &&
           a->pixel_width == b->pixel_width && a->pixel_height == b->pixel_height &&
           a->point_width == b->point_width && a->point_height == b->point_height;
}

#endif
