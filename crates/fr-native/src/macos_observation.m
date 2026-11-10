// Read-only host observation panels. This child creates no event source,
// capture stream, clipboard access, peer approval or input authority. AppKit
// objects belong exclusively to its main thread. Rust supervises completion
// of each visibility/event turn on a separate, sleep-aware clock.
#import <AppKit/AppKit.h>
#import <CoreGraphics/CoreGraphics.h>
#include <dispatch/dispatch.h>
#include <limits.h>
#include <math.h>
#include <pthread.h>
#include <stdbool.h>
#include <stdint.h>
#include <unistd.h>

enum { FR_OI_OPENING = 0, FR_OI_SHOWN = 1, FR_OI_LOCAL_STOP = 2, FR_OI_FAILED = 3 };
// The source user-data tag on every event posted by fr-input-agent. It must
// not turn its own remote click into a local user's Stop sharing gesture.
static const int64_t FR_REMOTE_INPUT_TAG = 0x46524350;
static dispatch_source_t parent_monitor;
static BOOL occupied;
static BOOL server_absent(CGWindowID window);

int fr_oi_bind_parent(uint32_t parent) {
    if (parent <= 1 || parent > INT_MAX || getppid() != (pid_t)parent || parent_monitor ||
        getuid() == 0 || getuid() != geteuid())
        return FR_OI_FAILED;
    dispatch_source_t watch = dispatch_source_create(
        DISPATCH_SOURCE_TYPE_PROC, (uintptr_t)parent, DISPATCH_PROC_EXIT,
        dispatch_get_global_queue(QOS_CLASS_USER_INITIATED, 0));
    if (!watch) return FR_OI_FAILED;
    // There is no held input state to release. Process exit closes the exact
    // windows even if AppKit is stuck; the broker still owns and reaps us.
    dispatch_source_set_event_handler(watch, ^{ _exit(76); });
    parent_monitor = watch;
    dispatch_resume(watch);
    return getppid() == (pid_t)parent ? 0 : FR_OI_FAILED;
}

@class FrObservationPanels;
@interface FrObservationBadge : NSView
@property(nonatomic, weak) FrObservationPanels *owner;
@end

@interface FrObservationPanel : NSPanel
@property(nonatomic) uint32_t displayID;
@property(nonatomic) NSRect displayFrame;
@property(nonatomic) CGFloat displayScale;
@property(nonatomic) NSRect originalFrame;
@end
@implementation FrObservationPanel
- (BOOL)canBecomeKeyWindow { return NO; }
- (BOOL)canBecomeMainWindow { return NO; }
@end

@interface FrObservationPanels : NSObject {
  @public
    int state;
}
@property(nonatomic, strong) NSMutableArray<FrObservationPanel *> *panels;
- (void)stop:(int)reason;
- (void)lost:(NSNotification *)notification;
- (BOOL)closePanels;
@end
@implementation FrObservationPanels
- (instancetype)init {
    self = [super init];
    if (self) {
        state = FR_OI_OPENING;
        _panels = [NSMutableArray array];
        // These AppKit notifications are delivered on its main thread. Even
        // a display that disappears and returns between polls ends this owner;
        // reusing a numeric display ID cannot reopen the original panel set.
        NSNotificationCenter *center = NSNotificationCenter.defaultCenter;
        for (NSNotificationName name in @[
                 NSApplicationDidChangeScreenParametersNotification,
                 NSApplicationWillHideNotification, NSApplicationWillTerminateNotification]) {
            [center addObserver:self selector:@selector(lost:) name:name object:NSApp];
        }
    }
    return self;
}
- (void)stop:(int)reason {
    if (state == FR_OI_OPENING || state == FR_OI_SHOWN) state = reason;
}
- (void)lost:(NSNotification *)notification {
    (void)notification;
    [self stop:FR_OI_FAILED];
}
- (BOOL)closePanels {
    // Retain scalar identities across the final strong releases, then ask
    // WindowServer whether those original windows are actually gone. Failure
    // to observe cleanup cannot release this process's sole native UI slot.
    if (_panels.count > 16) return NO;
    CGWindowID windows[16] = {0};
    NSUInteger count = 0;
    BOOL closed = YES;
    for (NSPanel *panel in _panels) {
        @try {
            NSInteger window = panel.windowNumber;
            if (window > 0 && window <= UINT32_MAX) windows[count++] = (CGWindowID)window;
            else if (window > UINT32_MAX) closed = NO;
            [panel orderOut:nil];
            [panel close];
            if (panel.isVisible) closed = NO;
        } @catch (NSException *exception) {
            (void)exception;
            closed = NO;
        }
    }
    @try {
        [_panels removeAllObjects];
    } @catch (NSException *exception) {
        (void)exception;
        return NO;
    }
    for (NSUInteger i = 0; i < count; ++i) {
        if (!server_absent(windows[i])) closed = NO;
    }
    return closed;
}
- (void)dealloc {
    @try {
        [NSNotificationCenter.defaultCenter removeObserver:self];
    } @catch (NSException *exception) {
        (void)exception;
    }
    @try {
        (void)[self closePanels];
    } @catch (NSException *exception) {
        (void)exception;
    }
}
@end

@implementation FrObservationBadge
- (BOOL)acceptsFirstMouse:(NSEvent *)event {
    (void)event;
    return YES;
}
- (void)mouseDown:(NSEvent *)event {
    CGEventRef native = event.CGEvent;
    // An unbacked synthetic NSEvent is not an attributable local gesture.
    if (!native || CGEventGetIntegerValueField(native, kCGEventSourceUserData) ==
                       FR_REMOTE_INPUT_TAG ||
        CGEventGetIntegerValueField(native, kCGEventSourceUnixProcessID) == getpid())
        return;
    [self.owner stop:FR_OI_LOCAL_STOP];
}
- (void)rightMouseDown:(NSEvent *)event { [self mouseDown:event]; }
- (void)otherMouseDown:(NSEvent *)event { [self mouseDown:event]; }
- (BOOL)isOpaque { return YES; }
- (void)drawRect:(NSRect)dirty {
    (void)dirty;
    [[NSColor colorWithSRGBRed:0.08 green:0.28 blue:0.52 alpha:1.0] setFill];
    NSRectFill(self.bounds);
    NSDictionary *attributes = @{
        NSFontAttributeName : [NSFont systemFontOfSize:13 weight:NSFontWeightSemibold],
        NSForegroundColorAttributeName : NSColor.whiteColor,
    };
    NSString *label = @"Screen sharing enabled — click to stop";
    NSSize size = [label sizeWithAttributes:attributes];
    [label drawAtPoint:NSMakePoint(NSMidX(self.bounds) - size.width / 2,
                                   NSMidY(self.bounds) - size.height / 2)
       withAttributes:attributes];
}
@end

static uint32_t screen_number(NSScreen *screen) {
    id number = screen.deviceDescription[@"NSScreenNumber"];
    if (![number isKindOfClass:NSNumber.class]) return 0;
    unsigned long long value = [number unsignedLongLongValue];
    return value > 0 && value <= UINT32_MAX ? (uint32_t)value : 0;
}

static BOOL rectangle(NSRect rect) {
    return isfinite(rect.origin.x) && isfinite(rect.origin.y) && isfinite(rect.size.width) &&
           isfinite(rect.size.height) && rect.size.width > 0 && rect.size.height > 0;
}

static BOOL window_number(CFDictionaryRef info, CFStringRef key, int64_t expected) {
    CFTypeRef value = CFDictionaryGetValue(info, key);
    int64_t actual = 0;
    return value && CFGetTypeID(value) == CFNumberGetTypeID() &&
           CFNumberGetValue((CFNumberRef)value, kCFNumberSInt64Type, &actual) &&
           actual == expected;
}

static BOOL server_visible(NSPanel *panel) {
    NSInteger window = panel.windowNumber;
    if (window <= 0 || window > UINT32_MAX) return NO;
    CFArrayRef list = CGWindowListCopyWindowInfo(kCGWindowListOptionIncludingWindow,
                                                (CGWindowID)window);
    if (!list) return NO;
    BOOL visible = NO;
    if (CFArrayGetCount(list) == 1) {
        CFTypeRef entry = CFArrayGetValueAtIndex(list, 0);
        if (CFGetTypeID(entry) == CFDictionaryGetTypeID()) {
            CFDictionaryRef info = (CFDictionaryRef)entry;
            CFTypeRef onscreen = CFDictionaryGetValue(info, kCGWindowIsOnscreen);
            visible = onscreen && CFGetTypeID(onscreen) == CFBooleanGetTypeID() &&
                      CFBooleanGetValue((CFBooleanRef)onscreen) &&
                      window_number(info, kCGWindowNumber, window) &&
                      window_number(info, kCGWindowOwnerPID, getpid());
        }
    }
    CFRelease(list);
    return visible;
}

static BOOL server_absent(CGWindowID window) {
    CFArrayRef list = CGWindowListCopyWindowInfo(kCGWindowListOptionIncludingWindow, window);
    if (!list) return NO;
    BOOL absent = CFArrayGetCount(list) == 0;
    CFRelease(list);
    return absent;
}

// No monitor can silently acquire a new panel. Sharing after display-set,
// placement, size or scale changes requires a fresh original host lifetime.
static int visible(FrObservationPanels *owner) {
    if (owner->state >= FR_OI_LOCAL_STOP) return owner->state;
    NSArray<NSScreen *> *screens = NSScreen.screens;
    if (screens.count != owner.panels.count || screens.count == 0 || NSApp.isHidden)
        return FR_OI_FAILED;
    BOOL mapped = YES;
    for (FrObservationPanel *panel in owner.panels) {
        NSScreen *original = nil;
        for (NSScreen *screen in screens) {
            if (screen_number(screen) == panel.displayID) {
                if (original) return FR_OI_FAILED;
                original = screen;
            }
        }
        if (!original || !NSEqualRects(original.frame, panel.displayFrame) ||
            original.backingScaleFactor != panel.displayScale ||
            !NSEqualRects(panel.frame, panel.originalFrame) ||
            screen_number(panel.screen) != panel.displayID ||
            !NSContainsRect(original.visibleFrame, panel.frame) || panel.alphaValue != 1.0)
            return FR_OI_FAILED;
        BOOL shown = panel.isVisible && !panel.isMiniaturized && panel.isOnActiveSpace &&
                     (panel.occlusionState & NSWindowOcclusionStateVisible) &&
                     server_visible(panel);
        if (!shown) mapped = NO;
    }
    if (mapped) return FR_OI_SHOWN;
    return owner->state == FR_OI_OPENING ? FR_OI_OPENING : FR_OI_FAILED;
}

int fr_oi_open(uint32_t max_displays, void **out) {
    if (!out) return FR_OI_FAILED;
    *out = NULL;
    if (!pthread_main_np() || occupied || !parent_monitor || max_displays == 0 ||
        max_displays > 16)
        return FR_OI_FAILED;
    // Even an uncertain partial open cannot be replaced inside this child.
    // Only proven explicit cleanup releases the original native slot.
    occupied = YES;
    @autoreleasepool {
        @try {
            [NSApplication sharedApplication];
            if (!NSApp || ![NSApp setActivationPolicy:NSApplicationActivationPolicyAccessory])
                return FR_OI_FAILED;
            [NSApp finishLaunching];
            FrObservationPanels *owner = [[FrObservationPanels alloc] init];
            if (!owner || !owner.panels) return FR_OI_FAILED;
            // Register terminal display notifications before taking the
            // original inventory; a change between these steps cannot be lost.
            NSArray<NSScreen *> *screens = NSScreen.screens;
            if (screens.count == 0 || screens.count > max_displays) return FR_OI_FAILED;
            for (NSScreen *screen in screens) {
                uint32_t display = screen_number(screen);
                if (!display || !rectangle(screen.frame) || !rectangle(screen.visibleFrame) ||
                    !isfinite(screen.backingScaleFactor) || screen.backingScaleFactor <= 0)
                    return FR_OI_FAILED;
                for (FrObservationPanel *prior in owner.panels) {
                    if (prior.displayID == display) return FR_OI_FAILED;
                }
                NSSize size = NSMakeSize(360, 30);
                // Independent control and clipboard badges occupy offsets 6
                // and 40. Do not obscure their separate local stop controls.
                NSRect frame = NSMakeRect(round(NSMidX(screen.visibleFrame) - size.width / 2),
                                          round(NSMaxY(screen.visibleFrame) - size.height - 74),
                                          size.width, size.height);
                if (!NSContainsRect(screen.visibleFrame, frame)) return FR_OI_FAILED;
                // NSScreen frames above use global coordinates. A non-nil
                // screen would interpret this origin relative to that screen
                // and offset a secondary display twice (Apple's initializer
                // contract); nil keeps the primary-origin coordinate system.
                FrObservationPanel *panel = [[FrObservationPanel alloc]
                    initWithContentRect:frame
                              styleMask:NSWindowStyleMaskBorderless |
                                        NSWindowStyleMaskNonactivatingPanel
                                backing:NSBackingStoreBuffered
                                  defer:NO
                                 screen:nil];
                if (!panel) return FR_OI_FAILED;
                panel.displayID = display;
                panel.displayFrame = screen.frame;
                panel.displayScale = screen.backingScaleFactor;
                panel.originalFrame = frame;
                panel.level = NSStatusWindowLevel;
                panel.collectionBehavior = NSWindowCollectionBehaviorCanJoinAllSpaces |
                    NSWindowCollectionBehaviorStationary |
                    NSWindowCollectionBehaviorFullScreenAuxiliary |
                    NSWindowCollectionBehaviorIgnoresCycle;
                panel.opaque = YES;
                panel.alphaValue = 1.0;
                panel.hasShadow = YES;
                panel.hidesOnDeactivate = NO;
                panel.floatingPanel = YES;
                panel.releasedWhenClosed = NO;
                FrObservationBadge *badge = [[FrObservationBadge alloc]
                    initWithFrame:NSMakeRect(0, 0, size.width, size.height)];
                if (!badge) return FR_OI_FAILED;
                badge.owner = owner;
                panel.contentView = badge;
                [owner.panels addObject:panel];
                [panel orderFrontRegardless];
                [panel displayIfNeeded];
            }
            *out = (__bridge_retained void *)owner;
            return 0;
        } @catch (NSException *exception) {
            (void)exception;
            return FR_OI_FAILED;
        }
    }
}

int fr_oi_pump(void *handle, uint32_t timeout_ms) {
    if (!pthread_main_np() || !handle || timeout_ms > 20) return FR_OI_FAILED;
    @autoreleasepool {
        FrObservationPanels *owner = (__bridge FrObservationPanels *)handle;
        @try {
            if (owner->state >= FR_OI_LOCAL_STOP) return owner->state;
            NSDate *until = [NSDate dateWithTimeIntervalSinceNow:timeout_ms / 1000.0];
            // Bounded native work; floods cannot produce an unbounded queue in
            // this helper or replace completion of the real visibility check.
            for (unsigned i = 0; i < 32; ++i) {
                NSEvent *event = [NSApp nextEventMatchingMask:NSEventMaskAny
                                                   untilDate:until
                                                      inMode:NSDefaultRunLoopMode
                                                     dequeue:YES];
                if (!event) break;
                [NSApp sendEvent:event];
                if (owner->state >= FR_OI_LOCAL_STOP) return owner->state;
                until = [NSDate distantPast];
            }
            // An already queued NSEvent flood must not bypass every other
            // main-loop source. A zero-time run makes one bounded source pass;
            // the independent watchdog still covers a stuck native callback.
            SInt32 turn = CFRunLoopRunInMode(kCFRunLoopDefaultMode, 0, true);
            if (turn == kCFRunLoopRunStopped || turn == kCFRunLoopRunFinished) {
                [owner stop:FR_OI_FAILED];
                return owner->state;
            }
            [NSApp updateWindows];
            int status = visible(owner);
            if (status == FR_OI_SHOWN) owner->state = status;
            else if (status >= FR_OI_LOCAL_STOP) [owner stop:status];
            return owner->state;
        } @catch (NSException *exception) {
            (void)exception;
            [owner stop:FR_OI_FAILED];
            return owner->state;
        }
    }
}

int fr_oi_close(void *handle) {
    if (!pthread_main_np() || !handle) return FR_OI_FAILED;
    @autoreleasepool {
        @try {
            FrObservationPanels *owner = (__bridge_transfer FrObservationPanels *)handle;
            [owner stop:FR_OI_FAILED];
            BOOL closed = [owner closePanels];
            owner = nil;
            if (closed) occupied = NO;
            return closed ? 0 : FR_OI_FAILED;
        } @catch (NSException *exception) {
            (void)exception;
            return FR_OI_FAILED;
        }
    }
}
