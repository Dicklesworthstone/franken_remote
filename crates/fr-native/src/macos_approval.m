// One original local consent prompt. AppKit owns the main thread; safe Rust
// checks bounded scalar event evidence and the original sleep-aware deadline.
// No input posting, capture, clipboard or peer/admission capability lives here.
#import <AppKit/AppKit.h>
#import <CoreGraphics/CoreGraphics.h>
#include <math.h>
#include <pthread.h>
#include <stdint.h>
#include <unistd.h>

typedef struct {
    uint32_t kind;
    uint32_t reserved;
    uint64_t number;
    uint64_t at_ns;
    int64_t source_pid;
    int64_t source_state;
    int64_t user_data;
} FrApprovalEvent;

typedef struct {
    uint32_t state;
    uint32_t count;
    uint64_t ready_ns;
    uint64_t sampled_ns;
    FrApprovalEvent events[2];
} FrApprovalTurn;

_Static_assert(sizeof(FrApprovalEvent) == 48, "fixed native approval event");
_Static_assert(sizeof(FrApprovalTurn) == 120, "fixed native approval snapshot");

enum { FR_AP_OPENING = 0, FR_AP_SHOWN = 1, FR_AP_DENIED = 2, FR_AP_FAILED = 3 };
static BOOL occupied;

static uint64_t uptime_ns(NSTimeInterval seconds) {
    if (!isfinite(seconds) || seconds <= 0 || seconds >= (double)UINT64_MAX / 1e9) return 0;
    return (uint64_t)(seconds * 1e9);
}

@class FrApprovalOwner;
@interface FrApprovalView : NSView
@property(nonatomic, weak) FrApprovalOwner *owner;
@property(nonatomic) BOOL drawn;
@end

@interface FrApprovalOwner : NSObject <NSWindowDelegate>
@property(nonatomic, strong) NSWindow *window;
@property(nonatomic, strong) FrApprovalView *view;
@property(nonatomic, strong) id<NSObject> activity;
@property(nonatomic, copy) NSString *details;
@property(nonatomic) BOOL control;
@property(nonatomic) uint32_t state;
@property(nonatomic) uint64_t ready;
@property(nonatomic) FrApprovalTurn turn;
- (BOOL)visible;
- (void)deny:(id)sender;
- (void)lost:(NSNotification *)notification;
- (void)record:(NSEvent *)event kind:(uint32_t)kind;
- (BOOL)closeResources;
@end

static NSRect allow_rect(NSRect bounds) {
    return NSMakeRect(NSMaxX(bounds) - 178, 24, 150, 42);
}
static NSRect deny_rect(NSRect bounds) {
    return NSMakeRect(NSMaxX(bounds) - 340, 24, 150, 42);
}
static NSRect details_rect(void) {
    return NSMakeRect(28, 100, 624, 264);
}
static NSDictionary *details_attributes(void) {
    NSMutableParagraphStyle *paragraph = [NSMutableParagraphStyle new];
    paragraph.lineBreakMode = NSLineBreakByCharWrapping;
    return @{
        NSFontAttributeName : [NSFont monospacedSystemFontOfSize:13 weight:NSFontWeightRegular],
        NSForegroundColorAttributeName : NSColor.labelColor,
        NSParagraphStyleAttributeName : paragraph,
    };
}
static BOOL details_fit(NSString *details) {
    if (!details.length) return NO;
    NSRect area = details_rect();
    NSRect needed = [details boundingRectWithSize:NSMakeSize(area.size.width, CGFLOAT_MAX)
        options:NSStringDrawingUsesLineFragmentOrigin | NSStringDrawingUsesFontLeading
        attributes:details_attributes()];
    return isfinite(needed.size.width) && isfinite(needed.size.height) &&
           needed.size.width <= area.size.width && needed.size.height <= area.size.height;
}

@implementation FrApprovalView
- (BOOL)acceptsFirstMouse:(NSEvent *)event {
    (void)event;
    // Activating a previously unfocused window is not an Allow gesture.
    return NO;
}
- (BOOL)acceptsFirstResponder { return YES; }
- (void)drawRect:(NSRect)dirty {
    (void)dirty;
    [NSColor.windowBackgroundColor setFill];
    NSRectFill(self.bounds);
    NSDictionary *heading = @{
        NSFontAttributeName : [NSFont systemFontOfSize:20 weight:NSFontWeightSemibold],
        NSForegroundColorAttributeName : NSColor.labelColor,
    };
    NSString *title = self.owner.control ? @"Allow remote control?" : @"Allow desktop viewing?";
    [title drawInRect:NSMakeRect(28, 376, 624, 32) withAttributes:heading];
    // All details come from the original private context, never an event or
    // a client-authored request label. Refuse instead of clipping disclosure.
    if (!details_fit(self.owner.details)) {
        self.owner.state = FR_AP_FAILED;
        return;
    }
    [self.owner.details drawWithRect:details_rect()
        options:NSStringDrawingUsesLineFragmentOrigin | NSStringDrawingUsesFontLeading
        attributes:details_attributes()];
    for (NSUInteger i = 0; i < 2; ++i) {
        NSRect rect = i ? allow_rect(self.bounds) : deny_rect(self.bounds);
        [(i ? NSColor.controlAccentColor : NSColor.controlColor) setFill];
        [[NSBezierPath bezierPathWithRoundedRect:rect xRadius:7 yRadius:7] fill];
        NSDictionary *label = @{
            NSFontAttributeName : [NSFont systemFontOfSize:14 weight:NSFontWeightSemibold],
            NSForegroundColorAttributeName : i ? NSColor.whiteColor : NSColor.labelColor,
        };
        NSString *text = i ? @"Allow" : @"Deny";
        NSSize size = [text sizeWithAttributes:label];
        [text drawAtPoint:NSMakePoint(NSMidX(rect) - size.width / 2,
                                     NSMidY(rect) - size.height / 2)
           withAttributes:label];
    }
    self.drawn = YES;
}
- (void)mouseDown:(NSEvent *)event {
    NSPoint point = [self convertPoint:event.locationInWindow fromView:nil];
    if (NSPointInRect(point, deny_rect(self.bounds))) {
        [self.owner deny:nil];
    } else {
        [self.owner record:event kind:NSPointInRect(point, allow_rect(self.bounds)) ? 1 : 3];
    }
}
- (void)mouseUp:(NSEvent *)event {
    NSPoint point = [self convertPoint:event.locationInWindow fromView:nil];
    [self.owner record:event kind:NSPointInRect(point, allow_rect(self.bounds)) ? 2 : 3];
}
- (void)mouseDragged:(NSEvent *)event {
    NSPoint point = [self convertPoint:event.locationInWindow fromView:nil];
    if (!NSPointInRect(point, allow_rect(self.bounds))) [self.owner record:event kind:3];
}
- (void)keyDown:(NSEvent *)event {
    // Keyboard shortcuts and accessibility actions can deny, never approve.
    if (event.keyCode == 53) [self.owner deny:nil];
}
- (BOOL)isAccessibilityElement { return YES; }
- (NSAccessibilityRole)accessibilityRole { return NSAccessibilityGroupRole; }
- (NSString *)accessibilityLabel {
    NSString *title = self.owner.control ? @"Allow remote control?" : @"Allow desktop viewing?";
    return [NSString stringWithFormat:@"%@\n%@\nClick Allow to accept, or press Escape to deny.",
        title, self.owner.details];
}
- (BOOL)accessibilityPerformCancel {
    [self.owner deny:nil];
    return YES;
}
- (BOOL)accessibilityPerformPress { return NO; }
@end

@implementation FrApprovalOwner
- (BOOL)visible {
    NSWindow *window = self.window;
    NSScreen *screen = window.screen;
    return window && screen && NSContainsRect(screen.visibleFrame, window.frame) &&
           window.contentView == self.view && self.view.window == window &&
           NSEqualRects(self.view.bounds, NSMakeRect(0, 0, 680, 440)) &&
           self.view.drawn && window.isVisible && window.isOnActiveSpace &&
           !window.isMiniaturized && !NSApp.isHidden && NSApp.isActive &&
           window.isKeyWindow && (window.occlusionState & NSWindowOcclusionStateVisible);
}
- (void)deny:(id)sender {
    (void)sender;
    if (self.state < FR_AP_DENIED) self.state = FR_AP_DENIED;
}
- (void)lost:(NSNotification *)notification {
    (void)notification;
    if (self.state < FR_AP_DENIED) self.state = FR_AP_FAILED;
}
- (BOOL)windowShouldClose:(NSWindow *)sender {
    (void)sender;
    [self deny:nil];
    return NO; // The original Rust owner performs the actual destruction.
}
- (void)windowDidResignKey:(NSNotification *)notification {
    if (self.ready) [self lost:notification];
}
- (void)record:(NSEvent *)event kind:(uint32_t)kind {
    if (self.state != FR_AP_SHOWN || !self.ready || ![self visible] ||
        event.window != self.window || event.windowNumber != self.window.windowNumber ||
        event.buttonNumber != 0) return;
    FrApprovalTurn turn = self.turn;
    if (turn.count == 2) {
        self.state = FR_AP_FAILED;
        return;
    }
    CGEventRef native = event.CGEvent;
    if (!native) {
        self.state = FR_AP_FAILED;
        return;
    }
    FrApprovalEvent *out = &turn.events[turn.count++];
    out->kind = kind;
    out->number = (uint64_t)CGEventGetIntegerValueField(native, kCGMouseEventNumber);
    out->at_ns = uptime_ns(event.timestamp);
    out->source_pid = CGEventGetIntegerValueField(native, kCGEventSourceUnixProcessID);
    out->source_state = CGEventGetIntegerValueField(native, kCGEventSourceStateID);
    out->user_data = CGEventGetIntegerValueField(native, kCGEventSourceUserData);
    self.turn = turn;
}
- (BOOL)closeResources {
    BOOL hidden = YES;
    @try {
        [NSNotificationCenter.defaultCenter removeObserver:self];
        [NSWorkspace.sharedWorkspace.notificationCenter removeObserver:self];
        self.window.delegate = nil;
        [self.window orderOut:nil];
        [self.window close];
        hidden = !self.window.isVisible;
        self.view.owner = nil;
        self.window.contentView = nil;
        self.view = nil;
        self.window = nil;
    } @catch (NSException *exception) {
        (void)exception;
        hidden = NO;
    }
    @try {
        if (self.activity) [NSProcessInfo.processInfo endActivity:self.activity];
        self.activity = nil;
    } @catch (NSException *exception) {
        (void)exception;
        hidden = NO;
    }
    return hidden;
}
@end

int fr_ap_open(uint32_t control, const uint8_t *details, size_t details_len, void **out) {
    if (!pthread_main_np() || occupied || !out || control > 1 || !details ||
        !details_len || details_len > 1024 ||
        getuid() == 0 || getuid() != geteuid()) return -1;
    BOOL visible_text = NO;
    for (size_t i = 0; i < details_len; ++i) {
        uint8_t byte = details[i];
        if ((byte < 0x20 || byte > 0x7e) && byte != '\n') return -1;
        if (byte > 0x20 && byte <= 0x7e) visible_text = YES;
    }
    if (!visible_text) return -1;
    *out = NULL;
    occupied = YES;
    @autoreleasepool {
        FrApprovalOwner *owner = [FrApprovalOwner new];
        int result = -1;
        @try {
          do {
            // Copy before creating any UI. No borrowed Rust storage escapes,
            // and an unrepresentable or oversized disclosure never maps a UI.
            owner.details = [[NSString alloc] initWithBytes:details length:details_len
                encoding:NSASCIIStringEncoding];
            [NSApplication sharedApplication];
            if (!details_fit(owner.details)) break;
            [NSApp setActivationPolicy:NSApplicationActivationPolicyAccessory];
            [NSApp finishLaunching];
            owner.control = control != 0;
            owner.activity = [NSProcessInfo.processInfo
                beginActivityWithOptions:NSActivityUserInitiated | NSActivityLatencyCritical
                                  reason:@"Waiting for a local desktop-sharing decision"];
            NSScreen *screen = NSScreen.mainScreen;
            if (!screen || !owner.activity) break;
            NSRect available = screen.visibleFrame;
            NSRect frame = NSMakeRect(round(NSMidX(available) - 340),
                                      round(NSMidY(available) - 220), 680, 440);
            if (!NSContainsRect(available, frame)) break;
            owner.window = [[NSWindow alloc]
                initWithContentRect:frame
                          styleMask:NSWindowStyleMaskTitled | NSWindowStyleMaskClosable
                            backing:NSBackingStoreBuffered defer:NO];
            owner.view = [[FrApprovalView alloc] initWithFrame:NSMakeRect(0, 0, 680, 440)];
            if (!owner.window || !owner.view ||
                !NSContainsRect(available, owner.window.frame)) break;
            owner.view.owner = owner;
            owner.window.contentView = owner.view;
            owner.window.delegate = owner;
            owner.window.title = @"FrankenRemote";
            owner.window.releasedWhenClosed = NO;
            owner.window.level = NSFloatingWindowLevel;
            NSNotificationCenter *center = NSNotificationCenter.defaultCenter;
            for (NSNotificationName name in @[ NSApplicationWillHideNotification,
                                               NSApplicationWillTerminateNotification ]) {
                [center addObserver:owner selector:@selector(lost:) name:name object:NSApp];
            }
            NSNotificationCenter *workspace = NSWorkspace.sharedWorkspace.notificationCenter;
            for (NSNotificationName name in @[ NSWorkspaceSessionDidResignActiveNotification,
                                               NSWorkspaceWillSleepNotification,
                                               NSWorkspaceWillPowerOffNotification,
                                               NSWorkspaceScreensDidSleepNotification ]) {
                [workspace addObserver:owner selector:@selector(lost:) name:name object:nil];
            }
            [owner.window makeKeyAndOrderFront:nil];
            [owner.window makeFirstResponder:owner.view];
            [NSApp activateIgnoringOtherApps:YES];
            *out = (__bridge_retained void *)owner;
            result = 0;
          } while (0);
        } @catch (NSException *exception) {
            (void)exception;
        }
        // Opening may fail after creating a window, registering observers or
        // acquiring an activity. Unwind those exact partial resources here;
        // the independent worker/parent still owns the final process reap.
        if (result) [owner closeResources];
        return result;
    }
}

int fr_ap_pump(void *raw, uint32_t wait_ms, FrApprovalTurn *out) {
    if (!pthread_main_np() || !raw || !out || wait_ms > 20) return -1;
    @autoreleasepool {
        @try {
            FrApprovalOwner *owner = (__bridge FrApprovalOwner *)raw;
            owner.turn = (FrApprovalTurn){0};
            NSDate *until = [NSDate dateWithTimeIntervalSinceNow:wait_ms / 1000.0];
            unsigned handled = 0;
            for (; handled < 32; ++handled) {
                NSEvent *event = [NSApp nextEventMatchingMask:NSEventMaskAny
                    untilDate:until inMode:NSDefaultRunLoopMode dequeue:YES];
                if (!event) break;
                [NSApp sendEvent:event];
                until = NSDate.distantPast;
            }
            if (handled == 32 && [NSApp nextEventMatchingMask:NSEventMaskAny
                untilDate:NSDate.distantPast inMode:NSDefaultRunLoopMode dequeue:NO]) {
                owner.state = FR_AP_FAILED;
            }
            [NSApp updateWindows];
            [owner.view displayIfNeeded];
            if (owner.state < FR_AP_DENIED) {
                if ([owner visible]) {
                    if (!owner.ready) owner.ready = uptime_ns(NSProcessInfo.processInfo.systemUptime);
                    owner.state = owner.ready ? FR_AP_SHOWN : FR_AP_FAILED;
                } else if (owner.ready) {
                    owner.state = FR_AP_FAILED;
                }
            }
            FrApprovalTurn turn = owner.turn;
            turn.state = owner.state;
            turn.ready_ns = owner.ready;
            turn.sampled_ns = uptime_ns(NSProcessInfo.processInfo.systemUptime);
            *out = turn;
            return turn.sampled_ns ? 0 : -1;
        } @catch (NSException *exception) {
            (void)exception;
            return -1;
        }
    }
}

int fr_ap_close(void *raw) {
    if (!pthread_main_np() || !raw) return -1;
    @autoreleasepool {
        @try {
            FrApprovalOwner *owner = (__bridge_transfer FrApprovalOwner *)raw;
            return [owner closeResources] ? 0 : -1;
        } @catch (NSException *exception) {
            (void)exception;
            return -1;
        }
    }
}
