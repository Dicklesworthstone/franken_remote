// macOS viewer glue (fr-5a4w): one AppKit window, VideoToolbox HEVC decode
// into IOSurface-backed NV12 pixel buffers, and a Metal pass that converts
// BT.709 YCbCr and aspect-fits the picture into a CAMetalLayer. Pixels never
// cross the CPU. Rust admits every parameter set and access unit before any
// byte reaches VideoToolbox; this file only binds the OS frameworks.
//
// Threads: window creation, event polling and freeing run on the main thread.
// Decoder creation, decode and present run on ONE caller thread at a time
// (the media thread). The renderer is shared and takes its own lock.
#import <AppKit/AppKit.h>
#import <CoreMedia/CoreMedia.h>
#import <CoreVideo/CoreVideo.h>
#import <Metal/Metal.h>
#import <QuartzCore/CAMetalLayer.h>
#import <VideoToolbox/VideoToolbox.h>
#include <os/lock.h>
#include <stdint.h>
#include <string.h>

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
} FrMacEvent;

#define FR_MAC_QUEUE 1024

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
     "    constant float4 &range [[buffer(0)]]) {\n"
     "  float y = (luma.sample(s, in.uv).r - range.x) * range.y;\n"
     "  float2 c = (chroma.sample(s, in.uv).rg - range.z) * range.w;\n"
     "  float3 rgb = float3(y + 1.5748 * c.y, y - 0.1873 * c.x - 0.4681 * c.y, y + 1.8556 * c.x);\n"
     "  return float4(saturate(rgb), 1.0);\n"
     "}\n";

// ---------------------------------------------------------------- renderer

@interface FrRenderer : NSObject {
  @public
    os_unfair_lock lock;
    CAMetalLayer *layer;
    id<MTLDevice> device;
    id<MTLCommandQueue> queue;
    id<MTLRenderPipelineState> pipeline;
    id<MTLSamplerState> sampler;
    CVMetalTextureCacheRef cache;
    CVPixelBufferRef last;   // retained: redrawn on resize or expose
    uint32_t sourceWidth;    // visible picture size
    uint32_t sourceHeight;
    BOOL fullRange;
    CGSize drawable;         // pixels
    double scale;            // pixels per point
    CGRect fit;              // drawable pixels, top-left origin
    uint64_t presented;
}
@end

@implementation FrRenderer
- (void)dealloc {
    if (last) CVPixelBufferRelease(last);
    if (cache) CFRelease(cache);
}

// Caller holds the lock.
- (void)refit {
    double sw = sourceWidth, sh = sourceHeight;
    double dw = drawable.width, dh = drawable.height;
    if (sw <= 0 || sh <= 0 || dw <= 0 || dh <= 0) {
        fit = CGRectZero;
        return;
    }
    double s = fmin(dw / sw, dh / sh);
    double w = round(sw * s), h = round(sh * s);
    fit = CGRectMake(floor((dw - w) / 2), floor((dh - h) / 2), w, h);
}

// Caller holds the lock. Draws `last` (or black) into a fresh drawable.
- (int)drawLocked {
    if (drawable.width < 1 || drawable.height < 1) return FR_MAC_OK;
    id<CAMetalDrawable> target = [layer nextDrawable];
    if (!target) return FR_MAC_UNAVAILABLE;
    MTLRenderPassDescriptor *pass = [MTLRenderPassDescriptor renderPassDescriptor];
    pass.colorAttachments[0].texture = target.texture;
    pass.colorAttachments[0].loadAction = MTLLoadActionClear;
    pass.colorAttachments[0].clearColor = MTLClearColorMake(0, 0, 0, 1);
    pass.colorAttachments[0].storeAction = MTLStoreActionStore;
    id<MTLCommandBuffer> commands = [queue commandBuffer];
    id<MTLRenderCommandEncoder> encoder = [commands renderCommandEncoderWithDescriptor:pass];
    CVMetalTextureRef luma = NULL, chroma = NULL;
    if (last && fit.size.width >= 1 && fit.size.height >= 1) {
        size_t w0 = CVPixelBufferGetWidthOfPlane(last, 0), h0 = CVPixelBufferGetHeightOfPlane(last, 0);
        size_t w1 = CVPixelBufferGetWidthOfPlane(last, 1), h1 = CVPixelBufferGetHeightOfPlane(last, 1);
        if (CVMetalTextureCacheCreateTextureFromImage(NULL, cache, last, NULL, MTLPixelFormatR8Unorm,
                                                      w0, h0, 0, &luma) == kCVReturnSuccess &&
            CVMetalTextureCacheCreateTextureFromImage(NULL, cache, last, NULL, MTLPixelFormatRG8Unorm,
                                                      w1, h1, 1, &chroma) == kCVReturnSuccess) {
            // Sample only the visible picture if the decoder kept coded padding.
            float extent[2] = {(float)fmin(1.0, (double)sourceWidth / (double)w0),
                               (float)fmin(1.0, (double)sourceHeight / (double)h0)};
            float range[4] = {fullRange ? 0.0f : 16.0f / 255.0f, fullRange ? 1.0f : 255.0f / 219.0f,
                              128.0f / 255.0f, fullRange ? 1.0f : 255.0f / 224.0f};
            MTLViewport viewport = {fit.origin.x, fit.origin.y, fit.size.width, fit.size.height, 0, 1};
            [encoder setViewport:viewport];
            [encoder setRenderPipelineState:pipeline];
            [encoder setVertexBytes:extent length:sizeof(extent) atIndex:0];
            [encoder setFragmentTexture:CVMetalTextureGetTexture(luma) atIndex:0];
            [encoder setFragmentTexture:CVMetalTextureGetTexture(chroma) atIndex:1];
            [encoder setFragmentSamplerState:sampler atIndex:0];
            [encoder setFragmentBytes:range length:sizeof(range) atIndex:0];
            [encoder drawPrimitives:MTLPrimitiveTypeTriangle vertexStart:0 vertexCount:3];
        }
    }
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
    return FR_MAC_OK;
}

- (int)present:(CVPixelBufferRef)picture {
    os_unfair_lock_lock(&lock);
    if (last) CVPixelBufferRelease(last);
    last = CVPixelBufferRetain(picture);
    int status = [self drawLocked];
    if (status == FR_MAC_OK) presented++;
    os_unfair_lock_unlock(&lock);
    return status;
}

// Main thread: the view's pixel size or scale changed.
- (void)resizeTo:(CGSize)points scale:(double)pixelsPerPoint {
    os_unfair_lock_lock(&lock);
    scale = pixelsPerPoint > 0 ? pixelsPerPoint : 1;
    drawable = CGSizeMake(fmax(1, round(points.width * scale)), fmax(1, round(points.height * scale)));
    layer.contentsScale = scale;
    layer.drawableSize = drawable;
    [self refit];
    [self drawLocked];
    os_unfair_lock_unlock(&lock);
}

// Main thread: normalize a view point (top-left origin) against the last fit.
- (void)normalize:(NSPoint)point x:(double *)x y:(double *)y inside:(BOOL *)inside {
    os_unfair_lock_lock(&lock);
    CGRect f = fit;
    double s = scale;
    os_unfair_lock_unlock(&lock);
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
    MTLSamplerDescriptor *sd = [MTLSamplerDescriptor new];
    sd.minFilter = MTLSamplerMinMagFilterLinear;
    sd.magFilter = MTLSamplerMinMagFilterLinear;
    sd.sAddressMode = MTLSamplerAddressModeClampToEdge;
    sd.tAddressMode = MTLSamplerAddressModeClampToEdge;
    r->sampler = [r->device newSamplerStateWithDescriptor:sd];
    if (!r->pipeline || !r->sampler ||
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
    r->sourceWidth = width;
    r->sourceHeight = height;
    r->scale = 1;
    *status = FR_MAC_OK;
    return r;
}

// ------------------------------------------------------------------ window

@class FrWindowOwner;

@interface FrMetalView : NSView
@property(nonatomic, weak) FrWindowOwner *owner;
@end

@interface FrWindowOwner : NSObject <NSWindowDelegate> {
  @public
    NSWindow *window;
    FrMetalView *view;
    FrRenderer *renderer;
    FrMacEvent queue[FR_MAC_QUEUE];
    size_t head, count;
    BOOL overflow;
    uint32_t buttons;  // pressed mouse buttons that went down inside the picture
}
- (void)push:(FrMacEvent)event;
- (void)layout;
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

@implementation FrWindowOwner
- (void)push:(FrMacEvent)event {
    if (count == FR_MAC_QUEUE) {
        overflow = YES;
        return;
    }
    queue[(head + count) % FR_MAC_QUEUE] = event;
    count++;
}
- (void)layout {
    if (!view) return;
    NSRect bounds = view.bounds;
    double scale = window ? window.backingScaleFactor : 1;
    [renderer resizeTo:bounds.size scale:scale];
}
- (BOOL)windowShouldClose:(NSWindow *)sender {
    (void)sender;
    FrMacEvent e = {0};
    e.kind = FR_MAC_CLOSE;
    [self push:e];
    return NO;  // Rust ends the session, then frees the window.
}
- (void)windowDidResignKey:(NSNotification *)note {
    (void)note;
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

// Returns YES when AppKit should still receive the event.
- (BOOL)translate:(NSEvent *)event {
    if (event.window != window) return YES;
    FrMacEvent e = {0};
    e.modifiers = event.modifierFlags;
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

int fr_mac_window_new(uint32_t width, uint32_t height, const char *title, size_t title_len,
                      void **out) {
    if (!out || width < 16 || height < 16 || (title_len && !title)) return FR_MAC_INVALID;
    *out = NULL;
    if (![NSThread isMainThread]) return FR_MAC_THREAD;
    @autoreleasepool {
        int status = FR_MAC_OK;
        FrRenderer *renderer = renderer_new(width, height, &status);
        if (!renderer) return status;
        FrWindowOwner *owner = [FrWindowOwner new];
        owner->renderer = renderer;
        // Start at the remote size in points (as Screen Sharing does), shrunk
        // to fit the visible screen with the picture's aspect ratio kept.
        NSRect visible = NSScreen.mainScreen ? NSScreen.mainScreen.visibleFrame
                                             : NSMakeRect(0, 0, 1280, 800);
        double s = fmin(1.0, fmin(visible.size.width * 0.9 / width,
                                  (visible.size.height * 0.9 - 28) / height));
        NSRect frame = NSMakeRect(0, 0, floor(width * s), floor(height * s));
        NSWindowStyleMask style = NSWindowStyleMaskTitled | NSWindowStyleMaskClosable |
                                  NSWindowStyleMaskMiniaturizable | NSWindowStyleMaskResizable;
        NSWindow *window = [[NSWindow alloc] initWithContentRect:frame
                                                       styleMask:style
                                                         backing:NSBackingStoreBuffered
                                                           defer:NO];
        window.releasedWhenClosed = NO;
        window.acceptsMouseMovedEvents = YES;
        window.delegate = owner;
        window.contentAspectRatio = NSMakeSize(width, height);
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
        window.contentView = view;
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

// Waits up to `timeout` seconds for the first event, then drains without
// waiting. Returns the number of translated events copied out.
size_t fr_mac_window_poll(void *handle, double timeout, FrMacEvent *out, size_t capacity,
                          int *overflow) {
    if (!handle || ![NSThread isMainThread]) return 0;
    FrWindowOwner *owner = (__bridge FrWindowOwner *)handle;
    @autoreleasepool {
        NSDate *until = timeout > 0 ? [NSDate dateWithTimeIntervalSinceNow:timeout]
                                    : [NSDate distantPast];
        while (owner->count < FR_MAC_QUEUE) {
            NSEvent *event = [NSApp nextEventMatchingMask:NSEventMaskAny
                                                untilDate:until
                                                   inMode:NSDefaultRunLoopMode
                                                  dequeue:YES];
            if (!event) break;
            if ([owner translate:event]) [NSApp sendEvent:event];
            until = [NSDate distantPast];
        }
        [NSApp updateWindows];
    }
    size_t n = 0;
    while (n < capacity && owner->count) {
        out[n++] = owner->queue[owner->head];
        owner->head = (owner->head + 1) % FR_MAC_QUEUE;
        owner->count--;
    }
    if (overflow) {
        *overflow = owner->overflow ? 1 : 0;
        owner->overflow = NO;
    }
    return n;
}

intptr_t fr_mac_window_number(void *handle) {
    if (!handle) return 0;
    FrWindowOwner *owner = (__bridge FrWindowOwner *)handle;
    return owner->window.windowNumber;
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

uint64_t fr_mac_renderer_presented(void *handle) {
    if (!handle) return 0;
    FrRenderer *r = (__bridge FrRenderer *)handle;
    os_unfair_lock_lock(&r->lock);
    uint64_t n = r->presented;
    os_unfair_lock_unlock(&r->lock);
    return n;
}

void fr_mac_renderer_release(void *handle) {
    if (handle) (void)(__bridge_transfer FrRenderer *)handle;
}

void fr_mac_window_free(void *handle) {
    if (!handle) return;
    @autoreleasepool {
        FrWindowOwner *owner = (__bridge_transfer FrWindowOwner *)handle;
        owner->window.delegate = nil;
        [owner->window orderOut:nil];
        [owner->window close];
        owner->view.owner = nil;
        owner->window = nil;
        owner->view = nil;
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
        d->renderer->sourceWidth = width;
        d->renderer->sourceHeight = height;
        d->renderer->fullRange = full_range ? YES : NO;
        [d->renderer refit];
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

// Decodes one length-prefixed access unit synchronously and presents it.
int fr_mac_decoder_decode(void *handle, const uint8_t *bytes, size_t len, int64_t pts) {
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
        result = [d->renderer present:d->image];
        CVPixelBufferRelease(d->image);
        d->image = NULL;
    }
    return result;
}

void fr_mac_decoder_free(void *handle) {
    if (handle) (void)(__bridge_transfer FrDecoder *)handle;
}
