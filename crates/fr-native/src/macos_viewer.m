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
#import <CoreVideo/CoreVideo.h>
#import <Metal/Metal.h>
#import <QuartzCore/CAMetalLayer.h>
#import <VideoToolbox/VideoToolbox.h>
#include <os/lock.h>
#include <stdint.h>
#include <string.h>
#include <math.h>

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
    FR_MAC_SHARE_ALL = 15,
};
#define FR_MAC_SHARING_HEIGHT 44.0
#define FR_MAC_SHARING_CELL 230.0
#define FR_MAC_CHANGE_WIDTH 136.0
#define FR_MAC_CLIPBOARD_WIDTH 184.0

typedef void (*FrClipboardStop)(void *, int);
typedef void (*FrClipboardRelease)(void *);

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
    return 16 + FR_MAC_SHARING_CELL * fr_sharing_cells(flags) +
           ((flags & FR_MAC_CHANGE_DISPLAY) ? FR_MAC_CHANGE_WIDTH : 0) +
           ((flags & FR_MAC_CLIPBOARD) ? FR_MAC_CLIPBOARD_WIDTH : 0);
}


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
    void *clipboardContext;
    FrClipboardStop clipboardStop;
    FrClipboardRelease clipboardRelease;
    BOOL clipboardStopped, clipboardUserOff;
    uint32_t sharingFlags, sharingActions, uploadStatus, downloadStatus;
    uint64_t uploadDone, uploadTotal, downloadDone, downloadTotal;
    BOOL uploadCancelled, downloadCancelled, layoutInProgress;
    BOOL displayChangeUsed;
    double displayChangeAt;
    FrRenderer *renderer;
    FrMacEvent queue[FR_MAC_QUEUE];
    size_t head, count;
    BOOL overflow;
    uint32_t buttons;  // pressed mouse buttons that went down inside the picture
}
- (void)push:(FrMacEvent)event;
- (void)layout;
- (void)buildSharing;
- (void)layoutSharing;
- (void)cancelUpload:(id)sender;
- (void)cancelDownload:(id)sender;
- (void)changeDisplay:(id)sender;
- (void)stopClipboard:(BOOL)userOff;
- (void)disableClipboard:(id)sender;
@end

@implementation FrMetalView
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

@implementation FrWindowOwner

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
}
- (void)cancelUpload:(id)sender {
    (void)sender;
    if (!uploadCancel.enabled || uploadCancelled) return;
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
    [self stopClipboard:NO];
    displayChangeAt = event.timestamp;
    changeDisplay.enabled = NO;
    changeDisplay.title = @"Changing Display";
}

- (void)stopClipboard:(BOOL)userOff {
    if (!(sharingFlags & FR_MAC_CLIPBOARD)) return;
    clipboardStopped = YES;
    clipboardUserOff |= userOff;
    // Fence the original Rust owner before drawing or cleanup can block.
    if (clipboardContext && clipboardStop) clipboardStop(clipboardContext, userOff ? 1 : 0);
    clipboardOff.enabled = NO;
    clipboardLabel.stringValue = @"Clipboard: Off";
}
- (void)disableClipboard:(id)sender {
    (void)sender;
    if (!clipboardOff.enabled) return;
    [self stopClipboard:YES];
}

- (void)push:(FrMacEvent)event {
    if (count == FR_MAC_QUEUE) {
        overflow = YES;
        [self stopClipboard:NO];
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
        sharing.frame = NSMakeRect(0, 0, NSWidth(bounds), FR_MAC_SHARING_HEIGHT);
        view.frame = NSMakeRect(0, FR_MAC_SHARING_HEIGHT, NSWidth(bounds),
                                fmax(1, NSHeight(bounds) - FR_MAC_SHARING_HEIGHT));
        [self layoutSharing];
    }
    double scale = window ? window.backingScaleFactor : 1;
    [renderer resizeTo:view.bounds.size scale:scale];
    layoutInProgress = NO;
}
- (BOOL)windowShouldClose:(NSWindow *)sender {
    (void)sender;
    [self stopClipboard:NO];
    FrMacEvent e = {0};
    e.kind = FR_MAC_CLOSE;
    [self push:e];
    return NO;  // Rust ends the session, then frees the window.
}
- (void)windowDidResignKey:(NSNotification *)note {
    (void)note;
    [self stopClipboard:NO];
    buttons = 0;  // the session releases everything it holds remotely
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
    (void)note;
    [self stopClipboard:NO];
}
- (void)windowDidChangeOcclusionState:(NSNotification *)note {
    (void)note;
    if (!window.isVisible || window.isMiniaturized || !window.isOnActiveSpace ||
        !(window.occlusionState & NSWindowOcclusionStateVisible)) [self stopClipboard:NO];
}

// Returns YES when AppKit should still receive the event.
- (BOOL)translate:(NSEvent *)event {
    if (event.window != window) return YES;
    FrMacEvent e = {0};
    e.modifiers = event.modifierFlags;
    e.timestamp = event.timestamp;
    switch (event.type) {
        case NSEventTypeKeyDown:
        case NSEventTypeKeyUp:
            if (!window.isKeyWindow) return NO;
            e.kind = event.type == NSEventTypeKeyDown ? FR_MAC_KEY_DOWN : FR_MAC_KEY_UP;
            e.code = event.keyCode;
            e.flags = event.type == NSEventTypeKeyDown && event.isARepeat ? 1 : 0;
            [self push:e];
            return NO;
        case NSEventTypeFlagsChanged:
            if (!window.isKeyWindow) return NO;
            e.kind = FR_MAC_MODIFIERS;
            e.code = event.keyCode;
            [self push:e];
            return NO;
        default:
            break;
    }
    NSPoint p = [view convertPoint:event.locationInWindow fromView:nil];
    BOOL inside = NO;
    BOOL inView = NSPointInRect(p, view.bounds);
    [renderer normalize:p x:&e.x y:&e.y inside:&inside];
    e.flags = inside ? 1 : 0;
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

// NSEvent timestamps and systemUptime use the same monotonic boot-time domain.
// Return the current value separately so a queued key retains its original age.
double fr_mac_event_time(void) {
    return NSProcessInfo.processInfo.systemUptime;
}

int fr_mac_window_new(uint32_t width, uint32_t height, uint32_t point_width,
                      uint32_t point_height, uint32_t sharing_flags, const char *title,
                      size_t title_len, void **out) {
    if (!out || width < 16 || height < 16 || point_width < 1 || point_height < 1 ||
        (sharing_flags & ~FR_MAC_SHARE_ALL) || (title_len && !title))
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
        // Start at the remote display's logical size in points, so a 2x
        // remote lands 1:1 on a 2x screen and a 1x remote keeps its natural
        // size. Only a picture that does not fit the visible screen (below
        // the title bar) is shrunk, with a margin and the aspect ratio kept:
        // any shrink resamples, so a fitting picture keeps its exact size.
        double chromeHeight = sharing_flags ? FR_MAC_SHARING_HEIGHT : 0;
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
    if (enabled && owner->displayChangeUsed && owner->displayChangeAt == 0) {
        owner->displayChangeUsed = NO;
        owner->changeDisplay.title = @"Change Display";
    }
    BOOL allowed = enabled && !owner->displayChangeUsed;
    if (owner->changeDisplay.enabled != allowed) owner->changeDisplay.enabled = allowed;
    return FR_MAC_OK;
}

int fr_mac_window_take_display_change(void *handle, double *captured) {
    if (!handle || !captured) return FR_MAC_INVALID;
    if (![NSThread isMainThread]) return FR_MAC_THREAD;
    FrWindowOwner *owner = (__bridge FrWindowOwner *)handle;
    *captured = owner->displayChangeAt;
    owner->displayChangeAt = 0;
    return FR_MAC_OK;
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
    @autoreleasepool {
        NSDate *until = timeout > 0 ? [NSDate dateWithTimeIntervalSinceNow:timeout]
                                    : [NSDate distantPast];
        // A display change must inspect the full bounded batch so a queued
        // close, escape or focus loss can veto reopening. File-only cancel
        // still reaches Rust immediately when no display choice is pending.
        while ((!owner->sharingActions || owner->displayChangeAt > 0) && owner->count < FR_MAC_QUEUE &&
               dequeued < FR_MAC_PUMP_EVENTS) {
            NSEvent *event = [NSApp nextEventMatchingMask:NSEventMaskAny
                                                untilDate:until
                                                   inMode:NSDefaultRunLoopMode
                                                  dequeue:YES];
            if (!event) break;
            // Count every AppKit event, including ignored events and events
            // for other windows: all of them consume this owner's pump turn.
            dequeued++;
            if ([owner translate:event]) [NSApp sendEvent:event];
            until = [NSDate distantPast];
        }
        // A local cancellation must reach Rust before any further UI drawing.
        if (!owner->sharingActions) [NSApp updateWindows];
    }
    // At either ceiling, a queued close, focus change or emergency release
    // may remain uninspected. Fail closed before granting or exporting input.
    // An exactly-full batch with an empty AppKit queue is conservatively
    // treated as incomplete too; checking that would require another poll.
    if (owner->count == FR_MAC_QUEUE || dequeued == FR_MAC_PUMP_EVENTS) {
        owner->overflow = YES;
        [owner stopClipboard:NO];
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
    *geometry_current = r->submittedPicture && r->submittedRevision == r->geometry.revision;
    os_unfair_lock_unlock(&r->geometryLock);
    *width = f.size.width >= 1 ? (uint32_t)f.size.width : 0;
    *height = f.size.height >= 1 ? (uint32_t)f.size.height : 0;
    NSWindow *w = owner->window;
    *key = w.isKeyWindow ? 1 : 0;
    *visible = w.isVisible && !w.isMiniaturized &&
                       (w.occlusionState & NSWindowOcclusionStateVisible)
                   ? 1
                   : 0;
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
