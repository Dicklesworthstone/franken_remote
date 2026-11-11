// macOS viewer glue (fr-5a4w): one AppKit window, VideoToolbox HEVC decode
// into IOSurface-backed NV12 pixel buffers, and a Metal pass that converts
// BT.709 YCbCr and aspect-fits the picture into a CAMetalLayer. Pixels never
// cross the CPU. Rust admits every parameter set and access unit before any
// byte reaches VideoToolbox; this file only binds the OS frameworks.
//
// Threads: window creation, event polling and freeing run on the main thread.
// Decoder creation, decode, repaint and present run on the media owner.
// The renderer's native lock is never taken by the window/event thread.
// Main-thread geometry shares only a short lock over plain numeric state.
#import <AppKit/AppKit.h>
#import <CoreMedia/CoreMedia.h>
#import <CoreGraphics/CoreGraphics.h>
#import <CoreVideo/CoreVideo.h>
#import <Metal/Metal.h>
#import <QuartzCore/CAMetalLayer.h>
#import <VideoToolbox/VideoToolbox.h>
#include <os/lock.h>
#include <stdint.h>
#include <string.h>
#include <math.h>
#include <unistd.h>
#include <stdbool.h>
#include <stdatomic.h>

enum {
    FR_MAC_OK = 0,
    FR_MAC_INVALID = -1,
    FR_MAC_UNAVAILABLE = -2,
    FR_MAC_CODEC = -3,
    FR_MAC_ALLOCATION = -4,
    FR_MAC_GEOMETRY = -5,
    FR_MAC_THREAD = -6,
    FR_MAC_SESSION_LOST = -7,
};

enum {
    FR_MAC_KEY_DOWN = 1,
    FR_MAC_KEY_UP = 2,
    FR_MAC_MODIFIERS = 3,
    FR_MAC_POINTER = 4,
    FR_MAC_BUTTON_DOWN = 5,
    FR_MAC_BUTTON_UP = 6,
    FR_MAC_SCROLL = 7,
    FR_MAC_CLOSE = 8,
    FR_MAC_FOCUS_LOST = 9,
    FR_MAC_FOCUS_GAINED = 10,
};

// Must match `RawEvent` in macos.rs.
typedef struct {
    uint32_t kind;
    uint32_t code;
    uint64_t modifiers;
    double x;
    double y;
    double dx;
    double dy;
    uint32_t flags;
    uint32_t reserved;
    double timestamp;
} FrMacEvent;

#define FR_MAC_QUEUE 1024
#define FR_MAC_PUMP_EVENTS 256

static NSString *const kShader =
    @"#include <metal_stdlib>\n"
     "using namespace metal;\n"
     "struct V { float4 position [[position]]; float2 uv; };\n"
     "vertex V fr_vertex(uint id [[vertex_id]], constant float2 &extent [[buffer(0)]]) {\n"
     "  float2 p = float2((id << 1) & 2, id & 2);\n"
     "  V out; out.position = float4(p * float2(2, -2) + float2(-1, 1), 0, 1);\n"
     "  out.uv = p * extent; return out;\n"
     "}\n"
     "fragment float4 fr_fragment(V in [[stage_in]], texture2d<float> luma [[texture(0)]],\n"
     "    texture2d<float> chroma [[texture(1)]], sampler s [[sampler(0)]],\n"
     "    sampler cs [[sampler(1)]], constant float4 &range [[buffer(0)]]) {\n"
     "  float y = (luma.sample(s, in.uv).r - range.x) * range.y;\n"
     "  float2 c = (chroma.sample(cs, in.uv).rg - range.z) * range.w;\n"
     "  float3 rgb = float3(y + 1.5748 * c.y, y - 0.1873 * c.x - 0.4681 * c.y, y + 1.8556 * c.x);\n"
     "  return float4(saturate(rgb), 1.0);\n"
     "}\n"
     "vertex V fr_cursor_vertex(uint id [[vertex_id]], constant float4 &rect [[buffer(0)]]) {\n"
     "  float2 p = float2(id & 1, id >> 1);\n"
     "  V out; out.position = float4(mix(rect.xy, rect.zw, p), 0, 1); out.uv = p; return out;\n"
     "}\n"
     "fragment float4 fr_cursor_fragment(V in [[stage_in]], texture2d<float> shape [[texture(0)]],\n"
     "    sampler s [[sampler(0)]]) {\n"
     "  return shape.sample(s, in.uv);\n"
     "}\n";

// ---------------------------------------------------------------- renderer

typedef struct {
    uint32_t sourceWidth;
    uint32_t sourceHeight;
    CGSize drawable;       // pixels
    double scale;          // pixels per point
    CGRect fit;            // drawable pixels, top-left origin
    BOOL integerFit;
    uint64_t revision;     // changes only when source/window geometry changes
    uint64_t redraw;       // also changes for an expose of unchanged geometry
} FrGeometry;

// Pure geometry only. No Objective-C, driver call or retained object belongs
// under the short geometry lock shared with the window/event thread.
static void fr_refit(FrGeometry *g) {
    double sw = g->sourceWidth, sh = g->sourceHeight;
    double dw = g->drawable.width, dh = g->drawable.height;
    if (sw <= 0 || sh <= 0 || dw <= 0 || dh <= 0) {
        g->fit = CGRectZero;
        g->integerFit = NO;
        return;
    }
    double s = fmin(dw / sw, dh / sh);
    // Preserve exact whole-pixel scaling rather than smearing sharp text.
    double k = floor(s + 1e-9);
    g->integerFit = k >= 1 && fabs(sw * k - sw * s) < 1 && fabs(sh * k - sh * s) < 1;
    if (g->integerFit) s = k;
    double w = round(sw * s), h = round(sh * s);
    g->fit = CGRectMake(floor((dw - w) / 2), floor((dh - h) / 2), w, h);
}

@interface FrRenderer : NSObject {
  @public
    os_unfair_lock lock;          // media/native work only
    os_unfair_lock geometryLock;  // plain state only; never waits on native work
    FrGeometry geometry;
    uint64_t submittedRevision;
    BOOL submittedPicture;
    BOOL redrawPending;
    uint64_t presented;           // read under geometryLock, not the native lock
    CGSize appliedDrawable;      // layer properties, under the media/native lock
    double appliedScale;
    CAMetalLayer *layer;
    id<MTLDevice> device;
    id<MTLCommandQueue> queue;
    id<MTLRenderPipelineState> pipeline;
    id<MTLSamplerState> sampler;
    id<MTLSamplerState> nearest;
    CVMetalTextureCacheRef cache;
    CVPixelBufferRef last;   // retained: redrawn on resize or expose
    BOOL fullRange;
    // The single client-rendered remote cursor: straight-alpha RGBA8 shape,
    // hotspot position in picture pixels. The host capture excludes it.
    id<MTLRenderPipelineState> cursorPipeline;
    id<MTLTexture> cursorShape;
    uint32_t cursorWidth;
    uint32_t cursorHeight;
    int64_t cursorLeft;      // position minus hotspot
    int64_t cursorTop;
    uint32_t cursorHotX;
    uint32_t cursorHotY;
    BOOL cursorShown;
}
@end

@implementation FrRenderer
- (void)dealloc {
    if (last) CVPixelBufferRelease(last);
    if (cache) CFRelease(cache);
}

// Caller holds the media lock. The geometry snapshot is released before ANY
// driver work; a stalled drawable/texture/command never holds up local input.
// Success means command submission of the retained picture, not GPU completion.
- (int)drawLocked {
    os_unfair_lock_lock(&geometryLock);
    FrGeometry g = geometry;
    os_unfair_lock_unlock(&geometryLock);
    if (!last) return FR_MAC_UNAVAILABLE;
    if (g.drawable.width < 1 || g.drawable.height < 1 ||
        g.fit.size.width < 1 || g.fit.size.height < 1)
        return FR_MAC_GEOMETRY;
    if (appliedScale != g.scale) {
        layer.contentsScale = g.scale;
        appliedScale = g.scale;
    }
    if (!CGSizeEqualToSize(appliedDrawable, g.drawable)) {
        layer.drawableSize = g.drawable;
        appliedDrawable = g.drawable;
    }
    id<CAMetalDrawable> target = [layer nextDrawable];
    if (!target) return FR_MAC_UNAVAILABLE;
    id<MTLTexture> targetTexture = target.texture;
    if (!targetTexture) return FR_MAC_ALLOCATION;
    id<MTLCommandBuffer> commands = [queue commandBuffer];
    if (!commands) return FR_MAC_ALLOCATION;
    if (CVPixelBufferGetPlaneCount(last) != 2) return FR_MAC_CODEC;
    size_t w0 = CVPixelBufferGetWidthOfPlane(last, 0), h0 = CVPixelBufferGetHeightOfPlane(last, 0);
    size_t w1 = CVPixelBufferGetWidthOfPlane(last, 1), h1 = CVPixelBufferGetHeightOfPlane(last, 1);
    if (!w0 || !h0 || !w1 || !h1 || w0 < g.sourceWidth || h0 < g.sourceHeight)
        return FR_MAC_GEOMETRY;
    CVMetalTextureRef luma = NULL, chroma = NULL;
    if (CVMetalTextureCacheCreateTextureFromImage(NULL, cache, last, NULL, MTLPixelFormatR8Unorm,
                                                  w0, h0, 0, &luma) != kCVReturnSuccess ||
        !luma || !CVMetalTextureGetTexture(luma) ||
        CVMetalTextureCacheCreateTextureFromImage(NULL, cache, last, NULL, MTLPixelFormatRG8Unorm,
                                                  w1, h1, 1, &chroma) != kCVReturnSuccess ||
        !chroma || !CVMetalTextureGetTexture(chroma)) {
        if (luma) CFRelease(luma);
        if (chroma) CFRelease(chroma);
        return FR_MAC_CODEC;
    }
    MTLRenderPassDescriptor *pass = [MTLRenderPassDescriptor renderPassDescriptor];
    if (!pass) {
        CFRelease(luma);
        CFRelease(chroma);
        return FR_MAC_ALLOCATION;
    }
    pass.colorAttachments[0].texture = targetTexture;
    pass.colorAttachments[0].loadAction = MTLLoadActionClear;
    pass.colorAttachments[0].clearColor = MTLClearColorMake(0, 0, 0, 1);
    pass.colorAttachments[0].storeAction = MTLStoreActionStore;
    id<MTLRenderCommandEncoder> encoder = [commands renderCommandEncoderWithDescriptor:pass];
    if (!encoder) {
        CFRelease(luma);
        CFRelease(chroma);
        return FR_MAC_ALLOCATION;
    }
    // Sample only the visible picture if the decoder kept coded padding.
    float extent[2] = {(float)fmin(1.0, (double)g.sourceWidth / (double)w0),
                       (float)fmin(1.0, (double)g.sourceHeight / (double)h0)};
    float range[4] = {fullRange ? 0.0f : 16.0f / 255.0f, fullRange ? 1.0f : 255.0f / 219.0f,
                      128.0f / 255.0f, fullRange ? 1.0f : 255.0f / 224.0f};
    MTLViewport viewport = {g.fit.origin.x, g.fit.origin.y, g.fit.size.width, g.fit.size.height, 0, 1};
    [encoder setViewport:viewport];
    [encoder setRenderPipelineState:pipeline];
    [encoder setVertexBytes:extent length:sizeof(extent) atIndex:0];
    [encoder setFragmentTexture:CVMetalTextureGetTexture(luma) atIndex:0];
    [encoder setFragmentTexture:CVMetalTextureGetTexture(chroma) atIndex:1];
    // Chroma remains linear; whole-pixel luma keeps its edges.
    [encoder setFragmentSamplerState:(g.integerFit ? nearest : sampler) atIndex:0];
    [encoder setFragmentSamplerState:sampler atIndex:1];
    [encoder setFragmentBytes:range length:sizeof(range) atIndex:0];
    [encoder drawPrimitives:MTLPrimitiveTypeTriangle vertexStart:0 vertexCount:3];
    [self drawCursorLocked:encoder geometry:&g];
    [encoder endEncoding];
    // The Metal textures keep the IOSurface alive until the GPU finishes.
    CVMetalTextureRef keepLuma = luma, keepChroma = chroma;
    [commands addCompletedHandler:^(id<MTLCommandBuffer> done) {
      (void)done;
      if (keepLuma) CFRelease(keepLuma);
      if (keepChroma) CFRelease(keepChroma);
    }];
    [commands presentDrawable:target];
    [commands commit];
    MTLCommandBufferStatus state = commands.status;
    if (state != MTLCommandBufferStatusCommitted && state != MTLCommandBufferStatusScheduled &&
        state != MTLCommandBufferStatusCompleted)
        return FR_MAC_CODEC;
    os_unfair_lock_lock(&geometryLock);
    submittedPicture = YES;
    submittedRevision = g.revision;
    // A resize or expose may have arrived while the driver was busy. It still
    // needs its own draw; an older submission cannot clear that newer request.
    if (geometry.redraw == g.redraw) redrawPending = NO;
    os_unfair_lock_unlock(&geometryLock);
    return FR_MAC_OK;
}

// Caller holds the lock: the cursor quad over the fitted picture, scaled with
// it and clipped to it, blended with straight alpha.
- (void)drawCursorLocked:(id<MTLRenderCommandEncoder>)encoder geometry:(const FrGeometry *)g {
    if (!cursorShown || !cursorShape || g->sourceWidth == 0 || g->sourceHeight == 0) return;
    double dw = g->drawable.width, dh = g->drawable.height;
    double kx = g->fit.size.width / g->sourceWidth, ky = g->fit.size.height / g->sourceHeight;
    double left = g->fit.origin.x + (double)cursorLeft * kx;
    double top = g->fit.origin.y + (double)cursorTop * ky;
    double right = left + cursorWidth * kx, bottom = top + cursorHeight * ky;
    float rect[4] = {(float)(left / dw * 2 - 1), (float)(1 - top / dh * 2), (float)(right / dw * 2 - 1),
                     (float)(1 - bottom / dh * 2)};
    MTLViewport whole = {0, 0, dw, dh, 0, 1};
    MTLScissorRect clip = {(NSUInteger)g->fit.origin.x, (NSUInteger)g->fit.origin.y,
                           (NSUInteger)g->fit.size.width, (NSUInteger)g->fit.size.height};
    [encoder setViewport:whole];
    [encoder setScissorRect:clip];
    [encoder setRenderPipelineState:cursorPipeline];
    [encoder setVertexBytes:rect length:sizeof(rect) atIndex:0];
    [encoder setFragmentTexture:cursorShape atIndex:0];
    [encoder setFragmentSamplerState:sampler atIndex:0];
    [encoder drawPrimitives:MTLPrimitiveTypeTriangleStrip vertexStart:0 vertexCount:4];
}

- (int)present:(CVPixelBufferRef)picture {
    os_unfair_lock_lock(&lock);
    if (last) CVPixelBufferRelease(last);
    last = CVPixelBufferRetain(picture);
    int status = [self drawLocked];
    if (status == FR_MAC_OK) {
        os_unfair_lock_lock(&geometryLock);
        presented++;
        os_unfair_lock_unlock(&geometryLock);
    }
    os_unfair_lock_unlock(&lock);
    return status;
}

// Main thread: publish geometry or an expose request, without rendering or
// touching a native lock held by decode/nextDrawable/Metal. Expose alone keeps
// existing geometry evidence valid until a real geometry change occurs.
- (void)resizeTo:(CGSize)points scale:(double)pixelsPerPoint {
    double scale = pixelsPerPoint > 0 ? pixelsPerPoint : 1;
    CGSize drawable = CGSizeMake(fmax(1, round(points.width * scale)),
                                  fmax(1, round(points.height * scale)));
    os_unfair_lock_lock(&geometryLock);
    if (geometry.scale != scale || !CGSizeEqualToSize(geometry.drawable, drawable)) {
        geometry.scale = scale;
        geometry.drawable = drawable;
        geometry.revision++;
        fr_refit(&geometry);
    }
    geometry.redraw++;
    redrawPending = YES;
    os_unfair_lock_unlock(&geometryLock);
}

// Main thread: normalize a view point (top-left origin) against the last fit.
- (void)normalize:(NSPoint)point x:(double *)x y:(double *)y inside:(BOOL *)inside {
    os_unfair_lock_lock(&geometryLock);
    CGRect f = geometry.fit;
    double s = geometry.scale;
    os_unfair_lock_unlock(&geometryLock);
    if (f.size.width < 1 || f.size.height < 1) {
        *x = *y = -1;
        *inside = NO;
        return;
    }
    *x = (point.x * s - f.origin.x) / f.size.width;
    *y = (point.y * s - f.origin.y) / f.size.height;
    *inside = *x >= 0 && *x < 1 && *y >= 0 && *y < 1;
}
@end

static FrRenderer *renderer_new(uint32_t width, uint32_t height, int *status) {
    FrRenderer *r = [FrRenderer new];
    r->lock = OS_UNFAIR_LOCK_INIT;
    r->geometryLock = OS_UNFAIR_LOCK_INIT;
    r->geometry.sourceWidth = width;
    r->geometry.sourceHeight = height;
    r->geometry.scale = 1;
    r->geometry.revision = 1;
    r->geometry.redraw = 1;
    r->redrawPending = YES;
    r->device = MTLCreateSystemDefaultDevice();
    if (!r->device) {
        *status = FR_MAC_UNAVAILABLE;
        return nil;
    }
    r->queue = [r->device newCommandQueue];
    NSError *error = nil;
    id<MTLLibrary> library = [r->device newLibraryWithSource:kShader options:nil error:&error];
    if (!library || !r->queue) {
        *status = FR_MAC_UNAVAILABLE;
        return nil;
    }
    MTLRenderPipelineDescriptor *pd = [MTLRenderPipelineDescriptor new];
    pd.vertexFunction = [library newFunctionWithName:@"fr_vertex"];
    pd.fragmentFunction = [library newFunctionWithName:@"fr_fragment"];
    pd.colorAttachments[0].pixelFormat = MTLPixelFormatBGRA8Unorm;
    r->pipeline = [r->device newRenderPipelineStateWithDescriptor:pd error:&error];
    MTLRenderPipelineDescriptor *cd = [MTLRenderPipelineDescriptor new];
    cd.vertexFunction = [library newFunctionWithName:@"fr_cursor_vertex"];
    cd.fragmentFunction = [library newFunctionWithName:@"fr_cursor_fragment"];
    MTLRenderPipelineColorAttachmentDescriptor *blend = cd.colorAttachments[0];
    blend.pixelFormat = MTLPixelFormatBGRA8Unorm;
    blend.blendingEnabled = YES;
    blend.rgbBlendOperation = MTLBlendOperationAdd;
    blend.alphaBlendOperation = MTLBlendOperationAdd;
    blend.sourceRGBBlendFactor = MTLBlendFactorSourceAlpha;
    blend.destinationRGBBlendFactor = MTLBlendFactorOneMinusSourceAlpha;
    blend.sourceAlphaBlendFactor = MTLBlendFactorOne;
    blend.destinationAlphaBlendFactor = MTLBlendFactorOneMinusSourceAlpha;
    r->cursorPipeline = [r->device newRenderPipelineStateWithDescriptor:cd error:&error];
    MTLSamplerDescriptor *sd = [MTLSamplerDescriptor new];
    sd.minFilter = MTLSamplerMinMagFilterLinear;
    sd.magFilter = MTLSamplerMinMagFilterLinear;
    sd.sAddressMode = MTLSamplerAddressModeClampToEdge;
    sd.tAddressMode = MTLSamplerAddressModeClampToEdge;
    r->sampler = [r->device newSamplerStateWithDescriptor:sd];
    sd.minFilter = MTLSamplerMinMagFilterNearest;
    sd.magFilter = MTLSamplerMinMagFilterNearest;
    r->nearest = [r->device newSamplerStateWithDescriptor:sd];
    if (!r->pipeline || !r->cursorPipeline || !r->sampler || !r->nearest ||
        CVMetalTextureCacheCreate(NULL, NULL, r->device, NULL, &r->cache) != kCVReturnSuccess) {
        *status = FR_MAC_UNAVAILABLE;
        return nil;
    }
    CAMetalLayer *layer = [CAMetalLayer layer];
    layer.device = r->device;
    layer.pixelFormat = MTLPixelFormatBGRA8Unorm;
    layer.framebufferOnly = YES;
    layer.opaque = YES;
    CGColorSpaceRef srgb = CGColorSpaceCreateWithName(kCGColorSpaceSRGB);
    layer.colorspace = srgb;
    CGColorSpaceRelease(srgb);
    CGColorRef black = CGColorCreateGenericRGB(0, 0, 0, 1);
    layer.backgroundColor = black;
    CGColorRelease(black);
    r->layer = layer;
    *status = FR_MAC_OK;
    return r;
}

// ------------------------------------------------------------------ window

// Local display choice and explicit file operations have reserved chrome.
// None of these controls participates in the remote input viewport.
enum {
    FR_MAC_SHARE_UPLOAD = 1,
    FR_MAC_SHARE_DOWNLOAD = 2,
    FR_MAC_CHANGE_DISPLAY = 4,
    FR_MAC_CLIPBOARD = 8,
    FR_MAC_TEXT_COMPOSER = 16,
    FR_MAC_FILE_SELECTION = 32,
    FR_MAC_SHARE_ALL = 63,
};
#define FR_MAC_SHARING_HEIGHT 44.0
#define FR_MAC_TEXT_HEIGHT 76.0
#define FR_MAC_TEXT_MIN_WIDTH 480.0
#define FR_MAC_TEXT_BYTES 4096
#define FR_MAC_TEXT_AGE 0.100
#define FR_MAC_REMOTE_INPUT_TAG INT64_C(0x46524350)
#define FR_MAC_SHARING_CELL 230.0
#define FR_MAC_CHANGE_WIDTH 136.0
#define FR_MAC_CLIPBOARD_WIDTH 184.0
#define FR_MAC_FILE_SELECTION_WIDTH 144.0
#define FR_MAC_FILE_SELECTION_BYTES (32 * 1024)
#define FR_MAC_FILE_SELECTION_PATHS 8
#define FR_MAC_FILE_SELECTION_AGE 2.0

// Main-thread scalar receipt; no renderer, surface or input token crosses it.
typedef struct {
    uint64_t revision;
    double width, height, x, y, observed;
    int32_t eligible;
    int32_t full_screen;
} FrMacResizeSample;
typedef void (*FrResizeChanged)(void *, const FrMacResizeSample *);
typedef void (*FrResizeRelease)(void *);

typedef void (*FrClipboardStop)(void *, int);
typedef void (*FrClipboardRelease)(void *);
typedef int (*FrFileSelectionReady)(void *);
typedef int (*FrFileSelectionSubmit)(void *, const uint8_t *, size_t, double);
typedef void (*FrFileSelectionStop)(void *);
typedef void (*FrFileSelectionRelease)(void *);

static BOOL fr_sharing_status(uint32_t status) {
    return status <= 5;
}

static NSString *fr_byte_count(uint64_t bytes) {
    static NSString *const units[] = {@"B", @"KiB", @"MiB", @"GiB", @"TiB", @"PiB", @"EiB"};
    double value = (double)bytes;
    size_t unit = 0;
    while (value >= 1024 && unit < 6) {
        value /= 1024;
        unit++;
    }
    return unit == 0 ? [NSString stringWithFormat:@"%llu B", (unsigned long long)bytes]
                    : [NSString stringWithFormat:@"%.1f %@", value, units[unit]];
}

static NSString *fr_transfer_label(NSString *direction, uint32_t status,
                                    uint64_t done, uint64_t total) {
    switch (status) {
        case 0: return [direction stringByAppendingString:@": Off"];
        case 1: return [direction stringByAppendingString:@": Waiting"];
        case 2:
            return [NSString stringWithFormat:@"%@: %@ / %@", direction,
                                               fr_byte_count(done), fr_byte_count(total)];
        case 3: return [direction stringByAppendingString:@": Finished"];
        case 4: return [direction stringByAppendingString:@": Cancelled"];
        default: return [direction stringByAppendingString:@": Failed"];
    }
}

static unsigned fr_sharing_cells(uint32_t flags) {
    return ((flags & FR_MAC_SHARE_UPLOAD) != 0) + ((flags & FR_MAC_SHARE_DOWNLOAD) != 0);
}

static double fr_sharing_width(uint32_t flags) {
    double width = 16 + FR_MAC_SHARING_CELL * fr_sharing_cells(flags) +
                   ((flags & FR_MAC_CHANGE_DISPLAY) ? FR_MAC_CHANGE_WIDTH : 0) +
                   ((flags & FR_MAC_CLIPBOARD) ? FR_MAC_CLIPBOARD_WIDTH : 0) +
                   ((flags & FR_MAC_FILE_SELECTION) ? FR_MAC_FILE_SELECTION_WIDTH : 0);
    return (flags & FR_MAC_TEXT_COMPOSER) ? fmax(width, FR_MAC_TEXT_MIN_WIDTH) : width;
}

static double fr_sharing_height(uint32_t flags) {
    return flags ? FR_MAC_SHARING_HEIGHT +
                       ((flags & FR_MAC_TEXT_COMPOSER) ? FR_MAC_TEXT_HEIGHT : 0)
                 : 0;
}

static NSUInteger fr_held_modifiers(NSUInteger flags) {
    // Function/numeric-pad flags classify some key events; they do not prove
    // a modifier remains held after an arrow/function key release.
    return flags & (NSEventModifierFlagShift | NSEventModifierFlagControl |
                    NSEventModifierFlagOption | NSEventModifierFlagCommand);
}

static BOOL fr_any_key(const BOOL *keys) {
    for (unsigned i = 0; i < 256; i++) if (keys[i]) return YES;
    return NO;
}


// Notification callbacks retain only this plain negative flag. They never
// retain an AppKit owner whose final release could occur off the main thread.
@interface FrWindowLifetime : NSObject {
  @public
    _Atomic(bool) unavailable;
}
@end
@implementation FrWindowLifetime
@end

@class FrWindowOwner;

@interface FrContentView : NSView
@property(nonatomic, weak) FrWindowOwner *owner;
@end

@interface FrSharingView : NSView
@end

// NSButton normally enters a tracking loop until mouse-up. These local stop
// controls act on mouse-down and return immediately, so holding the button
// cannot postpone Rust's cancellation/authority turn.
@interface FrSharingButton : NSButton
@end

// AppKit owns real text editing, input methods and candidate placement here.
// This view is local chrome, not a guessed remote document or caret.
@interface FrTextComposer : NSTextView {
  @public
    // Written once, before attachment. A retired editor never joins a new draft.
    uint64_t openingContext;
    NSUInteger selectionAnchor;
    BOOL selecting;
}
@property(nonatomic, weak) FrWindowOwner *owner;
- (BOOL)acceptsTextCallbacks;
@end


@interface FrMetalView : NSView
@property(nonatomic, weak) FrWindowOwner *owner;
@end

@interface FrWindowOwner : NSObject <NSWindowDelegate> {
  @public
    NSWindow *window;
    FrMetalView *view;
    FrContentView *content;
    FrSharingView *sharing;
    NSTextField *uploadLabel, *downloadLabel;
    NSTextField *clipboardLabel;
    FrSharingButton *uploadCancel, *downloadCancel, *changeDisplay;
    FrSharingButton *clipboardOff;
    FrSharingButton *sendCopiedFiles;
    void *fileSelectionContext;
    FrFileSelectionReady fileSelectionReady;
    FrFileSelectionSubmit fileSelectionSubmit;
    FrFileSelectionStop fileSelectionStop;
    FrFileSelectionRelease fileSelectionRelease;
    BOOL fileSelectionStopped;
    NSInteger fileDragSequence;
    double fileDragCaptured;
    void *clipboardContext;
    FrClipboardStop clipboardStop;
    FrClipboardRelease clipboardRelease;
    BOOL clipboardStopped, clipboardUserOff;
    uint32_t sharingFlags, sharingActions, uploadStatus, downloadStatus;
    uint64_t uploadDone, uploadTotal, downloadDone, downloadTotal;
    BOOL uploadCancelled, downloadCancelled, layoutInProgress;
    BOOL displayChangeUsed;
    double displayChangeAt;
    FrTextComposer *textEditor, *retiredTextEditor;
    NSScrollView *textScroll;
    NSTextField *textLabel;
    NSString *textNotice;
    FrSharingButton *textOpen, *textSend, *textCancel;
    uint64_t textContext, textPendingContext;
    uint64_t textUntil, textPendingUntil; // opaque original Rust clock bounds
    BOOL nativeStopPending, textCleaning, textOpening, textParked, clipboardUIStopped;
    double textContextSince, textOpenedAt, textPendingAt;
    BOOL textEligible, textActive, textPending, textTaken, textQuarantine;
    BOOL physicalKeys[256], localTextKeys[256];
    NSUInteger physicalModifiers, localTextModifiers;
    uint8_t textBytes[FR_MAC_TEXT_BYTES];
    size_t textLength;
    FrRenderer *renderer;
    FrMacEvent queue[FR_MAC_QUEUE];
    size_t head, count;
    BOOL overflow;
    FrWindowLifetime *lifetime;
    BOOL sessionUnavailableReported;
    NSNotificationCenter *lifetimeCenter;
    NSMutableArray<id> *lifetimeObservers;
    uint32_t buttons;  // pressed mouse buttons that went down inside the picture
    void *resizeContext;
    FrResizeChanged resizeChanged;
    FrResizeRelease resizeRelease;
    uint64_t resizeRevision;
    uint32_t resizeSourceWidth, resizeSourceHeight;
    CGSize resizeDrawable;
    double resizeScale;
    BOOL resizeSealed;
    // AppKit is animating into or out of full screen. `fullScreenFenced`: this
    // transition stopped the original attempt and its settled size is not yet
    // reported. Each fence or settle is one more sample revision
    // (`fullScreenBumps`), even when no drawable size changed.
    BOOL fullScreenMoving, fullScreenFenced;
    uint64_t fullScreenBumps;
}
- (void)push:(FrMacEvent)event;
- (void)layout;
- (void)buildSharing;
- (void)layoutSharing;
- (void)cancelUpload:(id)sender;
- (void)cancelDownload:(id)sender;
- (void)changeDisplay:(id)sender;
- (void)stopClipboard:(BOOL)userOff;
- (void)stopClipboardWithoutUI:(BOOL)userOff;
- (void)finishStoppedUI;
- (void)disableClipboard:(id)sender;
- (BOOL)fileSelectionAvailable;
- (void)stopFileSelections;
- (void)updateFileSelections;
- (void)sendCopiedFiles:(id)sender;
- (BOOL)submitFilePasteboard:(NSPasteboard *)pasteboard captured:(double)captured;
- (BOOL)textWindowReady;
- (BOOL)textPhysicalClear;
- (BOOL)textGesture:(NSEvent *)event;
- (void)updateTextControls;
- (void)checkSessionUnavailable;
- (FrTextComposer *)newTextEditor;
- (void)retireText:(NSString *)message;
- (void)cleanupRetiredText;
- (void)stopNativeInput:(NSString *)message;
- (void)cancelResize;
- (void)clearText:(NSString *)message;
- (void)openText:(id)sender;
- (void)sendText:(id)sender;
- (void)cancelText:(id)sender;
- (BOOL)allowsTextReplacement:(NSRange)range string:(id)string;
@end

@implementation FrTextComposer
- (BOOL)acceptsTextCallbacks {
    FrWindowOwner *owner = self.owner;
    // Scalar identity checks precede every superclass/IME call. Cancel/reopen
    // within the same Rust context still uses a different native editor.
    return owner && owner->textEditor == self && owner->textActive &&
           !owner->textPending && !owner->nativeStopPending && !owner->textCleaning &&
           openingContext && owner->textContext == openingContext &&
           !atomic_load_explicit(&owner->lifetime->unavailable, memory_order_acquire);
}
- (BOOL)shouldChangeTextInRange:(NSRange)range replacementString:(NSString *)string {
    if (![self acceptsTextCallbacks]) return NO;
    if (![super shouldChangeTextInRange:range replacementString:string] ||
        ![self acceptsTextCallbacks]) return NO;
    return [self.owner allowsTextReplacement:range string:string];
}
- (BOOL)shouldChangeTextInRanges:(NSArray<NSValue *> *)ranges
             replacementStrings:(NSArray<NSString *> *)strings {
    if (![self acceptsTextCallbacks]) return NO;
    // AppKit requires this superclass check first; it checks editing/delegate
    // permission without changing storage. Payload bounds still precede edits.
    if (![super shouldChangeTextInRanges:ranges replacementStrings:strings] ||
        ![self acceptsTextCallbacks]) return NO;
    // This bounded composer has one local selection. A multiple-selection
    // edit cannot multiply an individually admitted replacement.
    if (ranges.count != 1 || (strings && strings.count != 1)) return NO;
    return [self.owner allowsTextReplacement:ranges[0].rangeValue
                                     string:strings ? strings[0] : nil];
}
- (void)keyDown:(NSEvent *)event {
    if (![self acceptsTextCallbacks]) return;
    FrWindowOwner *owner = self.owner;
    NSUInteger required = NSEventModifierFlagShift | NSEventModifierFlagControl |
                          NSEventModifierFlagOption;
    // Also catch a key dispatched by AppKit's own editor processing, rather
    // than solely relying on the outer event pump's first check.
    if (owner && event.keyCode == 53 && (event.modifierFlags & required) == required) {
        FrMacEvent e = {0};
        e.kind = FR_MAC_KEY_DOWN;
        e.code = event.keyCode;
        e.modifiers = event.modifierFlags;
        e.timestamp = event.timestamp;
        [owner stopNativeInput:@"Local text discarded without sending."];
        [owner push:e];
        return;
    }
    if (owner && owner->textActive && !owner->textPending) [super keyDown:event];
}
- (void)insertText:(id)string replacementRange:(NSRange)range {
    if (![self acceptsTextCallbacks]) return;
    if ([self.owner allowsTextReplacement:range string:string] && [self acceptsTextCallbacks])
        [super insertText:string replacementRange:range];
}
- (void)setMarkedText:(id)string selectedRange:(NSRange)selection replacementRange:(NSRange)range {
    if (![self acceptsTextCallbacks]) return;
    NSString *value = [string isKindOfClass:NSAttributedString.class]
                         ? [(NSAttributedString *)string string]
                         : ([string isKindOfClass:NSString.class] ? string : nil);
    if (!value || selection.location > value.length ||
        selection.length > value.length - selection.location) return;
    if ([self.owner allowsTextReplacement:range string:string] && [self acceptsTextCallbacks])
        [super setMarkedText:string selectedRange:selection replacementRange:range];
}
- (void)didChangeText {
    if (![self acceptsTextCallbacks]) return;
    [super didChangeText];
    if (![self acceptsTextCallbacks]) return;
    FrWindowOwner *owner = self.owner;
    if (owner) {
        owner->textNotice = nil;
        [owner updateTextControls];
    }
}
- (void)setString:(NSString *)string {
    // NSTextView's direct setter (including accessibility edits) must not
    // bypass the same whole-document bound. Empty teardown is allowed only
    // after this editor has been detached from its owner.
    if (!string || string.length > FR_MAC_TEXT_BYTES) return;
    if (!self.owner && !string.length) {
        [super setString:string];
        return;
    }
    if (![self acceptsTextCallbacks]) return;
    if ([self.owner allowsTextReplacement:NSMakeRange(0, self.string.length) string:string] &&
        [self acceptsTextCallbacks])
        [super setString:string];
}
// The text editor accepts text/input-method events only. It does not acquire
// files, attributed attachments, a clipboard fallback, or an undo journal.
- (void)refuseLocalPaste {
    if (![self acceptsTextCallbacks]) return;
    FrWindowOwner *owner = self.owner;
    if (owner) {
        owner->textNotice = @"Paste is unavailable in this text composer.";
        [owner updateTextControls];
    }
}
- (void)paste:(id)sender { (void)sender; [self refuseLocalPaste]; }
- (void)pasteAsPlainText:(id)sender { (void)sender; [self refuseLocalPaste]; }
- (void)pasteAsRichText:(id)sender { (void)sender; [self refuseLocalPaste]; }
- (BOOL)readSelectionFromPasteboard:(NSPasteboard *)pasteboard {
    (void)pasteboard;
    [self refuseLocalPaste];
    return NO;
}
- (BOOL)readSelectionFromPasteboard:(NSPasteboard *)pasteboard type:(NSPasteboardType)type {
    (void)pasteboard;
    (void)type;
    [self refuseLocalPaste];
    return NO;
}
- (BOOL)performDragOperation:(id<NSDraggingInfo>)sender {
    (void)sender;
    return NO;
}
- (NSMenu *)menuForEvent:(NSEvent *)event {
    (void)event;
    return nil;
}
// NSTextView's default selection enters a mouse tracking loop. Update one
// selection per pump event so holding a drag cannot postpone Rust renewal.
- (void)mouseDown:(NSEvent *)event {
    if (![self acceptsTextCallbacks]) return;
    [self.window makeFirstResponder:self];
    if (![self acceptsTextCallbacks] || [self.inputContext handleEvent:event] ||
        ![self acceptsTextCallbacks] || self.hasMarkedText) return;
    NSPoint point = [self convertPoint:event.locationInWindow fromView:nil];
    NSUInteger index = [self characterIndexForInsertionAtPoint:point];
    if (![self acceptsTextCallbacks] || index > self.string.length) return;
    selectionAnchor = index;
    selecting = YES;
    [self setSelectedRange:NSMakeRange(index, 0)];
}
- (void)mouseDragged:(NSEvent *)event {
    if (!selecting || ![self acceptsTextCallbacks] || self.hasMarkedText) return;
    NSUInteger index = [self characterIndexForInsertionAtPoint:
        [self convertPoint:event.locationInWindow fromView:nil]];
    if (![self acceptsTextCallbacks] || index > self.string.length ||
        selectionAnchor > self.string.length) return;
    NSUInteger start = MIN(selectionAnchor, index), end = MAX(selectionAnchor, index);
    [self setSelectedRange:NSMakeRange(start, end - start)];
    if ([self acceptsTextCallbacks]) [self scrollRangeToVisible:NSMakeRange(index, 0)];
}
- (void)mouseUp:(NSEvent *)event {
    (void)event;
    selecting = NO;
}
- (void)rightMouseDown:(NSEvent *)event { (void)event; }
- (void)otherMouseDown:(NSEvent *)event { (void)event; }
@end

@implementation FrMetalView
- (NSDragOperation)draggingEntered:(id<NSDraggingInfo>)sender {
    // AppKit only requests metadata import after the actual release. Entering
    // or hovering neither reads a URL nor opens a source.
    return [self.owner fileSelectionAvailable] &&
           (sender.draggingSourceOperationMask & NSDragOperationCopy)
        ? NSDragOperationCopy : NSDragOperationNone;
}
- (NSDragOperation)draggingUpdated:(id<NSDraggingInfo>)sender {
    return [self draggingEntered:sender];
}
- (BOOL)prepareForDragOperation:(id<NSDraggingInfo>)sender {
    FrWindowOwner *owner = self.owner;
    if (!owner || [self draggingEntered:sender] != NSDragOperationCopy) return NO;
    owner->fileDragSequence = sender.draggingSequenceNumber;
    owner->fileDragCaptured = NSProcessInfo.processInfo.systemUptime;
    return YES;
}
- (BOOL)performDragOperation:(id<NSDraggingInfo>)sender {
    FrWindowOwner *owner = self.owner;
    if (!owner || owner->fileDragCaptured <= 0 ||
        owner->fileDragSequence != sender.draggingSequenceNumber) return NO;
    double captured = owner->fileDragCaptured;
    owner->fileDragCaptured = 0; // consume this release once, before native reads
    // Apple's NSDraggingDestination contract imports from the sender's actual
    // pasteboard here, only after prepareForDragOperation returned YES:
    // developer.apple.com/documentation/appkit/nsdraggingdestination
    return [owner submitFilePasteboard:sender.draggingPasteboard captured:captured];
}
- (void)draggingExited:(id<NSDraggingInfo>)sender {
    (void)sender;
    FrWindowOwner *owner = self.owner;
    if (owner) owner->fileDragCaptured = 0;
}
- (void)draggingEnded:(id<NSDraggingInfo>)sender {
    (void)sender;
    FrWindowOwner *owner = self.owner;
    if (owner) owner->fileDragCaptured = 0;
}
- (BOOL)wantsUpdateLayer {
    return YES;
}
- (CALayer *)makeBackingLayer {
    return self.owner->renderer->layer;
}
- (BOOL)acceptsFirstResponder {
    return YES;
}
- (BOOL)acceptsFirstMouse:(NSEvent *)event {
    (void)event;
    return YES;
}
- (BOOL)isFlipped {
    return YES;
}
- (void)setFrameSize:(NSSize)size {
    [super setFrameSize:size];
    [self.owner layout];
}
- (void)viewDidChangeBackingProperties {
    [super viewDidChangeBackingProperties];
    [self.owner layout];
}
- (void)updateLayer {
    [self.owner layout];
}
// Key events are translated in the pump and never dispatched here, so no
// key equivalent, beep or text-input machinery acts on remote keystrokes.
- (void)keyDown:(NSEvent *)event {
    (void)event;
}
- (void)keyUp:(NSEvent *)event {
    (void)event;
}
@end


@implementation FrContentView
- (BOOL)isFlipped { return YES; }
- (void)setFrameSize:(NSSize)size {
    [super setFrameSize:size];
    [self.owner layout];
}
@end

@implementation FrSharingView
- (BOOL)isFlipped { return YES; }
- (BOOL)isOpaque { return YES; }
- (void)drawRect:(NSRect)dirty {
    [NSColor.windowBackgroundColor setFill];
    NSRectFill(dirty);
    [NSColor.separatorColor setFill];
    NSRectFill(NSMakeRect(0, NSHeight(self.bounds) - 1, NSWidth(self.bounds), 1));
}
@end

@implementation FrSharingButton
- (BOOL)acceptsFirstResponder { return NO; }
- (void)mouseDown:(NSEvent *)event {
    NSPoint p = [self convertPoint:event.locationInWindow fromView:nil];
    if (self.enabled && NSPointInRect(p, self.bounds))
        [NSApp sendAction:self.action to:self.target from:self];
}
- (void)mouseDragged:(NSEvent *)event { (void)event; }
- (void)mouseUp:(NSEvent *)event { (void)event; }
@end

static NSTextField *fr_sharing_label(FrSharingView *parent, NSString *text) {
    NSTextField *label = [NSTextField labelWithString:text];
    label.font = [NSFont systemFontOfSize:11];
    label.lineBreakMode = NSLineBreakByTruncatingTail;
    label.selectable = NO;
    [parent addSubview:label];
    return label;
}

static FrSharingButton *fr_sharing_button(FrSharingView *parent, NSString *title,
                                          FrWindowOwner *owner, SEL action) {
    FrSharingButton *button = [[FrSharingButton alloc] initWithFrame:NSZeroRect];
    button.title = title;
    button.bezelStyle = NSBezelStyleRounded;
    button.controlSize = NSControlSizeSmall;
    button.target = owner;
    button.action = action;
    button.font = [NSFont systemFontOfSize:11];
    [parent addSubview:button];
    return button;
}

static BOOL fr_file_selection_bytes(NSPasteboard *pasteboard, uint8_t *bytes, size_t *length) {
    NSInteger revision = pasteboard.changeCount;
    NSArray<NSPasteboardItem *> *items = pasteboard.pasteboardItems;
    if (!items.count || items.count > FR_MAC_FILE_SELECTION_PATHS) return NO;
    // Promise types are a different, potentially materializing workflow.
    // Apple documents this complete list for both modern/legacy promises:
    // developer.apple.com/documentation/appkit/nsfilepromisereceiver/readabledraggedtypes
    NSArray<NSString *> *promises = NSFilePromiseReceiver.readableDraggedTypes;
    *length = 0;
    for (NSPasteboardItem *item in items) {
        for (NSString *promise in promises)
            if ([item.types containsObject:promise]) return NO;
        if (![item.types containsObject:NSPasteboardTypeFileURL]) return NO;
        NSString *uri = [item stringForType:NSPasteboardTypeFileURL];
        if (!uri || uri.length > FR_MAC_FILE_SELECTION_BYTES) return NO;
        // One pasteboard item denotes one URL. Do not let an embedded line
        // separator manufacture extra selections in the shared URI-list parser.
        if ([uri rangeOfCharacterFromSet:NSCharacterSet.newlineCharacterSet].location != NSNotFound)
            return NO;
        NSUInteger count = [uri lengthOfBytesUsingEncoding:NSUTF8StringEncoding];
        // Bound the entire payload before any application buffer copy.
        if (!count || count >= FR_MAC_FILE_SELECTION_BYTES - *length) return NO;
        NSUInteger used = 0;
        if (![uri getBytes:bytes + *length maxLength:count usedLength:&used
                   encoding:NSUTF8StringEncoding options:0
                      range:NSMakeRange(0, uri.length) remainingRange:NULL] || used != count)
            return NO;
        *length += count;
        bytes[(*length)++] = '\n';
    }
    return pasteboard.changeCount == revision;
}

// Reads only the original window and the renderer's short geometry lock.
// A missing/currently hidden window cannot grant a replacement attempt.
static FrMacResizeSample fr_resize_sample(FrWindowOwner *owner) {
    FrMacResizeSample sample = {0};
    os_unfair_lock_lock(&owner->renderer->geometryLock);
    sample.revision = owner->renderer->geometry.revision + owner->fullScreenBumps;
    BOOL sameSource = !owner->resizeContext ||
        (owner->renderer->geometry.sourceWidth == owner->resizeSourceWidth &&
         owner->renderer->geometry.sourceHeight == owner->resizeSourceHeight);
    os_unfair_lock_unlock(&owner->renderer->geometryLock);
    NSWindow *window = owner->window;
    NSSize size = window.contentView.bounds.size;
    NSPoint origin = window.frame.origin;
    sample.width = size.width;
    sample.height = size.height;
    sample.x = origin.x;
    sample.y = origin.y;
    sample.observed = NSProcessInfo.processInfo.systemUptime;
    sample.full_screen = (window.styleMask & NSWindowStyleMaskFullScreen) ? 1 : 0;
    sample.eligible = sameSource && !owner->fullScreenMoving && !owner->nativeStopPending &&
        !owner->overflow &&
        !owner->textActive && !owner->textPending && !owner->displayChangeAt &&
        // Native polling pauses at an undelivered sharing action. That is not
        // a complete final event turn: a queued close/focus/escape may follow.
        !owner->sharingActions &&
        !atomic_load_explicit(&owner->lifetime->unavailable, memory_order_acquire) &&
        window.isKeyWindow && window.isVisible && !window.isMiniaturized &&
        window.isOnActiveSpace && (window.occlusionState & NSWindowOcclusionStateVisible);
    return sample;
}

@implementation FrWindowOwner

- (void)dealloc {
    for (id observer in lifetimeObservers) [lifetimeCenter removeObserver:observer];
}
- (void)checkSessionUnavailable {
    if (atomic_load_explicit(&lifetime->unavailable, memory_order_acquire) &&
        !sessionUnavailableReported) {
        sessionUnavailableReported = YES;
        // Reuse the original native source's terminal focus-loss path. A wake
        // notification never clears this flag or restores this window's input.
        [self loseFocus];
    }
}

- (void)buildSharing {
    if (sharingFlags & FR_MAC_CHANGE_DISPLAY) {
        changeDisplay = fr_sharing_button(sharing, @"Change Display", self,
                                         @selector(changeDisplay:));
        changeDisplay.accessibilityLabel = @"Choose another remote display";
        changeDisplay.enabled = NO; // Rust arms only an eligible original mode.
    }
    if (sharingFlags & FR_MAC_CLIPBOARD) {
        clipboardLabel = fr_sharing_label(sharing, @"Clipboard: Waiting");
        clipboardOff = fr_sharing_button(sharing, @"Turn Off", self, @selector(disableClipboard:));
        clipboardOff.accessibilityLabel = @"Turn off shared text clipboard";
    }
    if (sharingFlags & FR_MAC_FILE_SELECTION) {
        sendCopiedFiles = fr_sharing_button(sharing, @"Send copied files", self,
                                           @selector(sendCopiedFiles:));
        sendCopiedFiles.accessibilityLabel = @"Send the local files explicitly copied in Finder";
        sendCopiedFiles.toolTip = @"Copy files in Finder before taking control, then click here. Up to 8 local files or directories.";
        sendCopiedFiles.enabled = NO;
    }
    if (sharingFlags & FR_MAC_SHARE_UPLOAD) {
        uploadStatus = 1;
        uploadLabel = fr_sharing_label(sharing, @"Send: Waiting");
        uploadCancel = fr_sharing_button(sharing, @"Cancel", self, @selector(cancelUpload:));
        uploadCancel.accessibilityLabel = @"Cancel sending file";
    }
    if (sharingFlags & FR_MAC_SHARE_DOWNLOAD) {
        downloadStatus = 1;
        downloadLabel = fr_sharing_label(sharing, @"Receive: Waiting");
        downloadCancel = fr_sharing_button(sharing, @"Cancel", self, @selector(cancelDownload:));
        downloadCancel.accessibilityLabel = @"Cancel receiving file";
    }
    if (sharingFlags & FR_MAC_TEXT_COMPOSER) {
        textLabel = fr_sharing_label(sharing, @"Text input unavailable");
        textOpen = fr_sharing_button(sharing, @"Edit Text", self, @selector(openText:));
        textSend = fr_sharing_button(sharing, @"Send", self, @selector(sendText:));
        textCancel = fr_sharing_button(sharing, @"Cancel", self, @selector(cancelText:));
        textOpen.accessibilityLabel = @"Compose text locally with your Mac input method";
        textSend.accessibilityLabel = @"Send this complete text to the original remote session";
        textCancel.accessibilityLabel = @"Discard local text without sending";
        textScroll = [[NSScrollView alloc] initWithFrame:NSZeroRect];
        textScroll.borderType = NSBezelBorder;
        // Wheel/caret scrolling remains; a native scrollbar would run its
        // own mouse tracking loop outside the bounded owner pump.
        textScroll.hasVerticalScroller = NO;
        textScroll.hasHorizontalScroller = NO;
        [sharing addSubview:textScroll];
        [self updateTextControls];
    }
}
- (void)layoutSharing {
    if (!sharing || !sharingFlags) return;
    unsigned cells = fr_sharing_cells(sharingFlags);
    double offset = changeDisplay ? FR_MAC_CHANGE_WIDTH : 0;
    if (changeDisplay) changeDisplay.frame = NSMakeRect(8, 8, FR_MAC_CHANGE_WIDTH - 8, 28);
    if (clipboardOff) {
        clipboardLabel.frame = NSMakeRect(8 + offset, 14, 108, 17);
        clipboardOff.frame = NSMakeRect(8 + offset + 112, 8, 68, 28);
        offset += FR_MAC_CLIPBOARD_WIDTH;
    }
    if (sendCopiedFiles) {
        sendCopiedFiles.frame = NSMakeRect(8 + offset, 8, FR_MAC_FILE_SELECTION_WIDTH - 8, 28);
        offset += FR_MAC_FILE_SELECTION_WIDTH;
    }
    double cell = cells ? (NSWidth(sharing.bounds) - 16 - offset) / cells : 0;
    unsigned index = 0;
    NSTextField *labels[] = {uploadLabel, downloadLabel};
    FrSharingButton *controls[] = {uploadCancel, downloadCancel};
    for (unsigned i = 0; i < 2; i++) {
        if (!labels[i]) continue;
        double x = 8 + offset + index++ * cell;
        labels[i].frame = NSMakeRect(x + 4, 14, fmax(1, cell - 78), 17);
        controls[i].frame = NSMakeRect(x + cell - 70, 8, 66, 28);
    }
    if (textScroll) {
        double width = NSWidth(sharing.bounds);
        textLabel.frame = NSMakeRect(8, 46, fmax(1, width - 16), 16);
        textScroll.frame = NSMakeRect(8, 64, fmax(1, width - 104), 48);
        textOpen.frame = NSMakeRect(width - 92, 76, 84, 28);
        textSend.frame = NSMakeRect(width - 92, 62, 84, 24);
        textCancel.frame = NSMakeRect(width - 92, 88, 84, 24);
    }
}

- (FrTextComposer *)newTextEditor {
    FrTextComposer *editor = [[FrTextComposer alloc] initWithFrame:NSMakeRect(0, 0, 100, 48)];
    if (!editor) return nil;
    // Keep construction in the same bounded retirement slot before any
    // configuration can synchronously report a terminal window callback.
    retiredTextEditor = editor;
    if (nativeStopPending) return editor;
    editor.richText = NO;
    editor.importsGraphics = NO;
    editor.allowsUndo = NO;
    editor.smartInsertDeleteEnabled = NO;
    editor.usesFindPanel = NO;
    editor.automaticTextReplacementEnabled = NO;
    editor.automaticSpellingCorrectionEnabled = NO;
    editor.automaticQuoteSubstitutionEnabled = NO;
    editor.automaticDashSubstitutionEnabled = NO;
    editor.automaticLinkDetectionEnabled = NO;
    editor.automaticDataDetectionEnabled = NO;
    editor.continuousSpellCheckingEnabled = NO;
    editor.grammarCheckingEnabled = NO;
    if (nativeStopPending) return editor;
    editor.font = [NSFont systemFontOfSize:13];
    editor.textContainerInset = NSMakeSize(4, 4);
    editor.minSize = NSMakeSize(0, 48);
    editor.maxSize = NSMakeSize(CGFLOAT_MAX, FR_MAC_TEXT_BYTES * 32.0);
    editor.verticallyResizable = YES;
    editor.horizontallyResizable = NO;
    editor.autoresizingMask = NSViewWidthSizable;
    editor.textContainer.size = NSMakeSize(100, FR_MAC_TEXT_BYTES * 32.0);
    editor.textContainer.widthTracksTextView = YES;
    if (nativeStopPending) return editor;
    [editor unregisterDraggedTypes];
    editor.accessibilityLabel = @"Local text, sent only when you click Send";
    editor.automaticTextCompletionEnabled = NO;
    editor.enabledTextCheckingTypes = 0;
    return editor;
}

- (BOOL)textWindowReady {
    if (nativeStopPending || textCleaning || textParked) return NO;
    if (atomic_load_explicit(&lifetime->unavailable, memory_order_acquire)) return NO;
    if (!window.isKeyWindow || !window.isVisible || window.isMiniaturized ||
        !window.isOnActiveSpace || !(window.occlusionState & NSWindowOcclusionStateVisible))
        return NO;
    os_unfair_lock_lock(&renderer->geometryLock);
    BOOL ready = renderer->submittedPicture &&
                 renderer->submittedRevision == renderer->geometry.revision;
    os_unfair_lock_unlock(&renderer->geometryLock);
    return ready;
}
- (BOOL)textPhysicalClear {
    if (buttons || physicalModifiers || localTextModifiers ||
        fr_any_key(physicalKeys) || fr_any_key(localTextKeys) ||
        fr_held_modifiers(NSEvent.modifierFlags) || (NSEvent.pressedMouseButtons & ~1UL))
        return NO;
    // Include keys held before this window/grant. The initiating primary
    // mouse press is local chrome; every other current button must be up.
    // Caps Lock is a toggle, not a physically-held modifier.
    for (CGKeyCode key = 0; key < 128; key++)
        if (key != 0x39 && CGEventSourceKeyState(kCGEventSourceStateCombinedSessionState, key))
            return NO;
    return YES;
}
- (BOOL)textGesture:(NSEvent *)event {
    double now = NSProcessInfo.processInfo.systemUptime;
    if (!event || event.type != NSEventTypeLeftMouseDown || event.window != window ||
        !isfinite(event.timestamp) || event.timestamp <= 0 ||
        event.timestamp < textContextSince || fr_held_modifiers(event.modifierFlags) ||
        now < event.timestamp ||
        now - event.timestamp >= FR_MAC_TEXT_AGE)
        return NO;
    CGEventRef native = event.CGEvent;
    // A selector invocation, self-created NSEvent or this product's remotely
    // injected event cannot manufacture fresh local Send intent.
    return native &&
           CGEventGetIntegerValueField(native, kCGEventSourceUserData) != FR_MAC_REMOTE_INPUT_TAG &&
           CGEventGetIntegerValueField(native, kCGEventSourceUnixProcessID) != getpid();
}
- (void)updateTextControls {
    if (!textScroll || nativeStopPending || textCleaning || textParked) return;
    BOOL available = textContext && textUntil && textEligible && [self textWindowReady];
    BOOL clear = [self textPhysicalClear];
    if (nativeStopPending) return;
    textOpen.hidden = textActive;
    textOpen.enabled = available && clear && !textPending;
    textSend.hidden = !textActive;
    textCancel.hidden = !textActive;
    textScroll.hidden = !textActive;
    if (nativeStopPending) return;
    textEditor.editable = textActive && !textPending && textContext;
    textEditor.selectable = textEditor.editable;
    if (nativeStopPending) return;
    textSend.enabled = available && clear && textActive && !textPending &&
                       textEditor.string.length > 0 && !textEditor.hasMarkedText;
    textCancel.enabled = textActive;
    if (nativeStopPending) return;
    if (textNotice) {
        textLabel.stringValue = textNotice;
    } else if (textPending) {
        textLabel.stringValue = @"Text awaiting the original session";
    } else if (textActive) {
        textLabel.stringValue = textEditor.hasMarkedText
            ? @"Finish composition, then click Send. Text remains local until Send."
            : (available ? @"Text remains local until Send. Limit: 4096 UTF-8 bytes."
                         : @"Sending paused. Your local text has not been sent.");
    } else {
        textLabel.stringValue = textContext
            ? @"Use your Mac input method. Physical keyboard control stays unchanged."
            : @"Text input unavailable";
    }
}
// This retirement is deliberately scalar-only. In particular, it must not
// ask an input method to discard text before Rust receives a terminal event.
- (void)retireText:(NSString *)message {
    textActive = NO;
    textPending = NO;
    textTaken = NO;
    textPendingContext = 0;
    textPendingUntil = 0;
    textPendingAt = 0;
    textOpenedAt = 0;
    textLength = 0;
    memset(textBytes, 0, sizeof(textBytes));
    // New editors are created only after this one retained slot is cleaned.
    // Moving the reference before clearing identity prevents ARC from running
    // NSTextView teardown on the native terminal callback.
    if (textEditor && !retiredTextEditor) {
        retiredTextEditor = textEditor;
        textEditor = nil;
    }
    textNotice = message;
    textQuarantine = fr_any_key(localTextKeys) || localTextModifiers != 0;
}
- (void)cleanupRetiredText {
    if (nativeStopPending || textCleaning || textOpening || textParked || !retiredTextEditor)
        return;
    textCleaning = YES;
    FrTextComposer *retired = retiredTextEditor;
    retiredTextEditor = nil;
    retired.owner = nil;
    retired.editable = NO;
    if (!nativeStopPending) [retired.inputContext discardMarkedText];
    if (!nativeStopPending) retired.string = @"";
    if (!nativeStopPending && window.firstResponder == retired) [window makeFirstResponder:view];
    if (!nativeStopPending && textScroll.documentView == retired) textScroll.documentView = nil;
    if (!nativeStopPending) [retired removeFromSuperview];
    if (nativeStopPending) retiredTextEditor = retired;
    textCleaning = NO;
}
- (void)cancelResize {
    if (resizeContext && resizeChanged) {
        FrMacResizeSample sample = {0}; // negative only, even after a prior seal
        resizeChanged(resizeContext, &sample);
    }
}
- (void)stopNativeInput:(NSString *)message {
    [self cancelResize];
    nativeStopPending = YES;
    [self stopFileSelections];
    textContext = 0;
    textUntil = 0;
    textEligible = NO;
    [self retireText:message];
    [self stopClipboardWithoutUI:NO];
}
- (void)clearText:(NSString *)message {
    [self retireText:message];
    if (nativeStopPending || textParked) return;
    [self cleanupRetiredText];
    if (!nativeStopPending) [self updateTextControls];
}
- (void)openText:(id)sender {
    NSEvent *event = NSApp.currentEvent;
    if (nativeStopPending || textCleaning || textOpening || textParked ||
        sender != textOpen || !textOpen.enabled ||
        textActive || textPending || !textContext || !textUntil || !textEligible ||
        ![self textWindowReady] || ![self textPhysicalClear] || ![self textGesture:event])
        return;
    uint64_t context = textContext;
    double captured = event.timestamp;
    [self cleanupRetiredText];
    if (nativeStopPending || retiredTextEditor || textContext != context) return;
    textOpening = YES;
    FrTextComposer *editor = [self newTextEditor];
    textOpening = NO;
    if (!editor || nativeStopPending || textContext != context || !textUntil) return;
    editor->openingContext = context;
    editor.owner = self;
    textEditor = editor;
    retiredTextEditor = nil; // current identity owns it before this release
    textActive = YES;
    textQuarantine = YES;
    textOpenedAt = captured;
    textNotice = nil;
    textScroll.documentView = editor;
    [editor setSelectedRange:NSMakeRange(0, 0)];
    [self updateTextControls];
    if (!nativeStopPending && textEditor == editor && ![window makeFirstResponder:editor])
        [self clearText:@"Text input unavailable"];
}
- (void)sendText:(id)sender {
    NSEvent *event = NSApp.currentEvent;
    if (nativeStopPending || textCleaning || textParked || sender != textSend || !textActive ||
        textPending || !textContext || !textUntil || !textEditor ||
        textEditor->openingContext != textContext || !textEligible ||
        ![self textWindowReady] || ![self textPhysicalClear] ||
        ![self textGesture:event] || event.timestamp <= textOpenedAt)
        return;
    FrTextComposer *editor = textEditor;
    uint64_t context = textContext, until = textUntil;
    if (editor.hasMarkedText) {
        textNotice = @"Finish composition before sending text.";
        [self updateTextControls];
        return;
    }
    NSString *value = editor.string;
    if (!value.length || value.length > FR_MAC_TEXT_BYTES) return;
    NSData *encoded = [value dataUsingEncoding:NSUTF8StringEncoding allowLossyConversion:NO];
    if (!encoded || !encoded.length || encoded.length > FR_MAC_TEXT_BYTES) {
        textNotice = @"Text exceeds the 4096-byte limit. Nothing was sent.";
        [self updateTextControls];
        return;
    }
    // Native queries may synchronously re-enter a lifecycle callback. Pin
    // this exact editor/context/deadline and recheck before exposing bytes.
    if (nativeStopPending || textEditor != editor || ![editor acceptsTextCallbacks] ||
        textContext != context || textUntil != until || !textEligible ||
        atomic_load_explicit(&lifetime->unavailable, memory_order_acquire))
        return;
    memcpy(textBytes, encoded.bytes, encoded.length);
    textLength = encoded.length;
    textPendingContext = context;
    textPendingUntil = until;
    textPendingAt = event.timestamp;
    textPending = YES;
    textTaken = NO;
    textNotice = nil;
    [self updateTextControls];
}
- (void)cancelText:(id)sender {
    if (sender == textCancel) [self clearText:@"Local text discarded without sending."];
}
- (BOOL)allowsTextReplacement:(NSRange)range string:(id)replacement {
    if (!textEditor || ![textEditor acceptsTextCallbacks] || ![self textWindowReady]) return NO;
    if (!replacement) return YES; // attributes only; rich text/attachments are disabled
    NSString *value = [replacement isKindOfClass:NSAttributedString.class]
                         ? [(NSAttributedString *)replacement string]
                         : ([replacement isKindOfClass:NSString.class] ? replacement : nil);
    NSString *current = textEditor.string;
    if (!value) return NO;
    if (value.length > FR_MAC_TEXT_BYTES || current.length > FR_MAC_TEXT_BYTES) {
        textNotice = @"Text exceeds the 4096-byte limit. Nothing was sent.";
        [self updateTextControls];
        return NO;
    }
    if (range.location == NSNotFound)
        range = textEditor.hasMarkedText ? textEditor.markedRange : textEditor.selectedRange;
    if (range.location > current.length || range.length > current.length - range.location)
        return NO;
    if (value.length > FR_MAC_TEXT_BYTES - (current.length - range.length)) {
        textNotice = @"Text exceeds the 4096-byte limit. Nothing was sent.";
        [self updateTextControls];
        return NO;
    }
    // Both UTF-16 lengths are bounded before constructing the candidate. A
    // whole conversion is admitted or refused; no scalar/preedit truncation.
    NSString *candidate = [current stringByReplacingCharactersInRange:range withString:value];
    NSData *encoded = [candidate dataUsingEncoding:NSUTF8StringEncoding allowLossyConversion:NO];
    if (!encoded || encoded.length > FR_MAC_TEXT_BYTES) {
        textNotice = @"Text exceeds the 4096-byte limit. Nothing was sent.";
        [self updateTextControls];
        return NO;
    }
    return YES;
}
- (void)cancelUpload:(id)sender {
    (void)sender;
    if (!uploadCancel.enabled || uploadCancelled) return;
    [self stopFileSelections];
    uploadCancelled = YES;
    sharingActions |= FR_MAC_SHARE_UPLOAD;
    uploadCancel.enabled = NO;
    uploadLabel.stringValue = @"Send: Cancelling";
}
- (void)cancelDownload:(id)sender {
    (void)sender;
    if (!downloadCancel.enabled || downloadCancelled) return;
    downloadCancelled = YES;
    sharingActions |= FR_MAC_SHARE_DOWNLOAD;
    downloadCancel.enabled = NO;
    downloadLabel.stringValue = @"Receive: Cancelling";
}
- (void)changeDisplay:(id)sender {
    (void)sender;
    NSEvent *event = NSApp.currentEvent;
    double now = NSProcessInfo.processInfo.systemUptime;
    if (!changeDisplay.enabled || displayChangeUsed || !window.isKeyWindow ||
        !event || event.type != NSEventTypeLeftMouseDown || event.window != window ||
        !isfinite(event.timestamp) || event.timestamp <= 0 || now < event.timestamp ||
        now - event.timestamp >= 0.250) return;
    displayChangeUsed = YES;
    [self stopFileSelections];
    // This is a nonterminal handoff: Rust must first inspect the rest of the
    // batch for a queued veto, then park the original producer. IME/chrome
    // cleanup waits for its explicit text_stopped acknowledgment.
    textContext = 0;
    textUntil = 0;
    textEligible = NO;
    [self retireText:@"Text cancelled: the selected display is changing."];
    [self stopClipboardWithoutUI:NO];
    displayChangeAt = event.timestamp;
}

- (void)stopClipboardWithoutUI:(BOOL)userOff {
    if (!(sharingFlags & FR_MAC_CLIPBOARD)) return;
    clipboardStopped = YES;
    clipboardUserOff |= userOff;
    // This original-owner callback is synchronous and does no AppKit work.
    if (clipboardContext && clipboardStop) clipboardStop(clipboardContext, userOff ? 1 : 0);
    clipboardUIStopped = YES;
}
- (void)finishStoppedUI {
    if (nativeStopPending) return;
    if (clipboardUIStopped) {
        clipboardUIStopped = NO;
        clipboardOff.enabled = NO;
        clipboardLabel.stringValue = @"Clipboard: Off";
    }
    if (!nativeStopPending && displayChangeUsed) {
        changeDisplay.enabled = NO;
        changeDisplay.title = @"Changing Display";
    }
}
- (void)stopClipboard:(BOOL)userOff {
    [self stopClipboardWithoutUI:userOff];
    [self finishStoppedUI];
}
- (void)disableClipboard:(id)sender {
    (void)sender;
    if (!clipboardOff.enabled) return;
    [self stopClipboard:YES];
}

- (BOOL)fileSelectionAvailable {
    if (!(sharingFlags & FR_MAC_FILE_SELECTION) || fileSelectionStopped ||
        uploadCancelled || !fileSelectionContext || !fileSelectionReady ||
        !fileSelectionSubmit || ![self textWindowReady] || !sharing || sharing.isHidden)
        return NO;
    return fileSelectionReady(fileSelectionContext) != 0;
}
- (void)stopFileSelections {
    fileSelectionStopped = YES;
    fileDragCaptured = 0;
    // This callback fences metadata and the exact latest transfer without
    // entering AppKit or borrowing the main-thread session.
    if (fileSelectionContext && fileSelectionStop) fileSelectionStop(fileSelectionContext);
}
- (void)updateFileSelections {
    if (!sendCopiedFiles || nativeStopPending) return;
    sendCopiedFiles.enabled = [self fileSelectionAvailable];
    // Cancelling remains available while an explicit selector is alive, even
    // after a completed/refused batch, and during local descriptor preparation.
    if (!uploadCancelled && !fileSelectionStopped) uploadCancel.enabled = YES;
}
- (void)sendCopiedFiles:(id)sender {
    NSEvent *event = NSApp.currentEvent;
    if (sender != sendCopiedFiles || !sendCopiedFiles.enabled ||
        ![self fileSelectionAvailable] || ![self textPhysicalClear] ||
        ![self textGesture:event]) return;
    // No clipboard URL is inspected on hover, a timer, or remote input. Keep
    // the click's original age across all pasteboard and worker operations.
    [self submitFilePasteboard:NSPasteboard.generalPasteboard captured:event.timestamp];
}
- (BOOL)submitFilePasteboard:(NSPasteboard *)pasteboard captured:(double)captured {
    if (![self fileSelectionAvailable] || !isfinite(captured) || captured <= 0) return NO;
    BOOL accepted = NO;
    @autoreleasepool {
        uint8_t bytes[FR_MAC_FILE_SELECTION_BYTES];
        size_t length = 0;
        BOOL selected = fr_file_selection_bytes(pasteboard, bytes, &length);
        double now = NSProcessInfo.processInfo.systemUptime;
        // Rust validates local file-URL syntax and reserves the single bounded
        // slot. No NSURL resolution, metadata lookup or source read occurs here.
        if (selected && [self fileSelectionAvailable] && now >= captured &&
            now - captured < FR_MAC_FILE_SELECTION_AGE)
            accepted = fileSelectionSubmit(fileSelectionContext, bytes, length, now - captured) != 0;
        if (!nativeStopPending && uploadLabel) {
            uploadLabel.stringValue = accepted ? @"Send: Preparing" : @"Send: Selection refused";
        }
        [self updateFileSelections];
    }
    return accepted;
}

- (void)push:(FrMacEvent)event {
    if (count == FR_MAC_QUEUE) {
        overflow = YES;
        [self stopNativeInput:@"Text cancelled: native input overflow."];
        return;
    }
    queue[(head + count) % FR_MAC_QUEUE] = event;
    count++;
}
- (void)layout {
    if (!view || layoutInProgress) return;
    layoutInProgress = YES;
    if (content) {
        NSRect bounds = content.bounds;
        double chrome = fr_sharing_height(sharingFlags);
        sharing.frame = NSMakeRect(0, 0, NSWidth(bounds), chrome);
        view.frame = NSMakeRect(0, chrome, NSWidth(bounds),
                                fmax(1, NSHeight(bounds) - chrome));
        [self layoutSharing];
    }
    double scale = window ? window.backingScaleFactor : 1;
    [renderer resizeTo:view.bounds.size scale:scale];
    // No geometry is reported while AppKit animates full screen: the start
    // already fenced the original, and only the settled size is a receipt.
    if (resizeContext && resizeChanged && !fullScreenMoving && !fullScreenFenced) {
        FrMacResizeSample sample = fr_resize_sample(self);
        if (sample.revision != resizeRevision) {
            os_unfair_lock_lock(&renderer->geometryLock);
            BOOL localGeometry = renderer->geometry.scale != resizeScale ||
                !CGSizeEqualToSize(renderer->geometry.drawable, resizeDrawable);
            resizeScale = renderer->geometry.scale;
            resizeDrawable = renderer->geometry.drawable;
            os_unfair_lock_unlock(&renderer->geometryLock);
            resizeRevision = sample.revision;
            // Remote picture reconfiguration is not a local window gesture.
            if (resizeSealed || !localGeometry) sample.eligible = 0;
            // The callback records this receipt and synchronously fences the
            // original viewer before another input or IME/native cleanup call.
            resizeChanged(resizeContext, &sample);
        }
    }
    layoutInProgress = NO;
    if ((textActive || textPending) && ![self textWindowReady]) {
        // A true native geometry/window loss invalidates this active draft.
        // Do not perform IME cleanup or another drawing pass before Rust
        // fences the original input. No-draft viewing resizes are unchanged.
        overflow = YES;
        [self stopNativeInput:@"Text cancelled: the visible picture changed."];
    }
}
- (BOOL)windowShouldClose:(NSWindow *)sender {
    (void)sender;
    [self stopNativeInput:@"Local text discarded without sending."];
    FrMacEvent e = {0};
    e.kind = FR_MAC_CLOSE;
    [self push:e];
    return NO;  // Rust ends the session, then frees the window.
}
- (void)windowDidResignKey:(NSNotification *)note {
    (void)note;
    // A fenced full-screen move already stopped the original attempt; key and
    // Space changes while AppKit animates it are not a user's focus change.
    // The settled sample still requires the key, visible, active-Space window.
    if (fullScreenFenced) return;
    [self loseFocus];
}
// The terminal focus-loss path: AppKit's resign-key notification and an
// unavailable session both end held input here.
- (void)loseFocus {
    if (nativeStopPending) return;
    [self stopNativeInput:@"Text cancelled: the window lost focus."];
    buttons = 0;  // the session releases everything it holds remotely
    memset(physicalKeys, 0, sizeof(physicalKeys));
    memset(localTextKeys, 0, sizeof(localTextKeys));
    physicalModifiers = 0;
    localTextModifiers = 0;
    textQuarantine = NO;
    FrMacEvent e = {0};
    e.kind = FR_MAC_FOCUS_LOST;
    [self push:e];
}
- (void)windowDidBecomeKey:(NSNotification *)note {
    (void)note;
    FrMacEvent e = {0};
    e.kind = FR_MAC_FOCUS_GAINED;
    [self push:e];
}
- (void)windowDidChangeBackingProperties:(NSNotification *)note {
    (void)note;
    [self layout];
}
- (void)windowWillMiniaturize:(NSNotification *)note {
    [self windowDidResignKey:note];
}
- (void)windowDidChangeOcclusionState:(NSNotification *)note {
    (void)note;
    if (fullScreenFenced) {
        [self fullScreenSettle];
        return;
    }
    if (!window.isVisible || window.isMiniaturized || !window.isOnActiveSpace ||
        !(window.occlusionState & NSWindowOcclusionStateVisible)) {
        [self windowDidResignKey:note];
    }
}
// Full screen moves the window to its own Space and resizes it over an
// animation. A bound watcher gets one sample at the start (the old size at a
// new revision: Rust stops the original before any input can map through the
// moving geometry) and one when it has settled and is shown; the seal waits in
// between (fr_mac_window_full_screen).
- (void)fullScreenWillMove {
    if (fullScreenMoving) return;
    FrMacResizeSample sample = fr_resize_sample(self);
    fullScreenMoving = YES;
    if (!resizeContext || !resizeChanged || resizeSealed || nativeStopPending) return;
    fullScreenBumps++;
    sample.revision++;
    resizeRevision = sample.revision;
    fullScreenFenced = YES;
    resizeChanged(resizeContext, &sample);
}
- (void)fullScreenDidMove {
    if (!fullScreenMoving) return;
    fullScreenMoving = NO;
    [self layout];
    [self fullScreenSettle];
}
// AppKit's did-enter/exit arrives before the window is shown on its new
// Space; its occlusion notification follows. Report the settled size then.
// A transition that failed back to the old size is still a new receipt.
- (void)fullScreenSettle {
    if (!fullScreenFenced || fullScreenMoving ||
        !(window.occlusionState & NSWindowOcclusionStateVisible))
        return;
    fullScreenFenced = NO;
    if (!resizeContext || !resizeChanged || resizeSealed) return;
    fullScreenBumps++;
    FrMacResizeSample sample = fr_resize_sample(self);
    os_unfair_lock_lock(&renderer->geometryLock);
    resizeScale = renderer->geometry.scale;
    resizeDrawable = renderer->geometry.drawable;
    os_unfair_lock_unlock(&renderer->geometryLock);
    resizeRevision = sample.revision;
    resizeChanged(resizeContext, &sample);
}
- (void)windowWillEnterFullScreen:(NSNotification *)note {
    (void)note;
    [self fullScreenWillMove];
}
- (void)windowWillExitFullScreen:(NSNotification *)note {
    (void)note;
    [self fullScreenWillMove];
}
- (void)windowDidEnterFullScreen:(NSNotification *)note {
    (void)note;
    [self fullScreenDidMove];
}
- (void)windowDidExitFullScreen:(NSNotification *)note {
    (void)note;
    [self fullScreenDidMove];
}
- (void)windowDidFailToEnterFullScreen:(NSWindow *)failed {
    (void)failed;
    [self fullScreenDidMove];
}
- (void)windowDidFailToExitFullScreen:(NSWindow *)failed {
    (void)failed;
    [self fullScreenDidMove];
}

// Returns YES when AppKit should still receive the event.
- (BOOL)translate:(NSEvent *)event {
    if (nativeStopPending) return NO;
    if (event.window != window) return YES;
    if (atomic_load_explicit(&lifetime->unavailable, memory_order_acquire)) {
        [self checkSessionUnavailable];
        return NO; // return the negative before any AppKit cleanup can block
    }
    FrMacEvent e = {0};
    e.modifiers = event.modifierFlags;
    e.timestamp = event.timestamp;
    BOOL keyboard = event.type == NSEventTypeKeyDown || event.type == NSEventTypeKeyUp ||
                    event.type == NSEventTypeFlagsChanged;
    if (keyboard) {
        if (!window.isKeyWindow) return NO;
        // Local emergency revoke precedes text mode, readiness, and all native
        // key suppression. Never ask the text system to interpret this chord.
        NSUInteger escapeFlags = NSEventModifierFlagShift | NSEventModifierFlagControl |
                                 NSEventModifierFlagOption;
        if (event.type == NSEventTypeKeyDown && event.keyCode == 53 &&
            (event.modifierFlags & escapeFlags) == escapeFlags) {
            e.kind = FR_MAC_KEY_DOWN;
            e.code = event.keyCode;
            e.flags = event.isARepeat ? 1 : 0;
            [self stopNativeInput:@"Local text discarded without sending."];
            [self push:e];
            return NO;
        }
        if (event.keyCode >= 256) {
            overflow = YES;
            [self stopNativeInput:@"Text cancelled: invalid native key."];
            return NO;
        }
        if (textActive || textPending || textQuarantine) {
            localTextModifiers = fr_held_modifiers(event.modifierFlags);
            if (event.type != NSEventTypeFlagsChanged)
                localTextKeys[event.keyCode] = event.type == NSEventTypeKeyDown;
            BOOL dispatch = textActive && !textPending && textContext &&
                            isfinite(event.timestamp) && event.timestamp >= textOpenedAt;
            if (!textActive && !textPending && !fr_any_key(localTextKeys) && !localTextModifiers)
                textQuarantine = NO;
            [self updateTextControls];
            // AppKit receives the original event only while the local editor
            // owns it. A consumed key-up never reaches the remote HID stream.
            return dispatch;
        }
        physicalModifiers = fr_held_modifiers(event.modifierFlags);
        if (event.type == NSEventTypeFlagsChanged) {
            e.kind = FR_MAC_MODIFIERS;
        } else {
            physicalKeys[event.keyCode] = event.type == NSEventTypeKeyDown;
            e.kind = event.type == NSEventTypeKeyDown ? FR_MAC_KEY_DOWN : FR_MAC_KEY_UP;
            e.flags = event.type == NSEventTypeKeyDown && event.isARepeat ? 1 : 0;
        }
        e.code = event.keyCode;
        [self push:e];
        [self updateTextControls];
        return NO;
    }
    NSPoint p = [view convertPoint:event.locationInWindow fromView:nil];
    BOOL inside = NO;
    BOOL inView = NSPointInRect(p, view.bounds);
    [renderer normalize:p x:&e.x y:&e.y inside:&inside];
    e.flags = inside ? 1 : 0;
    // Editing is an explicit local mode. Remote pointer/button/scroll actions
    // cannot move the target document while a draft or Send is pending.
    if ((textActive || textPending || textQuarantine) && inView) return NO;
    switch (event.type) {
        case NSEventTypeMouseMoved:
            if (!inView) return YES;
            e.kind = FR_MAC_POINTER;
            [self push:e];
            return YES;
        case NSEventTypeLeftMouseDragged:
        case NSEventTypeRightMouseDragged:
        case NSEventTypeOtherMouseDragged:
            if (!buttons) return YES;
            e.kind = FR_MAC_POINTER;
            [self push:e];
            return YES;
        case NSEventTypeLeftMouseDown:
        case NSEventTypeRightMouseDown:
        case NSEventTypeOtherMouseDown: {
            if (!inside || event.buttonNumber > 31) return YES;
            uint32_t bit = 1u << event.buttonNumber;
            buttons |= bit;
            e.kind = FR_MAC_BUTTON_DOWN;
            e.code = (uint32_t)event.buttonNumber;
            [self push:e];
            return YES;
        }
        case NSEventTypeLeftMouseUp:
        case NSEventTypeRightMouseUp:
        case NSEventTypeOtherMouseUp: {
            if (event.buttonNumber > 31) return YES;
            uint32_t bit = 1u << event.buttonNumber;
            if (!(buttons & bit)) return YES;
            buttons &= ~bit;
            e.kind = FR_MAC_BUTTON_UP;
            e.code = (uint32_t)event.buttonNumber;
            [self push:e];
            return YES;
        }
        case NSEventTypeScrollWheel:
            if (!inside) return YES;
            e.kind = FR_MAC_SCROLL;
            e.dx = event.scrollingDeltaX;
            e.dy = event.scrollingDeltaY;
            e.flags |= event.hasPreciseScrollingDeltas ? 2 : 0;
            [self push:e];
            return NO;
        default:
            return YES;
    }
}
@end

int fr_mac_app_init(void) {
    if (![NSThread isMainThread]) return FR_MAC_THREAD;
    @autoreleasepool {
        [NSApplication sharedApplication];
        [NSApp setActivationPolicy:NSApplicationActivationPolicyRegular];
        [NSApp finishLaunching];
    }
    return FR_MAC_OK;
}

// The main screen's area a new viewer window can fill without being shrunk:
// whole points of the visible frame below the 28-point title bar and
// `reserved` points of native controls, in the screen's pixels, plus its
// whole pixels per point. fr_mac_window_new keeps a picture of at most this
// many points at its exact size, so this is the largest picture shown 1:1.
// Main thread (AppKit screens).
int fr_mac_window_box(uint32_t reserved, uint32_t *width, uint32_t *height, uint32_t *scale) {
    if (!width || !height || !scale) return FR_MAC_INVALID;
    if (![NSThread isMainThread]) return FR_MAC_THREAD;
    @autoreleasepool {
        NSScreen *screen = NSScreen.mainScreen;
        if (!screen) return FR_MAC_GEOMETRY;
        double s = screen.backingScaleFactor;
        if (!(s >= 1 && s <= 4) || s != floor(s)) return FR_MAC_GEOMETRY;
        NSRect visible = screen.visibleFrame;
        double w = floor(visible.size.width) * s;
        double h = floor(visible.size.height - 28 - (double)reserved) * s;
        if (!(w >= 64 && h >= 64 && w <= UINT32_MAX && h <= UINT32_MAX)) return FR_MAC_GEOMETRY;
        *width = (uint32_t)w;
        *height = (uint32_t)h;
        *scale = (uint32_t)s;
        return FR_MAC_OK;
    }
}

// NSEvent timestamps and systemUptime use the same monotonic boot-time domain.
// Return the current value separately so a queued key retains its original age.
double fr_mac_event_time(void) {
    return NSProcessInfo.processInfo.systemUptime;
}

int fr_mac_window_new(uint32_t width, uint32_t height, uint32_t point_width,
                      uint32_t point_height, uint32_t sharing_flags, const char *title,
                      size_t title_len, void **out) {
    if (!out || width < 16 || height < 16 || point_width < 1 || point_height < 1 ||
        (sharing_flags & ~FR_MAC_SHARE_ALL) || (title_len && !title) ||
        ((sharing_flags & FR_MAC_FILE_SELECTION) && !(sharing_flags & FR_MAC_SHARE_UPLOAD)))
        return FR_MAC_INVALID;
    *out = NULL;
    if (![NSThread isMainThread]) return FR_MAC_THREAD;
    @autoreleasepool {
        NSRect visible = NSScreen.mainScreen ? NSScreen.mainScreen.visibleFrame
                                             : NSMakeRect(0, 0, 1280, 800);
        if (sharing_flags &&
            fr_sharing_width(sharing_flags) > visible.size.width)
            return FR_MAC_GEOMETRY;
        int status = FR_MAC_OK;
        FrRenderer *renderer = renderer_new(width, height, &status);
        if (!renderer) return status;
        FrWindowOwner *owner = [FrWindowOwner new];
        owner->renderer = renderer;
        owner->sharingFlags = sharing_flags;
        FrWindowLifetime *signal = [FrWindowLifetime new];
        if (!signal) return FR_MAC_ALLOCATION;
        atomic_init(&signal->unavailable, false);
        owner->lifetime = signal;
        owner->lifetimeCenter = NSWorkspace.sharedWorkspace.notificationCenter;
        owner->lifetimeObservers = [NSMutableArray arrayWithCapacity:3];
        if (!owner->lifetimeCenter || !owner->lifetimeObservers) return FR_MAC_ALLOCATION;
        for (NSNotificationName name in @[NSWorkspaceWillSleepNotification,
                                           NSWorkspaceScreensDidSleepNotification,
                                           NSWorkspaceSessionDidResignActiveNotification]) {
            id token = [owner->lifetimeCenter addObserverForName:name object:nil queue:nil
                usingBlock:^(NSNotification *note) {
                    (void)note;
                    atomic_store_explicit(&signal->unavailable, true, memory_order_release);
                }];
            if (!token) return FR_MAC_UNAVAILABLE;
            [owner->lifetimeObservers addObject:token];
        }
        // Start at the remote display's logical size in points, so a 2x
        // remote lands 1:1 on a 2x screen and a 1x remote keeps its natural
        // size. Only a picture that does not fit the visible screen (below
        // the title bar) is shrunk, with a margin and the aspect ratio kept:
        // any shrink resamples, so a fitting picture keeps its exact size.
        double chromeHeight = fr_sharing_height(sharing_flags);
        double minimumWidth = sharing_flags
                                  ? fr_sharing_width(sharing_flags)
                                  : 1;
        double s = 1.0;
        if (point_width > visible.size.width ||
            point_height > visible.size.height - 28 - chromeHeight)
            s = fmin(visible.size.width * 0.9 / point_width,
                     fmax(1, visible.size.height * 0.9 - 28 - chromeHeight) / point_height);
        NSRect frame = NSMakeRect(0, 0, fmax(minimumWidth, floor(point_width * s)),
                                  fmax(1, floor(point_height * s)) + chromeHeight);
        NSWindowStyleMask style = NSWindowStyleMaskTitled | NSWindowStyleMaskClosable |
                                  NSWindowStyleMaskMiniaturizable | NSWindowStyleMaskResizable;
        NSWindow *window = [[NSWindow alloc] initWithContentRect:frame
                                                       styleMask:style
                                                         backing:NSBackingStoreBuffered
                                                           defer:NO];
        window.releasedWhenClosed = NO;
        window.acceptsMouseMovedEvents = YES;
        window.delegate = owner;
        if (!sharing_flags) window.contentAspectRatio = NSMakeSize(width, height);
        // The Metal view keeps its own picture aspect ratio. Native chrome has
        // fixed point height and must not be folded into that pixel geometry.
        if (sharing_flags) window.contentMinSize = NSMakeSize(minimumWidth, chromeHeight + 64);
        window.backgroundColor = NSColor.blackColor;
        NSString *name = title_len ? [[NSString alloc] initWithBytes:title
                                                              length:title_len
                                                            encoding:NSUTF8StringEncoding]
                                   : @"FrankenRemote";
        window.title = name ? name : @"FrankenRemote";
        FrMetalView *view = [[FrMetalView alloc] initWithFrame:frame];
        view.owner = owner;
        owner->window = window;
        owner->view = view;
        view.layerContentsRedrawPolicy = NSViewLayerContentsRedrawDuringViewResize;
        view.wantsLayer = YES;
        if (sharing_flags) {
            FrContentView *content = [[FrContentView alloc] initWithFrame:frame];
            content.owner = owner;
            owner->content = content;
            owner->sharing = [[FrSharingView alloc] initWithFrame:NSZeroRect];
            [content addSubview:view];
            [content addSubview:owner->sharing];
            [owner buildSharing];
            window.contentView = content;
        } else {
            window.contentView = view;
        }
        [owner layout];
        [window makeFirstResponder:view];
        [window center];
        [window makeKeyAndOrderFront:nil];
        if (@available(macOS 14.0, *)) {
            [NSApp activate];
        } else {
#pragma clang diagnostic push
#pragma clang diagnostic ignored "-Wdeprecated-declarations"
            [NSApp activateIgnoringOtherApps:YES];
#pragma clang diagnostic pop
        }
        [owner layout];
        *out = (__bridge_retained void *)owner;
    }
    return FR_MAC_OK;
}


// Content-free status only: paths, file names and file contents never enter
// this chrome. Statuses: 0 off/absent, 1 waiting, 2 active, 3 finished,
// 4 cancelled, 5 failed. Rust supplies actual publication/cleanup evidence;
// the toolbar NEVER infers successful completion from staged byte counts.
int fr_mac_window_sharing_update(void *handle,
                                 uint32_t upload_status, uint64_t upload_done, uint64_t upload_total,
                                 uint32_t download_status, uint64_t download_done, uint64_t download_total) {
    if (!handle || !fr_sharing_status(upload_status) || !fr_sharing_status(download_status) ||
        upload_done > upload_total || download_done > download_total)
        return FR_MAC_INVALID;
    if (![NSThread isMainThread]) return FR_MAC_THREAD;
    FrWindowOwner *owner = (__bridge FrWindowOwner *)handle;
    if ((!(owner->sharingFlags & FR_MAC_SHARE_UPLOAD) &&
         (upload_status || upload_done || upload_total)) ||
        (!(owner->sharingFlags & FR_MAC_SHARE_DOWNLOAD) &&
         (download_status || download_done || download_total)))
        return FR_MAC_INVALID;
    BOOL changed = owner->uploadStatus != upload_status || owner->uploadDone != upload_done ||
                   owner->uploadTotal != upload_total || owner->downloadStatus != download_status ||
                   owner->downloadDone != download_done || owner->downloadTotal != download_total;
    if (!changed) return FR_MAC_OK;
    @autoreleasepool {
        owner->uploadStatus = upload_status;
        owner->uploadDone = upload_done;
        owner->uploadTotal = upload_total;
        owner->downloadStatus = download_status;
        owner->downloadDone = download_done;
        owner->downloadTotal = download_total;
        if (owner->uploadLabel) {
            owner->uploadLabel.stringValue =
                owner->uploadCancelled && (upload_status == 1 || upload_status == 2)
                    ? @"Send: Cancelling"
                    : fr_transfer_label(@"Send", upload_status, upload_done, upload_total);
            owner->uploadCancel.enabled = !owner->uploadCancelled &&
                                           (upload_status == 1 || upload_status == 2);
        }
        if (owner->downloadLabel) {
            owner->downloadLabel.stringValue =
                owner->downloadCancelled && (download_status == 1 || download_status == 2)
                    ? @"Receive: Cancelling"
                    : fr_transfer_label(@"Receive", download_status, download_done, download_total);
            owner->downloadCancel.enabled = !owner->downloadCancelled &&
                                             (download_status == 1 || download_status == 2);
        }
    }
    return FR_MAC_OK;
}

// Drain local cancellation latches every pump. "Visible" means mapped and
// unclipped AppKit chrome; it is not an unobscured privacy/UI heartbeat proof.
// File ownership and cancellation remain with the original Rust session.
int fr_mac_window_sharing_state(void *handle, uint32_t *actions, int *visible) {
    if (!handle || !actions || !visible) return FR_MAC_INVALID;
    if (![NSThread isMainThread]) return FR_MAC_THREAD;
    FrWindowOwner *owner = (__bridge FrWindowOwner *)handle;
    *actions = owner->sharingActions;
    owner->sharingActions = 0;
    NSWindow *w = owner->window;
    *visible = owner->sharingFlags && owner->sharing && !owner->sharing.hidden &&
               w.isVisible && !w.isMiniaturized && w.isOnActiveSpace &&
               (w.occlusionState & NSWindowOcclusionStateVisible) &&
               NSContainsRect(owner->sharing.visibleRect, owner->sharing.bounds)
                   ? 1 : 0;
    return FR_MAC_OK;
}

int fr_mac_window_display_change_enabled(void *handle, int enabled) {
    if (!handle || (enabled != 0 && enabled != 1)) return FR_MAC_INVALID;
    if (![NSThread isMainThread]) return FR_MAC_THREAD;
    FrWindowOwner *owner = (__bridge FrWindowOwner *)handle;
    if (!owner->changeDisplay) return FR_MAC_INVALID;
    if (owner->nativeStopPending) return FR_MAC_UNAVAILABLE;
    if (enabled && owner->displayChangeUsed && owner->displayChangeAt == 0) {
        owner->displayChangeUsed = NO;
        owner->changeDisplay.title = @"Change Display";
    }
    BOOL allowed = enabled && !owner->displayChangeUsed;
    if (owner->changeDisplay.enabled != allowed) owner->changeDisplay.enabled = allowed;
    return owner->nativeStopPending ? FR_MAC_UNAVAILABLE : FR_MAC_OK;
}

int fr_mac_window_take_display_change(void *handle, double *captured) {
    if (!handle || !captured) return FR_MAC_INVALID;
    if (![NSThread isMainThread]) return FR_MAC_THREAD;
    FrWindowOwner *owner = (__bridge FrWindowOwner *)handle;
    *captured = owner->displayChangeAt;
    owner->displayChangeAt = 0;
    return FR_MAC_OK;
}

// Contexts are opaque, monotonic identifiers issued by the original Rust
// source owner. A changed/absent context clears draft and pending bytes before
// any new context can be exposed. Temporary readiness loss only disables Send.
int fr_mac_window_text_update(void *handle, uint64_t context, uint64_t until, int eligible) {
    if (!handle || (eligible != 0 && eligible != 1) || (!context && until))
        return FR_MAC_INVALID;
    if (![NSThread isMainThread]) return FR_MAC_THREAD;
    FrWindowOwner *owner = (__bridge FrWindowOwner *)handle;
    if (!owner->textScroll) return context ? FR_MAC_INVALID : FR_MAC_OK;
    [owner checkSessionUnavailable];
    // A disabled ordinary update is also used for a readiness/context lapse.
    // It is not evidence that Rust has already stopped its original owner.
    if (owner->nativeStopPending || owner->textCleaning || owner->textOpening)
        return FR_MAC_UNAVAILABLE;
    if (owner->textParked) return context ? FR_MAC_UNAVAILABLE : FR_MAC_OK;
    if (owner->textContext != context) {
        BOOL hadDraft = owner->textActive || owner->textPending;
        owner->textContext = 0;
        owner->textUntil = 0;
        owner->textEligible = NO;
        [owner clearText:hadDraft ? @"Text cancelled: view or control changed." : nil];
        if (owner->nativeStopPending) return FR_MAC_UNAVAILABLE;
        owner->textContextSince = NSProcessInfo.processInfo.systemUptime;
        owner->textContext = context;
    }
    // This bound is opaque: only Rust compares its own clock. A zero bound
    // pauses Send without discarding an otherwise still-valid local draft.
    owner->textUntil = until;
    owner->textEligible = context && eligible && until;
    if ((owner->textActive || owner->textPending) && ![owner textWindowReady]) {
        [owner stopNativeInput:@"Text cancelled: the visible window changed."];
        return FR_MAC_UNAVAILABLE;
    }
    if (owner->nativeStopPending) return FR_MAC_UNAVAILABLE;
    // A pending display handoff must wait for the original producer to park.
    if (!owner->displayChangeAt) [owner cleanupRetiredText];
    if (owner->nativeStopPending) return FR_MAC_UNAVAILABLE;
    [owner updateTextControls];
    [owner checkSessionUnavailable];
    return owner->nativeStopPending ? FR_MAC_UNAVAILABLE : FR_MAC_OK;
}

// Explicit custody: fenced=1 means Rust has stopped the original input/media
// owners (or no input owner exists); fenced=0 is a parked display handoff.
// Parked mode performs scalar retirement only and leaves the native editor
// retained until a later fully-fenced cleanup. It never calls AppKit/IME.
// Return 0 without a prior negative, +1 for a prior negative (a parked veto,
// or fully-fenced completed cleanup), and a negative error for incomplete
// fully-fenced cleanup. A parked success is not proof of native cleanup.
int fr_mac_window_text_stopped(void *handle, int fenced) {
    if (!handle || (fenced != 0 && fenced != 1)) return FR_MAC_INVALID;
    if (![NSThread isMainThread]) return FR_MAC_THREAD;
    FrWindowOwner *owner = (__bridge FrWindowOwner *)handle;
    [owner checkSessionUnavailable];
    BOOL hadNegative = owner->nativeStopPending;
    if (!fenced) {
        owner->textParked = YES;
        owner->textContext = 0;
        owner->textUntil = 0;
        owner->textEligible = NO;
        [owner retireText:owner->textNotice];
        return hadNegative ||
               atomic_load_explicit(&owner->lifetime->unavailable, memory_order_acquire)
                   ? 1 : FR_MAC_OK;
    }
    owner->nativeStopPending = NO;
    owner->textParked = NO;
    owner->textContext = 0;
    owner->textUntil = 0;
    owner->textEligible = NO;
    [owner retireText:owner->textNotice];
    [owner cleanupRetiredText];
    if (!owner->nativeStopPending) [owner finishStoppedUI];
    if (!owner->nativeStopPending) [owner updateTextControls];
    [owner checkSessionUnavailable];
    if (owner->nativeStopPending || owner->retiredTextEditor || owner->textEditor ||
        owner->textCleaning || owner->textOpening)
        return FR_MAC_UNAVAILABLE;
    return hadNegative ? 1 : FR_MAC_OK;
}

// Copy one bounded original Send at most once. The visible draft stays local
// and disabled until the synchronous admission acknowledgement below. Native
// state cannot renew the Send timestamp, retry it, or construct wire input.
int fr_mac_window_text_take(void *handle, uint8_t *bytes, size_t capacity, size_t *length,
                            uint64_t *context, double *captured, uint64_t *until) {
    if (!handle || !bytes || !length || !context || !captured || !until) return FR_MAC_INVALID;
    *length = 0;
    *context = 0;
    *captured = 0;
    *until = 0;
    if (![NSThread isMainThread]) return FR_MAC_THREAD;
    FrWindowOwner *owner = (__bridge FrWindowOwner *)handle;
    [owner checkSessionUnavailable];
    if (owner->nativeStopPending) return FR_MAC_UNAVAILABLE;
    if (!owner->textPending || owner->textTaken) return FR_MAC_OK;
    if (!owner->textContext || owner->textContext != owner->textPendingContext) {
        [owner clearText:@"Text cancelled: view or control changed."];
        return owner->nativeStopPending ? FR_MAC_UNAVAILABLE : FR_MAC_OK;
    }
    if (![owner textWindowReady]) {
        [owner stopNativeInput:@"Text cancelled: the visible window changed."];
        return FR_MAC_UNAVAILABLE;
    }
    if (owner->nativeStopPending) return FR_MAC_UNAVAILABLE;
    if (!owner->textLength || owner->textLength > FR_MAC_TEXT_BYTES ||
        !owner->textPendingUntil || capacity < owner->textLength)
        return FR_MAC_INVALID;
    *length = owner->textLength;
    *context = owner->textPendingContext;
    *captured = owner->textPendingAt;
    *until = owner->textPendingUntil;
    memcpy(bytes, owner->textBytes, owner->textLength);
    memset(owner->textBytes, 0, sizeof(owner->textBytes));
    owner->textLength = 0;
    owner->textTaken = YES;
    return FR_MAC_OK;
}

int fr_mac_window_text_complete(void *handle, uint64_t context, int admitted) {
    if (!handle || !context || (admitted != 0 && admitted != 1)) return FR_MAC_INVALID;
    if (![NSThread isMainThread]) return FR_MAC_THREAD;
    FrWindowOwner *owner = (__bridge FrWindowOwner *)handle;
    [owner checkSessionUnavailable];
    if (owner->nativeStopPending) return FR_MAC_UNAVAILABLE;
    // A late completion cannot touch a different draft, including one cleared
    // by a native focus/change callback after Send was captured.
    if (!owner->textPending || !owner->textTaken || context != owner->textPendingContext)
        return FR_MAC_OK;
    if (context == owner->textContext && ![owner textWindowReady]) {
        [owner stopNativeInput:@"Text cancelled: the visible window changed."];
        return FR_MAC_UNAVAILABLE;
    }
    if (owner->nativeStopPending) return FR_MAC_UNAVAILABLE;
    if (admitted) {
        [owner clearText:@"Text queued for remote input."];
    } else if (context == owner->textContext) {
        owner->textPending = NO;
        owner->textTaken = NO;
        owner->textPendingContext = 0;
        owner->textPendingUntil = 0;
        owner->textPendingAt = 0;
        owner->textNotice = @"Text was not queued. Click Send again only if you still intend to send.";
        [owner updateTextControls];
        if (!owner->nativeStopPending && owner->textEditor)
            [owner->window makeFirstResponder:owner->textEditor];
    } else {
        [owner clearText:@"Text cancelled: view or control changed."];
    }
    return owner->nativeStopPending ? FR_MAC_UNAVAILABLE : FR_MAC_OK;
}

// Adopt exactly once, only after all validation. Failure retains nothing.
int fr_mac_window_clipboard_bind(void *handle, void *context, FrClipboardStop stop,
                                  FrClipboardRelease release) {
    if (!handle || !context || !stop || !release) return FR_MAC_INVALID;
    if (![NSThread isMainThread]) return FR_MAC_THREAD;
    FrWindowOwner *owner = (__bridge FrWindowOwner *)handle;
    if (!owner->clipboardOff || owner->clipboardContext) return FR_MAC_INVALID;
    owner->clipboardContext = context;
    owner->clipboardStop = stop;
    owner->clipboardRelease = release;
    if (owner->clipboardStopped) stop(context, owner->clipboardUserOff ? 1 : 0);
    return FR_MAC_OK;
}

// Adopt one original selection owner. Callbacks and view registration are
// main-thread only; a failed bind retains no pointer.
int fr_mac_window_file_selection_bind(void *handle, void *context,
        FrFileSelectionReady ready, FrFileSelectionSubmit submit,
        FrFileSelectionStop stop, FrFileSelectionRelease release) {
    if (!handle || !context || !ready || !submit || !stop || !release) return FR_MAC_INVALID;
    if (![NSThread isMainThread]) return FR_MAC_THREAD;
    FrWindowOwner *owner = (__bridge FrWindowOwner *)handle;
    if (!owner->sendCopiedFiles || owner->fileSelectionContext) return FR_MAC_INVALID;
    owner->fileSelectionContext = context;
    owner->fileSelectionReady = ready;
    owner->fileSelectionSubmit = submit;
    owner->fileSelectionStop = stop;
    owner->fileSelectionRelease = release;
    if (owner->fileSelectionStopped || owner->nativeStopPending) stop(context);
    else [owner->view registerForDraggedTypes:@[NSPasteboardTypeFileURL]];
    return FR_MAC_OK;
}
int fr_mac_window_file_selection_update(void *handle) {
    if (!handle) return FR_MAC_INVALID;
    if (![NSThread isMainThread]) return FR_MAC_THREAD;
    FrWindowOwner *owner = (__bridge FrWindowOwner *)handle;
    [owner updateFileSelections];
    return FR_MAC_OK;
}

int fr_mac_window_clipboard_update(void *handle, uint32_t state) {
    if (!handle || state > 4) return FR_MAC_INVALID;
    if (![NSThread isMainThread]) return FR_MAC_THREAD;
    FrWindowOwner *owner = (__bridge FrWindowOwner *)handle;
    if (!owner->clipboardOff) return FR_MAC_INVALID;
    static NSString *const labels[] = {@"Clipboard: Waiting", @"Clipboard: On",
        @"Clipboard: Off", @"Clipboard: N/A", @"Clipboard: Ended"};
    owner->clipboardLabel.stringValue = owner->clipboardStopped ? labels[2] : labels[state];
    owner->clipboardOff.enabled = !owner->clipboardStopped && state < 2;
    return FR_MAC_OK;
}

int fr_mac_window_clipboard_visible(void *handle, int *visible) {
    if (!handle || !visible) return FR_MAC_INVALID;
    if (![NSThread isMainThread]) return FR_MAC_THREAD;
    FrWindowOwner *owner = (__bridge FrWindowOwner *)handle;
    NSWindow *w = owner->window;
    *visible = owner->clipboardOff && !owner->clipboardStopped &&
               !owner->sharing.hidden && !owner->clipboardOff.hidden &&
               w.isKeyWindow && w.isVisible && !w.isMiniaturized && w.isOnActiveSpace &&
               (w.occlusionState & NSWindowOcclusionStateVisible) &&
               NSContainsRect(owner->sharing.visibleRect, owner->clipboardOff.frame) ? 1 : 0;
    return FR_MAC_OK;
}

// Waits up to `timeout` seconds for the first event, then drains one bounded
// batch without waiting. Returns the number of translated events copied out.
size_t fr_mac_window_poll(void *handle, double timeout, FrMacEvent *out, size_t capacity,
                          int *overflow) {
    if (!handle || ![NSThread isMainThread]) return 0;
    FrWindowOwner *owner = (__bridge FrWindowOwner *)handle;
    size_t dequeued = 0;
    [owner checkSessionUnavailable];
    @autoreleasepool {
        NSDate *until = timeout > 0 ? [NSDate dateWithTimeIntervalSinceNow:timeout]
                                    : [NSDate distantPast];
        // A display change or captured Send must inspect the full bounded
        // batch so queued close, escape or focus loss can veto the action.
        // File-only cancel still reaches Rust immediately when neither is pending.
        while (!owner->nativeStopPending &&
               (!owner->sharingActions || owner->displayChangeAt > 0 || owner->textPending) &&
               owner->count < FR_MAC_QUEUE && dequeued < FR_MAC_PUMP_EVENTS) {
            NSEvent *event = [NSApp nextEventMatchingMask:NSEventMaskAny
                                                untilDate:until
                                                   inMode:NSDefaultRunLoopMode
                                                  dequeue:YES];
            if (!event) break;
            // Count every AppKit event, including ignored events and events
            // for other windows: all of them consume this owner's pump turn.
            dequeued++;
            if ([owner translate:event] && !owner->nativeStopPending) [NSApp sendEvent:event];
            until = [NSDate distantPast];
        }
        // A local cancellation must reach Rust before any further UI drawing.
        if (!owner->sharingActions && !owner->nativeStopPending &&
            !owner->displayChangeAt && !owner->textPending &&
            owner->count < FR_MAC_QUEUE && dequeued < FR_MAC_PUMP_EVENTS)
            [NSApp updateWindows];
    }
    // At either ceiling, a queued close, focus change or emergency release
    // may remain uninspected. Fail closed before granting or exporting input.
    // An exactly-full batch with an empty AppKit queue is conservatively
    // treated as incomplete too; checking that would require another poll.
    if (owner->count == FR_MAC_QUEUE || dequeued == FR_MAC_PUMP_EVENTS) {
        owner->overflow = YES;
        [owner stopNativeInput:@"Text cancelled: native input overflow."];
    }
    size_t n = 0;
    while (n < capacity && owner->count) {
        out[n++] = owner->queue[owner->head];
        owner->head = (owner->head + 1) % FR_MAC_QUEUE;
        owner->count--;
    }
    if (overflow) {
        *overflow = owner->overflow || owner->count ? 1 : 0;
        owner->overflow = NO;
    }
    return n;
}

intptr_t fr_mac_window_number(void *handle) {
    if (!handle) return 0;
    FrWindowOwner *owner = (__bridge FrWindowOwner *)handle;
    return owner->window.windowNumber;
}

// Desired picture size, window visibility, and whether a real retained picture
// was submitted at that exact geometry. No main-thread wait on native work.
int fr_mac_window_state(void *handle, uint32_t *width, uint32_t *height, int *key,
                        int *visible, int *geometry_current) {
    if (!handle || !width || !height || !key || !visible || !geometry_current)
        return FR_MAC_INVALID;
    if (![NSThread isMainThread]) return FR_MAC_THREAD;
    FrWindowOwner *owner = (__bridge FrWindowOwner *)handle;
    FrRenderer *r = owner->renderer;
    os_unfair_lock_lock(&r->geometryLock);
    CGRect f = r->geometry.fit;
    *geometry_current = r->submittedPicture && r->submittedRevision == r->geometry.revision &&
                        !owner->fullScreenMoving && !owner->fullScreenFenced;
    os_unfair_lock_unlock(&r->geometryLock);
    *width = f.size.width >= 1 ? (uint32_t)f.size.width : 0;
    *height = f.size.height >= 1 ? (uint32_t)f.size.height : 0;
    NSWindow *w = owner->window;
    BOOL available = !atomic_load_explicit(&owner->lifetime->unavailable, memory_order_acquire);
    *key = available && w.isKeyWindow ? 1 : 0;
    *visible = available && w.isVisible && !w.isMiniaturized &&
                       (w.occlusionState & NSWindowOcclusionStateVisible)
                   ? 1
                   : 0;
    return FR_MAC_OK;
}

// Arm after the original startup produced a real current picture. Rust owns
// the context until success; this exact window then owns its one final release.
int fr_mac_window_resize_bind(void *handle, void *context, FrResizeChanged changed,
                              FrResizeRelease release, FrMacResizeSample *initial) {
    if (!handle || !context || !changed || !release || !initial) return FR_MAC_INVALID;
    if (![NSThread isMainThread]) return FR_MAC_THREAD;
    FrWindowOwner *owner = (__bridge FrWindowOwner *)handle;
    if (owner->resizeContext) return FR_MAC_INVALID;
    FrMacResizeSample sample = fr_resize_sample(owner);
    os_unfair_lock_lock(&owner->renderer->geometryLock);
    uint64_t revision = sample.revision - owner->fullScreenBumps;
    BOOL current = owner->renderer->submittedPicture &&
        owner->renderer->submittedRevision == revision &&
        owner->renderer->geometry.revision == revision;
    owner->resizeSourceWidth = owner->renderer->geometry.sourceWidth;
    owner->resizeSourceHeight = owner->renderer->geometry.sourceHeight;
    owner->resizeDrawable = owner->renderer->geometry.drawable;
    owner->resizeScale = owner->renderer->geometry.scale;
    os_unfair_lock_unlock(&owner->renderer->geometryLock);
    if (!current || !sample.eligible) return FR_MAC_UNAVAILABLE;
    owner->resizeContext = context;
    owner->resizeChanged = changed;
    owner->resizeRelease = release;
    owner->resizeRevision = sample.revision;
    *initial = sample;
    return FR_MAC_OK;
}

// A complete native event turn precedes this seal. Later terminal callbacks
// may still veto it; native owner release clears callbacks before our own close.
int fr_mac_window_resize_seal(void *handle, FrMacResizeSample *sample) {
    if (!handle || !sample) return FR_MAC_INVALID;
    if (![NSThread isMainThread]) return FR_MAC_THREAD;
    FrWindowOwner *owner = (__bridge FrWindowOwner *)handle;
    if (!owner->resizeContext || owner->resizeSealed || owner->fullScreenMoving ||
        owner->fullScreenFenced)
        return FR_MAC_UNAVAILABLE;
    *sample = fr_resize_sample(owner);
    owner->resizeSealed = YES;
    return FR_MAC_OK;
}

// Whether AppKit is still animating this window into or out of full screen
// (or a fenced transition has not reported its shown, settled size yet);
// with `enter`, first take a NEW window (no watcher yet) into full screen.
int fr_mac_window_full_screen(void *handle, int enter, int *moving) {
    if (!handle || !moving || (enter != 0 && enter != 1)) return FR_MAC_INVALID;
    if (![NSThread isMainThread]) return FR_MAC_THREAD;
    FrWindowOwner *owner = (__bridge FrWindowOwner *)handle;
    if (enter) {
        if (owner->resizeContext) return FR_MAC_INVALID;
        if (!(owner->window.styleMask & NSWindowStyleMaskFullScreen) && !owner->fullScreenMoving)
            [owner->window toggleFullScreen:nil];
    }
    *moving = owner->fullScreenMoving || owner->fullScreenFenced ? 1 : 0;
    return FR_MAC_OK;
}

// Restore a checked local point position only while constructing a NEW window.
// A removed monitor is a refusal; never reopen invisibly off-screen.
int fr_mac_window_place(void *handle, int32_t x, int32_t y) {
    if (!handle) return FR_MAC_INVALID;
    if (![NSThread isMainThread]) return FR_MAC_THREAD;
    FrWindowOwner *owner = (__bridge FrWindowOwner *)handle;
    if (owner->resizeContext) return FR_MAC_INVALID;
    NSRect frame = owner->window.frame;
    frame.origin = NSMakePoint(x, y);
    // A backing-scale transition commonly straddles two screens. Select only
    // a currently intersecting screen, then make the fresh title accessible;
    // no original off-screen location can silently migrate to another display.
    NSScreen *target = nil;
    double largest = 0;
    for (NSScreen *screen in NSScreen.screens) {
        NSRect overlap = NSIntersectionRect(frame, screen.visibleFrame);
        double area = NSWidth(overlap) * NSHeight(overlap);
        if (area > largest) { largest = area; target = screen; }
    }
    if (!target) return FR_MAC_GEOMETRY;
    NSRect visible = target.visibleFrame;
    double titleWidth = fmin(frame.size.width, 160);
    if (NSWidth(visible) < titleWidth || NSHeight(visible) < 28) return FR_MAC_GEOMETRY;
    double maxX = NSMaxX(visible) - fmin(frame.size.width, NSWidth(visible));
    frame.origin.x = fmax(NSMinX(visible), fmin(frame.origin.x, maxX));
    double top = fmax(NSMinY(visible) + 28,
                      fmin(NSMaxY(frame), NSMaxY(visible)));
    frame.origin.y = top - frame.size.height;
    NSRect title = NSMakeRect(frame.origin.x, top - 28, titleWidth, 28);
    if (!NSContainsRect(visible, title)) return FR_MAC_GEOMETRY;
    [owner->window setFrameOrigin:frame.origin];
    [owner layout];
    return FR_MAC_OK;
}

// Fix or free the window's size. An input layout is valid for one size only.
int fr_mac_window_set_resizable(void *handle, int resizable) {
    if (!handle) return FR_MAC_INVALID;
    if (![NSThread isMainThread]) return FR_MAC_THREAD;
    FrWindowOwner *owner = (__bridge FrWindowOwner *)handle;
    NSWindowStyleMask style = owner->window.styleMask;
    owner->window.styleMask =
        resizable ? (style | NSWindowStyleMaskResizable) : (style & ~NSWindowStyleMaskResizable);
    return FR_MAC_OK;
}

// A renderer with no window, drawing into a detached layer of a fixed pixel
// size: decode and presentation tests off the main thread.
int fr_mac_surface_offscreen(uint32_t width, uint32_t height, void **out) {
    if (!out || width < 16 || height < 16) return FR_MAC_INVALID;
    *out = NULL;
    @autoreleasepool {
        int status = FR_MAC_OK;
        FrRenderer *renderer = renderer_new(width, height, &status);
        if (!renderer) return status;
        [renderer resizeTo:CGSizeMake(width, height) scale:1];
        *out = (__bridge_retained void *)renderer;
    }
    return FR_MAC_OK;
}

// A +1 reference to the window's renderer, for the media thread.
void *fr_mac_window_renderer(void *handle) {
    if (!handle) return NULL;
    FrWindowOwner *owner = (__bridge FrWindowOwner *)handle;
    return (__bridge_retained void *)owner->renderer;
}

// The remote cursor: kind 0 hides it (the shape is kept), 1 moves the hotspot
// of the existing shape, 2 replaces the shape (≤ 256×256 RGBA8, straight
// alpha) and shows it. Redraws the retained picture with the new cursor when
// one has been presented. Invalid updates change nothing; a native draw failure
// ends the original media owner without pretending the cursor was displayed.
int fr_mac_renderer_cursor(void *handle, int kind, int32_t x, int32_t y, uint32_t width,
                           uint32_t height, uint32_t hotspot_x, uint32_t hotspot_y,
                           const uint8_t *rgba, size_t len) {
    if (!handle) return FR_MAC_INVALID;
    @autoreleasepool {
        FrRenderer *r = (__bridge FrRenderer *)handle;
        id<MTLTexture> shape = nil;
        if (kind == 2) {
            if (!rgba || width == 0 || height == 0 || width > 256 || height > 256 ||
                hotspot_x >= width || hotspot_y >= height || len != (size_t)width * height * 4) {
                return FR_MAC_INVALID;
            }
            MTLTextureDescriptor *td =
                [MTLTextureDescriptor texture2DDescriptorWithPixelFormat:MTLPixelFormatRGBA8Unorm
                                                                   width:width
                                                                  height:height
                                                               mipmapped:NO];
            td.usage = MTLTextureUsageShaderRead;
            shape = [r->device newTextureWithDescriptor:td];
            if (!shape) return FR_MAC_ALLOCATION;
            [shape replaceRegion:MTLRegionMake2D(0, 0, width, height)
                     mipmapLevel:0
                       withBytes:rgba
                     bytesPerRow:(NSUInteger)width * 4];
        } else if (kind != 0 && kind != 1) {
            return FR_MAC_INVALID;
        }
        os_unfair_lock_lock(&r->lock);
        int status = FR_MAC_OK;
        if (kind == 1 && !r->cursorShape) {
            status = FR_MAC_INVALID;
        } else {
            if (kind == 2) {
                r->cursorShape = shape;
                r->cursorWidth = width;
                r->cursorHeight = height;
                r->cursorHotX = hotspot_x;
                r->cursorHotY = hotspot_y;
            }
            r->cursorShown = kind != 0;
            if (kind != 0) {
                r->cursorLeft = (int64_t)x - r->cursorHotX;
                r->cursorTop = (int64_t)y - r->cursorHotY;
            }
            if (r->last) status = [r drawLocked];
        }
        os_unfair_lock_unlock(&r->lock);
        return status;
    }
}

// The original media owner calls this between requests. Repaint only retained
// real pixels; an empty/black surface cannot establish geometry readiness.
// A repaint neither increments `presented` nor creates frame/freshness evidence.
int fr_mac_renderer_refresh(void *handle) {
    if (!handle) return FR_MAC_INVALID;
    @autoreleasepool {
        FrRenderer *r = (__bridge FrRenderer *)handle;
        os_unfair_lock_lock(&r->lock);
        os_unfair_lock_lock(&r->geometryLock);
        BOOL dirty = r->redrawPending;
        os_unfair_lock_unlock(&r->geometryLock);
        int status = dirty && r->last ? [r drawLocked] : FR_MAC_OK;
        os_unfair_lock_unlock(&r->lock);
        return status;
    }
}

uint64_t fr_mac_renderer_presented(void *handle) {
    if (!handle) return 0;
    FrRenderer *r = (__bridge FrRenderer *)handle;
    os_unfair_lock_lock(&r->geometryLock);
    uint64_t n = r->presented;
    os_unfair_lock_unlock(&r->geometryLock);
    return n;
}

void fr_mac_renderer_release(void *handle) {
    if (handle) (void)(__bridge_transfer FrRenderer *)handle;
}

void fr_mac_window_free(void *handle) {
    if (!handle) return;
    @autoreleasepool {
        FrWindowOwner *owner = (__bridge_transfer FrWindowOwner *)handle;
        // Clear the original geometry callbacks before our own native teardown
        // generates focus/layout changes. No late callback can reach a new window.
        void *resizeContext = owner->resizeContext;
        FrResizeRelease resizeRelease = owner->resizeRelease;
        owner->resizeContext = NULL;
        owner->resizeChanged = NULL;
        owner->resizeRelease = NULL;
        if (resizeContext && resizeRelease) resizeRelease(resizeContext);
        [owner stopFileSelections];
        void *fileContext = owner->fileSelectionContext;
        FrFileSelectionRelease fileRelease = owner->fileSelectionRelease;
        owner->fileSelectionContext = NULL;
        owner->fileSelectionReady = NULL;
        owner->fileSelectionSubmit = NULL;
        owner->fileSelectionStop = NULL;
        owner->fileSelectionRelease = NULL;
        [owner->view unregisterDraggedTypes];
        if (fileContext && fileRelease) fileRelease(fileContext);
        atomic_store_explicit(&owner->lifetime->unavailable, true, memory_order_release);
        for (id observer in owner->lifetimeObservers) [owner->lifetimeCenter removeObserver:observer];
        [owner->lifetimeObservers removeAllObjects];
        owner->lifetimeCenter = nil;
        owner->nativeStopPending = NO; // Rust has stopped the original owners before free.
        owner->textParked = NO;
        owner->textContext = 0;
        owner->textUntil = 0;
        owner->textEligible = NO;
        [owner clearText:nil];
        owner->textEditor.owner = nil;
        owner->retiredTextEditor.owner = nil;
        [owner stopClipboard:NO];
        // Clear callbacks before releasing the unique Rust owner.
        void *context = owner->clipboardContext;
        FrClipboardRelease release = owner->clipboardRelease;
        owner->clipboardContext = NULL;
        owner->clipboardStop = NULL;
        owner->clipboardRelease = NULL;
        if (context && release) release(context);
        owner->window.delegate = nil;
        [owner->window orderOut:nil];
        [owner->window close];
        owner->view.owner = nil;
        owner->content.owner = nil;
        owner->window = nil;
        owner->view = nil;
        owner->content = nil;
        owner->sharing = nil;
        owner->textEditor = nil;
        owner->retiredTextEditor = nil;
        owner->textScroll = nil;
        owner->textLabel = nil;
        owner->textOpen = nil;
        owner->textSend = nil;
        owner->textCancel = nil;
        owner->textNotice = nil;
        owner->uploadLabel = nil;
        owner->downloadLabel = nil;
        owner->clipboardLabel = nil;
        owner->uploadCancel = nil;
        owner->downloadCancel = nil;
        owner->changeDisplay = nil;
        owner->clipboardOff = nil;
        owner->sendCopiedFiles = nil;
    }
}

// ----------------------------------------------------------------- decoder

@interface FrDecoder : NSObject {
  @public
    FrRenderer *renderer;
    CMVideoFormatDescriptionRef format;
    VTDecompressionSessionRef session;
    OSStatus status;
    CVImageBufferRef image;
}
@end

@implementation FrDecoder
- (void)dealloc {
    if (session) {
        VTDecompressionSessionInvalidate(session);
        CFRelease(session);
    }
    if (format) CFRelease(format);
    if (image) CVPixelBufferRelease(image);
}
@end

static void fr_decoded(void *refcon, void *frame, OSStatus status, VTDecodeInfoFlags flags,
                       CVImageBufferRef image, CMTime pts, CMTime duration) {
    (void)frame;
    (void)flags;
    (void)pts;
    (void)duration;
    FrDecoder *d = (__bridge FrDecoder *)refcon;
    d->status = status;
    if (d->image) CVPixelBufferRelease(d->image);
    d->image = status == noErr && image ? CVPixelBufferRetain(image) : NULL;
}

// Parameter sets arrive already admitted by Rust's HEVC guard.
int fr_mac_decoder_new(void *renderer_handle, const uint8_t *vps, size_t vps_len,
                       const uint8_t *sps, size_t sps_len, const uint8_t *pps, size_t pps_len,
                       uint32_t width, uint32_t height, int full_range, void **out) {
    if (!renderer_handle || !out || !vps || !sps || !pps || !vps_len || !sps_len || !pps_len)
        return FR_MAC_INVALID;
    *out = NULL;
    @autoreleasepool {
        FrDecoder *d = [FrDecoder new];
        d->renderer = (__bridge FrRenderer *)renderer_handle;
        const uint8_t *sets[3] = {vps, sps, pps};
        const size_t sizes[3] = {vps_len, sps_len, pps_len};
        if (CMVideoFormatDescriptionCreateFromHEVCParameterSets(kCFAllocatorDefault, 3, sets, sizes, 4,
                                                                NULL, &d->format) != noErr)
            return FR_MAC_CODEC;
        CMVideoDimensions dims = CMVideoFormatDescriptionGetDimensions(d->format);
        if ((uint32_t)dims.width != width || (uint32_t)dims.height != height) return FR_MAC_GEOMETRY;
        OSType pixel = full_range ? kCVPixelFormatType_420YpCbCr8BiPlanarFullRange
                                  : kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange;
        NSDictionary *attributes = @{
            (id)kCVPixelBufferPixelFormatTypeKey : @(pixel),
            (id)kCVPixelBufferMetalCompatibilityKey : @YES,
            (id)kCVPixelBufferIOSurfacePropertiesKey : @{},
        };
        NSDictionary *spec = @{
            (id)kVTVideoDecoderSpecification_EnableHardwareAcceleratedVideoDecoder : @YES,
        };
        VTDecompressionOutputCallbackRecord callback = {fr_decoded, (__bridge void *)d};
        if (VTDecompressionSessionCreate(kCFAllocatorDefault, d->format, (__bridge CFDictionaryRef)spec,
                                         (__bridge CFDictionaryRef)attributes, &callback,
                                         &d->session) != noErr)
            return FR_MAC_UNAVAILABLE;
        VTSessionSetProperty(d->session, kVTDecompressionPropertyKey_RealTime, kCFBooleanTrue);
        os_unfair_lock_lock(&d->renderer->lock);
        d->renderer->fullRange = full_range ? YES : NO;
        os_unfair_lock_lock(&d->renderer->geometryLock);
        FrGeometry *g = &d->renderer->geometry;
        BOOL sourceChanged = g->sourceWidth != width || g->sourceHeight != height;
        if (sourceChanged) {
            g->sourceWidth = width;
            g->sourceHeight = height;
            g->revision++;
            fr_refit(g);
            d->renderer->submittedPicture = NO;
        }
        g->redraw++;
        d->renderer->redrawPending = YES;
        os_unfair_lock_unlock(&d->renderer->geometryLock);
        // A changed source needs its own decoded picture. A resize of the
        // window may refit retained pixels; a new source may not crop/relabel
        // the previous decoder's pixels as if they belonged to its geometry.
        if (sourceChanged && d->renderer->last) {
            CVPixelBufferRelease(d->renderer->last);
            d->renderer->last = NULL;
        }
        os_unfair_lock_unlock(&d->renderer->lock);
        *out = (__bridge_retained void *)d;
    }
    return FR_MAC_OK;
}

// Reports whether the session runs on a hardware decoder (1), not (0), or unknown (-1).
int fr_mac_decoder_hardware(void *handle) {
    if (!handle) return -1;
    FrDecoder *d = (__bridge FrDecoder *)handle;
    CFBooleanRef value = NULL;
    if (VTSessionCopyProperty(d->session, kVTDecompressionPropertyKey_UsingHardwareAcceleratedVideoDecoder,
                              kCFAllocatorDefault, &value) != noErr ||
        !value)
        return -1;
    int hardware = CFBooleanGetValue(value) ? 1 : 0;
    CFRelease(value);
    return hardware;
}

// Decodes one length-prefixed access unit synchronously and, when `present`
// is set, draws it. A decode-only unit still advances the reference chain.
int fr_mac_decoder_decode(void *handle, const uint8_t *bytes, size_t len, int64_t pts,
                          int present) {
    if (!handle || !bytes || !len) return FR_MAC_INVALID;
    FrDecoder *d = (__bridge FrDecoder *)handle;
    int result = FR_MAC_OK;
    @autoreleasepool {
        CMBlockBufferRef block = NULL;
        CMSampleBufferRef sample = NULL;
        if (CMBlockBufferCreateWithMemoryBlock(kCFAllocatorDefault, NULL, len, kCFAllocatorDefault, NULL,
                                               0, len, kCMBlockBufferAssureMemoryNowFlag,
                                               &block) != kCMBlockBufferNoErr ||
            CMBlockBufferReplaceDataBytes(bytes, block, 0, len) != kCMBlockBufferNoErr) {
            if (block) CFRelease(block);
            return FR_MAC_ALLOCATION;
        }
        CMSampleTimingInfo timing = {kCMTimeInvalid, CMTimeMake(pts, 1000000), kCMTimeInvalid};
        const size_t sizes[1] = {len};
        if (CMSampleBufferCreateReady(kCFAllocatorDefault, block, d->format, 1, 1, &timing, 1, sizes,
                                      &sample) != noErr) {
            CFRelease(block);
            return FR_MAC_ALLOCATION;
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
        if (status == kVTInvalidSessionErr) return FR_MAC_SESSION_LOST;
        if (status != noErr || d->status != noErr || !d->image ||
            (info & kVTDecodeInfo_FrameDropped))
            return FR_MAC_CODEC;
        if (present) result = [d->renderer present:d->image];
        CVPixelBufferRelease(d->image);
        d->image = NULL;
    }
    return result;
}

void fr_mac_decoder_free(void *handle) {
    if (handle) (void)(__bridge_transfer FrDecoder *)handle;
}
