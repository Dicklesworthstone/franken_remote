// Read-only host observation indicator: one menu bar status item whose
// popover holds the local Stop sharing control. This child creates no event
// source, capture stream, clipboard access, peer approval or input authority.
// AppKit objects belong exclusively to its main thread. Rust supervises
// completion of each visibility/event turn on a separate, sleep-aware clock.
#import <AppKit/AppKit.h>
#import <CoreGraphics/CoreGraphics.h>
#include <dispatch/dispatch.h>
#include <limits.h>
#include <math.h>
#include <pthread.h>
#include <stdbool.h>
#include <stdatomic.h>
#include <stdint.h>
#include <unistd.h>

enum { FR_OI_OPENING = 0, FR_OI_SHOWN = 1, FR_OI_LOCAL_STOP = 2, FR_OI_FAILED = 3 };
// The source user-data tag on every event posted by fr-input-agent. It must
// not turn its own remote click into a local user's Stop sharing gesture.
static const int64_t FR_REMOTE_INPUT_TAG = 0x46524350;
static dispatch_source_t parent_monitor;
static BOOL occupied;

int fr_oi_bind_parent(uint32_t parent) {
    if (parent <= 1 || parent > INT_MAX || getppid() != (pid_t)parent || parent_monitor ||
        getuid() == 0 || getuid() != geteuid())
        return FR_OI_FAILED;
    dispatch_source_t watch = dispatch_source_create(
        DISPATCH_SOURCE_TYPE_PROC, (uintptr_t)parent, DISPATCH_PROC_EXIT,
        dispatch_get_global_queue(QOS_CLASS_USER_INITIATED, 0));
    if (!watch) return FR_OI_FAILED;
    // There is no held input state to release. Process exit removes the exact
    // status item even if AppKit is stuck; the broker still owns and reaps us.
    dispatch_source_set_event_handler(watch, ^{ _exit(76); });
    parent_monitor = watch;
    dispatch_resume(watch);
    return getppid() == (pid_t)parent ? 0 : FR_OI_FAILED;
}

// A real local gesture: backed by a native event that fr-input-agent did not
// post and that this process did not synthesize.
static BOOL local_gesture(NSEvent *event) {
    CGEventRef native = event.CGEvent;
    return native &&
           CGEventGetIntegerValueField(native, kCGEventSourceUserData) != FR_REMOTE_INPUT_TAG &&
           CGEventGetIntegerValueField(native, kCGEventSourceUnixProcessID) != getpid();
}

@class FrObservationIndicator;

// Click target over the status item's button. It shows or hides the popover
// on mouse-down itself: NSButton's own tracking loop would hold the main
// thread for as long as the button is pressed, and a main thread that misses
// its 250 ms turn ends the host.
@interface FrStatusTarget : NSView
@property(nonatomic, weak) FrObservationIndicator *owner;
@property(nonatomic) BOOL drawn;
@end

// The popover's Stop Sharing control, handled on mouse-down for the same
// reason; it never enters a tracking loop.
@interface FrStopControl : NSView
@property(nonatomic, weak) FrObservationIndicator *owner;
@end

@interface FrObservationIndicator : NSObject <NSPopoverDelegate> {
  @public
    atomic_int state;
}
@property(nonatomic, strong) NSStatusItem *item;
@property(nonatomic, strong) FrStatusTarget *target;
@property(nonatomic, strong) NSPopover *popover;
@property(nonatomic, strong) id<NSObject> activity;
- (void)stop:(int)reason;
- (void)lost:(NSNotification *)notification;
- (void)toggle;
- (BOOL)closeIndicator;
@end

static BOOL server_gone(CGWindowID window);

@implementation FrObservationIndicator
- (instancetype)init {
    self = [super init];
    if (self) {
        atomic_init(&state, FR_OI_OPENING);
        // Delivered on AppKit's main thread. The status item does not depend
        // on the display set, so display changes do not end this owner.
        NSNotificationCenter *center = NSNotificationCenter.defaultCenter;
        for (NSNotificationName name in
             @[ NSApplicationWillHideNotification, NSApplicationWillTerminateNotification ]) {
            [center addObserver:self selector:@selector(lost:) name:name object:NSApp];
        }
    }
    return self;
}
- (void)stop:(int)reason {
    // Native callbacks and accessibility actions may arrive while a visibility
    // query is completing. Preserve the first terminal cause permanently.
    int previous = atomic_load(&state);
    while (previous < FR_OI_LOCAL_STOP &&
           !atomic_compare_exchange_weak(&state, &previous, reason)) {}
}
- (void)lost:(NSNotification *)notification {
    (void)notification;
    [self stop:FR_OI_FAILED];
}
- (void)toggle {
    if (atomic_load(&state) >= FR_OI_LOCAL_STOP) return;
    @try {
        if (_popover.shown) {
            [_popover performClose:nil];
            return;
        }
        // A transient popover needs the app active to see the click that
        // dismisses it. This accessory app has no menus or Dock icon to show.
        if (@available(macOS 14.0, *)) {
            [NSApp activate];
        } else {
            [NSApp activateIgnoringOtherApps:YES];
        }
        [_popover showRelativeToRect:_target.bounds ofView:_target preferredEdge:NSRectEdgeMinY];
        [_item.button highlight:YES];
    } @catch (NSException *exception) {
        (void)exception;
        [self stop:FR_OI_FAILED];
    }
}
- (void)popoverDidClose:(NSNotification *)notification {
    (void)notification;
    @try {
        [_item.button highlight:NO];
    } @catch (NSException *exception) {
        (void)exception;
    }
}
- (BOOL)closeIndicator {
    // Retain scalar identities across the final releases, then ask
    // WindowServer whether those original windows are actually gone. Failure
    // to observe cleanup cannot release this process's sole native UI slot.
    BOOL closed = YES;
    CGWindowID windows[2] = {0};
    NSUInteger count = 0;
    @autoreleasepool {
        @try {
            NSWindow *status = _item.button.window;
            NSWindow *shown = _popover.shown ? _popover.contentViewController.view.window : nil;
            for (NSWindow *window in @[ status ?: NSNull.null, shown ?: NSNull.null ]) {
                if (![window isKindOfClass:NSWindow.class]) continue;
                NSInteger number = window.windowNumber;
                // A system-drawn status item (macOS 26+) has no CGWindowID;
                // its removal is judged by AppKit's isVisible below.
                if (number > 0 && number <= UINT32_MAX) windows[count++] = (CGWindowID)number;
                else if (number <= 0 && window == shown) closed = NO;
            }
            if (_popover) {
                _popover.animates = NO;
                [_popover close];
            }
            // The popover's window otherwise leaves on a later run-loop turn.
            [shown orderOut:nil];
            if (_item) [NSStatusBar.systemStatusBar removeStatusItem:_item];
            if (status.isVisible || shown.isVisible) closed = NO;
        } @catch (NSException *exception) {
            (void)exception;
            closed = NO;
        }
        @try {
            _popover.delegate = nil;
            _popover = nil;
            _target = nil;
            _item = nil;
            if (_activity) [NSProcessInfo.processInfo endActivity:_activity];
            _activity = nil;
        } @catch (NSException *exception) {
            (void)exception;
            closed = NO;
        }
    }
    // Window ordering reaches WindowServer with a later run-loop commit, so
    // an open popover is still listed on screen right after its close. Give
    // that commit a bounded number of short passes (at most ~150 ms).
    BOOL gone = NO;
    for (int pass = 0; pass <= 30 && !gone; ++pass) {
        gone = YES;
        for (NSUInteger i = 0; i < count; ++i) {
            if (!server_gone(windows[i])) gone = NO;
        }
        if (!gone && pass < 30) CFRunLoopRunInMode(kCFRunLoopDefaultMode, 0.005, true);
    }
    return closed && gone;
}
- (void)dealloc {
    @try {
        [NSNotificationCenter.defaultCenter removeObserver:self];
    } @catch (NSException *exception) {
        (void)exception;
    }
    @try {
        (void)[self closeIndicator];
    } @catch (NSException *exception) {
        (void)exception;
    }
}
@end

@implementation FrStatusTarget
- (BOOL)acceptsFirstMouse:(NSEvent *)event {
    (void)event;
    return YES;
}
- (void)mouseDown:(NSEvent *)event {
    (void)event;
    // Showing the popover grants nothing; only its Stop control acts.
    [self.owner toggle];
}
- (void)rightMouseDown:(NSEvent *)event { [self mouseDown:event]; }
- (void)otherMouseDown:(NSEvent *)event { [self mouseDown:event]; }
- (BOOL)accessibilityPerformPress {
    [self.owner toggle];
    return YES;
}
- (BOOL)isOpaque { return NO; }
- (void)drawRect:(NSRect)dirty {
    (void)dirty;
    // The button draws the icon beneath this transparent view. Window
    // mapping alone is not proof that this original target was drawn.
    if (self.owner) self.drawn = YES;
}
@end

@implementation FrStopControl
- (BOOL)acceptsFirstMouse:(NSEvent *)event {
    (void)event;
    return YES;
}
- (void)mouseDown:(NSEvent *)event {
    // An unbacked synthetic NSEvent is not an attributable local gesture.
    if (!local_gesture(event)) return;
    [self.owner stop:FR_OI_LOCAL_STOP];
}
- (void)rightMouseDown:(NSEvent *)event { [self mouseDown:event]; }
- (void)otherMouseDown:(NSEvent *)event { [self mouseDown:event]; }
- (BOOL)accessibilityPerformPress {
    // Accessibility exposes only this negative action, never peer approval.
    [self.owner stop:FR_OI_LOCAL_STOP];
    return YES;
}
- (BOOL)isOpaque { return NO; }
- (void)drawRect:(NSRect)dirty {
    (void)dirty;
    FrObservationIndicator *owner = self.owner;
    if (!owner) return;
    @try {
        [NSColor.systemRedColor setFill];
        [[NSBezierPath bezierPathWithRoundedRect:self.bounds xRadius:6 yRadius:6] fill];
        NSDictionary *attributes = @{
            NSFontAttributeName : [NSFont systemFontOfSize:13 weight:NSFontWeightSemibold],
            NSForegroundColorAttributeName : NSColor.whiteColor,
        };
        NSString *label = @"Stop Sharing";
        NSSize size = [label sizeWithAttributes:attributes];
        [label drawAtPoint:NSMakePoint(round(NSMidX(self.bounds) - size.width / 2),
                                       round(NSMidY(self.bounds) - size.height / 2))
            withAttributes:attributes];
    } @catch (NSException *exception) {
        (void)exception;
        [owner stop:FR_OI_FAILED];
    }
}
@end

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

static BOOL onscreen(CFDictionaryRef info) {
    CFTypeRef value = CFDictionaryGetValue(info, kCGWindowIsOnscreen);
    return value && CFGetTypeID(value) == CFBooleanGetTypeID() &&
           CFBooleanGetValue((CFBooleanRef)value);
}

// WindowServer's record of this process's own window: -1 when it has none,
// otherwise whether that window is on screen.
static int server_window(CGWindowID window) {
    CFArrayRef list = CGWindowListCopyWindowInfo(kCGWindowListOptionIncludingWindow, window);
    if (!list) return -1;
    int result = -1;
    if (CFArrayGetCount(list) == 1) {
        CFTypeRef entry = CFArrayGetValueAtIndex(list, 0);
        if (CFGetTypeID(entry) == CFDictionaryGetTypeID()) {
            CFDictionaryRef info = (CFDictionaryRef)entry;
            if (window_number(info, kCGWindowNumber, window) &&
                window_number(info, kCGWindowOwnerPID, getpid()))
                result = onscreen(info) ? 1 : 0;
        }
    }
    CFRelease(list);
    return result;
}

static BOOL server_gone(CGWindowID window) {
    CFArrayRef list = CGWindowListCopyWindowInfo(kCGWindowListOptionIncludingWindow, window);
    if (!list) return NO;
    BOOL gone = CFArrayGetCount(list) == 0;
    CFRelease(list);
    // A removed status item's window may outlive it briefly; off screen and
    // no longer ordered in is gone for a viewer of the menu bar.
    return gone || server_window(window) == 0;
}

// The original status item must still be installed, drawn and ordered in, and
// it must have been on screen (AppKit's occlusion state and screen) before
// the first readiness. For an item in this process's own window (before
// macOS 26), WindowServer's record of that window must agree. macOS 26 and
// later draw status items in a system process: the window number is then not
// a CGWindowID (2^32 on 26.5 and 27) and AppKit's state is the evidence.
// After readiness, an item the system has collapsed still counts: when
// another app is full screen the menu bar slides away and AppKit reports the
// item off screen (with menuBarVisible still YES in this process), exactly as
// the system's own recording indicator then disappears. No monitor can
// substitute another item.
static int visible(FrObservationIndicator *owner) {
    int status = atomic_load(&owner->state);
    if (status >= FR_OI_LOCAL_STOP) return status;
    if (NSApp.isHidden || NSScreen.screens.count == 0) return FR_OI_FAILED;
    NSStatusItem *item = owner.item;
    NSStatusBarButton *button = item.button;
    FrStatusTarget *target = owner.target;
    if (!item || !item.isVisible || !button || !target || target.superview != button)
        return FR_OI_FAILED;
    NSWindow *window = button.window;
    NSInteger number = window.windowNumber;
    BOOL present = window && window.isVisible;
    BOOL on = present && (window.occlusionState & NSWindowOcclusionStateVisible) && window.screen;
    if (number > 0 && number <= UINT32_MAX) {
        int server = server_window((CGWindowID)number);
        present = present && server >= 0;
        on = on && server == 1;
    } else if (number <= 0) {
        present = on = NO;
    }
    // A collapsed item also has a zero-height button, so its geometry is
    // judged only while it is on screen.
    BOOL drawn = present && target.window == window && target.drawn && !target.hidden &&
                 !button.hidden;
    BOOL shown = drawn && (on ? rectangle(target.bounds) &&
                                    NSContainsRect(target.visibleRect, target.bounds)
                              : status == FR_OI_SHOWN);
    status = atomic_load(&owner->state);
    if (status >= FR_OI_LOCAL_STOP) return status;
    if (shown) return FR_OI_SHOWN;
    return status == FR_OI_OPENING ? FR_OI_OPENING : FR_OI_FAILED;
}

static NSTextField *label(NSString *text, NSFont *font, NSColor *color, NSRect frame) {
    NSTextField *field = [NSTextField labelWithString:text];
    field.font = font;
    field.textColor = color;
    field.frame = frame;
    return field;
}

static NSViewController *stop_sheet(FrObservationIndicator *owner) {
    NSView *view = [[NSView alloc] initWithFrame:NSMakeRect(0, 0, 260, 104)];
    [view addSubview:label(@"Screen sharing is on", [NSFont boldSystemFontOfSize:13],
                           NSColor.labelColor, NSMakeRect(14, 74, 232, 18))];
    [view addSubview:label(@"FrankenRemote is sharing this Mac's screen.",
                           [NSFont systemFontOfSize:11], NSColor.secondaryLabelColor,
                           NSMakeRect(14, 54, 232, 16))];
    FrStopControl *stop = [[FrStopControl alloc] initWithFrame:NSMakeRect(14, 14, 232, 28)];
    stop.owner = owner;
    stop.accessibilityElement = YES;
    stop.accessibilityRole = NSAccessibilityButtonRole;
    stop.accessibilityLabel = @"Stop sharing";
    [view addSubview:stop];
    NSViewController *controller = [[NSViewController alloc] init];
    controller.view = view;
    return controller;
}

int fr_oi_open(void **out) {
    if (!out) return FR_OI_FAILED;
    *out = NULL;
    if (!pthread_main_np() || occupied || !parent_monitor) return FR_OI_FAILED;
    // Even an uncertain partial open cannot be replaced inside this child.
    // Only proven explicit cleanup releases the original native slot.
    occupied = YES;
    @autoreleasepool {
        @try {
            [NSApplication sharedApplication];
            if (!NSApp || ![NSApp setActivationPolicy:NSApplicationActivationPolicyAccessory])
                return FR_OI_FAILED;
            [NSApp finishLaunching];
            FrObservationIndicator *owner = [[FrObservationIndicator alloc] init];
            if (!owner || NSScreen.screens.count == 0) return FR_OI_FAILED;
            // An icon-only accessory app is a prime App Nap candidate, and a
            // napped or coalesced main thread misses its 250 ms turn. The
            // system may still idle-sleep; sleeping ends the host anyway.
            owner.activity = [NSProcessInfo.processInfo
                beginActivityWithOptions:NSActivityUserInitiatedAllowingIdleSystemSleep |
                                         NSActivityLatencyCritical
                                  reason:@"FrankenRemote screen sharing indicator"];
            NSStatusItem *item =
                [NSStatusBar.systemStatusBar statusItemWithLength:NSSquareStatusItemLength];
            NSStatusBarButton *button = item.button;
            if (!item || !button) return FR_OI_FAILED;
            owner.item = item;
            // No NSStatusItemBehaviorRemovalAllowed, so Command-drag does not
            // remove it; the autosave name keeps a moved item's place. An
            // autosaved item also restores a saved hidden state; never that.
            item.behavior = 0;
            item.autosaveName = @"com.frankenremote.observation-indicator";
            item.visible = YES;
            NSString *description = @"FrankenRemote screen sharing is on";
            NSImage *icon = [NSImage imageWithSystemSymbolName:@"rectangle.on.rectangle"
                                      accessibilityDescription:description];
            if (icon) {
                icon.template = YES;
                button.image = icon;
            } else {
                button.title = @"FR";
            }
            button.toolTip = description;
            FrStatusTarget *target = [[FrStatusTarget alloc] initWithFrame:button.bounds];
            if (!target) return FR_OI_FAILED;
            target.owner = owner;
            target.autoresizingMask = NSViewWidthSizable | NSViewHeightSizable;
            target.accessibilityElement = YES;
            target.accessibilityRole = NSAccessibilityButtonRole;
            target.accessibilityLabel = @"FrankenRemote screen sharing is on. Show Stop sharing.";
            [button addSubview:target];
            owner.target = target;
            NSPopover *popover = [[NSPopover alloc] init];
            if (!popover) return FR_OI_FAILED;
            popover.behavior = NSPopoverBehaviorTransient;
            popover.contentViewController = stop_sheet(owner);
            popover.delegate = owner;
            owner.popover = popover;
            [target setNeedsDisplay:YES];
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
        FrObservationIndicator *owner = (__bridge FrObservationIndicator *)handle;
        @try {
            int state = atomic_load(&owner->state);
            if (state >= FR_OI_LOCAL_STOP) return state;
            NSDate *until = [NSDate dateWithTimeIntervalSinceNow:timeout_ms / 1000.0];
            // Bounded native work; floods cannot produce an unbounded queue in
            // this helper or replace completion of the real visibility check.
            unsigned drained = 0;
            for (; drained < 32; ++drained) {
                NSEvent *event = [NSApp nextEventMatchingMask:NSEventMaskAny
                                                   untilDate:until
                                                      inMode:NSDefaultRunLoopMode
                                                     dequeue:YES];
                if (!event) break;
                [NSApp sendEvent:event];
                state = atomic_load(&owner->state);
                if (state >= FR_OI_LOCAL_STOP) return state;
                until = [NSDate distantPast];
            }
            if (drained == 32) {
                // Do not mint readiness behind an already queued local Stop.
                // Peeking is bounded and does not consume or drop that event.
                NSEvent *pending = [NSApp nextEventMatchingMask:NSEventMaskAny
                                                     untilDate:NSDate.distantPast
                                                        inMode:NSDefaultRunLoopMode
                                                       dequeue:NO];
                if (pending) {
                    [owner stop:FR_OI_FAILED];
                    return atomic_load(&owner->state);
                }
            }
            // An already queued NSEvent flood must not bypass every other
            // main-loop source. A zero-time run makes one bounded source pass;
            // the independent watchdog still covers a stuck native callback.
            SInt32 turn = CFRunLoopRunInMode(kCFRunLoopDefaultMode, 0, true);
            if (turn == kCFRunLoopRunStopped || turn == kCFRunLoopRunFinished) {
                [owner stop:FR_OI_FAILED];
                return atomic_load(&owner->state);
            }
            state = atomic_load(&owner->state);
            if (state >= FR_OI_LOCAL_STOP) return state;
            [NSApp updateWindows];
            state = atomic_load(&owner->state);
            if (state >= FR_OI_LOCAL_STOP) return state;
            int status = visible(owner);
            if (status == FR_OI_SHOWN) {
                int opening = FR_OI_OPENING;
                // A callback during any AppKit query above cannot be replaced
                // by a late positive observation of the same status item.
                atomic_compare_exchange_strong(&owner->state, &opening, FR_OI_SHOWN);
            } else if (status >= FR_OI_LOCAL_STOP) {
                [owner stop:status];
            }
            return atomic_load(&owner->state);
        } @catch (NSException *exception) {
            (void)exception;
            [owner stop:FR_OI_FAILED];
            return atomic_load(&owner->state);
        }
    }
}

int fr_oi_close(void *handle) {
    if (!pthread_main_np() || !handle) return FR_OI_FAILED;
    @autoreleasepool {
        @try {
            FrObservationIndicator *owner = (__bridge_transfer FrObservationIndicator *)handle;
            [owner stop:FR_OI_FAILED];
            BOOL closed = [owner closeIndicator];
            owner = nil;
            if (closed) occupied = NO;
            return closed ? 0 : FR_OI_FAILED;
        } @catch (NSException *exception) {
            (void)exception;
            return FR_OI_FAILED;
        }
    }
}
