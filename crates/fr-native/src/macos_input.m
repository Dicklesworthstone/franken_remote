// macOS host input injection (fr-macos-host-control-b14s), compiled only into
// the per-lease input executor `fr-input-agent`; frd never loads it.
//
// Events are Quartz events from one private event source, posted at the HID
// tap so the window server routes them exactly like hardware input. Every
// event carries FR_IN_TAG as its source user data, which lets the control
// indicator tell a remote click on it from the local user's. The executor
// records the keys and buttons it holds after every operation; a panic or
// the parent's exit releases that set here before the process ends.
#import <AppKit/AppKit.h>
#import <CoreGraphics/CoreGraphics.h>
#include <dispatch/dispatch.h>
#include <math.h>
#include <pthread.h>
#include <stdatomic.h>
#include <stdint.h>
#include <unistd.h>
#include "macos_display.h"

#define FR_IN_OK 0
#define FR_IN_INVALID -1
#define FR_IN_UNAVAILABLE -2
#define FR_IN_PERMISSION -3
#define FR_IN_GEOMETRY -5
#define FR_IN_TOO_LARGE -9

// "FRCP": source user data on every event this executor posts.
static const int64_t FR_IN_TAG = 0x46524350;
// Real hardware events carry this bit; applications see ours the same way.
static const uint64_t FR_IN_NONCOALESCED = 0x100;

typedef struct {
    FrDisplay display;
    double origin_x;  // the display's bounds in global points
    double origin_y;
} FrInDisplay;

static int describe(CGDirectDisplayID id, FrInDisplay *out) {
    CGRect points;
    switch (fr_display_describe(id, &out->display, &points)) {
    case FR_DISPLAY_OK:
        out->origin_x = points.origin.x;
        out->origin_y = points.origin.y;
        return FR_IN_OK;
    case FR_DISPLAY_GONE: return FR_IN_GEOMETRY;
    default: return FR_IN_UNAVAILABLE;
    }
}

// Accessibility ("control your computer") is what posting events requires.
// This checks and never prompts.
int fr_in_access(void) { return CGPreflightPostEventAccess() ? 1 : 0; }

// Asks for the grant on behalf of this process's responsible process (frd
// when launchd runs it). macOS shows its prompt only while no decision is
// recorded; otherwise this only reports the current state.
int fr_in_request_access(void) { return CGRequestPostEventAccess() ? 1 : 0; }

// Active displays, mirrors excluded (the capture worker offers only sources).
// Core Graphics only: input needs no Screen Recording grant.
int fr_in_displays(FrInDisplay *out, size_t capacity, size_t *count) {
    if (!out || !count) return FR_IN_INVALID;
    *count = 0;
    CGDirectDisplayID ids[16];
    uint32_t total = 0;
    if (CGGetActiveDisplayList(16, ids, &total) != kCGErrorSuccess) return FR_IN_UNAVAILABLE;
    size_t n = 0;
    for (uint32_t i = 0; i < total; i++) {
        if (CGDisplayMirrorsDisplay(ids[i]) != kCGNullDirectDisplay) continue;
        if (n == capacity) return FR_IN_TOO_LARGE;
        int status = describe(ids[i], &out[n]);
        if (status) return status;
        n++;
    }
    *count = n;
    return FR_IN_OK;
}

// FR_IN_OK while the display still has exactly this mode and placement.
int fr_in_display_check(const FrInDisplay *expected) {
    if (!expected) return FR_IN_INVALID;
    FrInDisplay now;
    int status = describe(expected->display.display_id, &now);
    if (status) return status;
    return fr_display_same(&expected->display, &now.display) && now.origin_x == expected->origin_x &&
                   now.origin_y == expected->origin_y
               ? FR_IN_OK
               : FR_IN_GEOMETRY;
}

static CGEventSourceRef new_source(void) {
    CGEventSourceRef source = CGEventSourceCreate(kCGEventSourceStatePrivate);
    if (!source) return NULL;
    CGEventSourceSetUserData(source, FR_IN_TAG);
    // Quartz otherwise suppresses the local mouse and keyboard for 0.25 s
    // after each posted event: the local user must never lose their devices
    // (and the indicator) to a remote controller.
    CGEventSourceSetLocalEventsSuppressionInterval(source, 0);
    return source;
}

void *fr_in_open(void) { return (void *)new_source(); }

void fr_in_close(void *source) {
    if (source) CFRelease((CGEventSourceRef)source);
}

// 0: the HID tap, like hardware. Otherwise one process id: every event goes
// to that process only and never reaches the desktop. The event test reads
// back what this executor posts this way; the agent never sets it.
static _Atomic int32_t route_pid;

void fr_in_route_to_process(int32_t pid) { atomic_store(&route_pid, pid > 0 ? pid : 0); }

// A complete event reaches the OS without a native allocation, pointer read
// or property mutation after Rust's final original-authority check.
static void post_ready(CGEventRef event) {
    int32_t pid = atomic_load(&route_pid);
    if (pid)
        CGEventPostToPid(pid, event);
    else
        CGEventPost(kCGHIDEventTap, event);
}

static void post_borrowed(CGEventRef event, uint64_t flags) {
    CGEventSetFlags(event, (CGEventFlags)(flags | FR_IN_NONCOALESCED));
    post_ready(event);
}

static int prepared(CGEventRef event, uint64_t flags, void **out) {
    if (!event) return FR_IN_UNAVAILABLE;
    CGEventSetFlags(event, (CGEventFlags)(flags | FR_IN_NONCOALESCED));
    *out = event;
    return FR_IN_OK;
}

int fr_in_event_post(void *event) {
    if (!event) return FR_IN_INVALID;
    post_ready((CGEventRef)event);
    return FR_IN_OK;
}

static int post(CGEventRef event, uint64_t flags) {
    if (!event) return FR_IN_UNAVAILABLE;
    post_borrowed(event, flags);
    CFRelease(event);
    return FR_IN_OK;
}

// `modifier`: the key is a held modifier, so the window server must see a
// flags change (`flags` is the state after it), as hardware produces.
int fr_in_key_prepare(void *source, uint16_t keycode, int down, int repeat, int modifier,
                       uint64_t flags, void **out) {
    if (!out) return FR_IN_INVALID;
    *out = NULL;
    if (!source || keycode >= 128) return FR_IN_INVALID;
    CGEventRef event = CGEventCreateKeyboardEvent((CGEventSourceRef)source, keycode, down != 0);
    if (!event) return FR_IN_UNAVAILABLE;
    if (modifier) CGEventSetType(event, kCGEventFlagsChanged);
    if (repeat) CGEventSetIntegerValueField(event, kCGKeyboardEventAutorepeat, 1);
    return prepared(event, flags, out);
}

// Release-only cleanup calls the combined form after authority has ended.
int fr_in_key(void *source, uint16_t keycode, int down, int repeat, int modifier, uint64_t flags) {
    if (down || repeat) return FR_IN_INVALID;
    void *event = NULL;
    int result = fr_in_key_prepare(source, keycode, down, repeat, modifier, flags, &event);
    if (result) return result;
    result = fr_in_event_post(event);
    CFRelease((CGEventRef)event);
    return result;
}

// Prepare one complete Unicode scalar. Rust owns both returned events; no
// native pointer or text is retained beyond those bounded event objects.
// Virtual key zero is used only while the owner's remote held set is empty.
// No layout translation, modifier synthesis or clipboard fallback occurs.
int fr_in_text_prepare(void *source, const uint16_t *units, size_t count,
                       void **down, void **up) {
    if (!down || !up) return FR_IN_INVALID;
    *down = NULL;
    *up = NULL;
    if (!source || !units || (count != 1 && count != 2)) return FR_IN_INVALID;
    if ((count == 1 && units[0] >= 0xd800 && units[0] <= 0xdfff) ||
        (count == 2 && !(units[0] >= 0xd800 && units[0] <= 0xdbff &&
                        units[1] >= 0xdc00 && units[1] <= 0xdfff)))
        return FR_IN_INVALID;
    CGEventRef press = CGEventCreateKeyboardEvent((CGEventSourceRef)source, 0, true);
    CGEventRef release = CGEventCreateKeyboardEvent((CGEventSourceRef)source, 0, false);
    if (!press || !release) {
        if (press) CFRelease(press);
        if (release) CFRelease(release);
        return FR_IN_UNAVAILABLE;
    }
    CGEventKeyboardSetUnicodeString(press, (UniCharCount)count, units);
    // The release has no committed payload: it only cleans up the key-down.
    CGEventKeyboardSetUnicodeString(release, 0, NULL);
    UniChar actual[2] = {0, 0};
    UniCharCount length = 0, released = 0;
    CGEventKeyboardGetUnicodeString(press, 2, &length, actual);
    CGEventKeyboardGetUnicodeString(release, 0, &released, NULL);
    if (length != count || actual[0] != units[0] ||
        (count == 2 && actual[1] != units[1]) || released != 0) {
        CFRelease(press);
        CFRelease(release);
        return FR_IN_UNAVAILABLE;
    }
    CGEventSetFlags(press, (CGEventFlags)FR_IN_NONCOALESCED);
    CGEventSetFlags(release, (CGEventFlags)FR_IN_NONCOALESCED);
    *down = press;
    *up = release;
    return FR_IN_OK;
}

// Both allocations/UTF-16 validation already completed. The caller has just
// checked the original authority deadline and recorded key zero pessimistically.
// No fallible work is performed between the two posts: after the positive
// submission, the empty key-up is unconditional release-only cleanup. Quartz
// posts have no application-delivery result; success means submitted, not seen.
int fr_in_text_post(void *down, void *up) {
    // These private, immutable owners came only from text_prepare. Permission
    // and display queries run BEFORE Rust's final deadline check, never here.
    if (!down || !up || down == up) return FR_IN_INVALID;
    post_ready((CGEventRef)down);
    post_ready((CGEventRef)up);
    return FR_IN_OK;
}

void fr_in_event_close(void *event) {
    if (event) CFRelease((CGEventRef)event);
}

static int current_location(CGPoint *out) {
    CGEventRef now = CGEventCreate(NULL);
    if (!now) return FR_IN_UNAVAILABLE;
    CGPoint at = CGEventGetLocation(now);
    CFRelease(now);
    if (!isfinite(at.x) || !isfinite(at.y)) return FR_IN_UNAVAILABLE;
    *out = at;
    return FR_IN_OK;
}

int fr_in_location(double *x, double *y) {
    if (!x || !y) return FR_IN_INVALID;
    CGPoint at;
    int result = current_location(&at);
    if (result) return result;
    *x = at.x;
    *y = at.y;
    return FR_IN_OK;
}

// `button`: 0 left, 1 right, 2 middle, 3 back, 4 forward (Quartz numbers).
static void mouse_types(int button, CGEventType *down, CGEventType *up, CGEventType *dragged) {
    switch (button) {
    case 0:
        *down = kCGEventLeftMouseDown;
        *up = kCGEventLeftMouseUp;
        *dragged = kCGEventLeftMouseDragged;
        break;
    case 1:
        *down = kCGEventRightMouseDown;
        *up = kCGEventRightMouseUp;
        *dragged = kCGEventRightMouseDragged;
        break;
    default:
        *down = kCGEventOtherMouseDown;
        *up = kCGEventOtherMouseUp;
        *dragged = kCGEventOtherMouseDragged;
        break;
    }
}

// Move to (x, y) global points. `held` is the lowest-numbered button this
// executor holds (-1 for none): a move with a button down is a drag.
int fr_in_move_prepare(void *source, double x, double y, int held, uint64_t flags, void **out) {
    if (!out) return FR_IN_INVALID;
    *out = NULL;
    if (!source || held < -1 || held > 4 || !isfinite(x) || !isfinite(y)) return FR_IN_INVALID;
    CGEventType type = kCGEventMouseMoved, down, up;
    CGMouseButton button = kCGMouseButtonLeft;
    if (held >= 0) {
        mouse_types(held, &down, &up, &type);
        button = (CGMouseButton)held;
    }
    CGPoint from, to = CGPointMake(x, y);
    int result = current_location(&from);
    if (result) return result;
    CGEventRef event = CGEventCreateMouseEvent((CGEventSourceRef)source, type, to, button);
    if (!event) return FR_IN_UNAVAILABLE;
    // Applications that track relative motion read these, as from a mouse.
    CGEventSetIntegerValueField(event, kCGMouseEventDeltaX, (int64_t)lround(to.x - from.x));
    CGEventSetIntegerValueField(event, kCGMouseEventDeltaY, (int64_t)lround(to.y - from.y));
    if (held >= 2) CGEventSetIntegerValueField(event, kCGMouseEventButtonNumber, held);
    return prepared(event, flags, out);
}

// A press or release at (x, y): where this executor last placed the pointer,
// since a just-posted move may not have reached the window server yet.
int fr_in_button_prepare(void *source, int button, int pressed, int64_t clicks, double x, double y,
                         uint64_t flags, void **out) {
    if (!out) return FR_IN_INVALID;
    *out = NULL;
    if (!source || button < 0 || button > 4 || clicks < 1 || !isfinite(x) || !isfinite(y))
        return FR_IN_INVALID;
    CGEventType down, up, dragged;
    mouse_types(button, &down, &up, &dragged);
    CGEventRef event = CGEventCreateMouseEvent((CGEventSourceRef)source, pressed ? down : up,
                                               CGPointMake(x, y), (CGMouseButton)button);
    if (!event) return FR_IN_UNAVAILABLE;
    CGEventSetIntegerValueField(event, kCGMouseEventClickState, clicks);
    if (button >= 2) CGEventSetIntegerValueField(event, kCGMouseEventButtonNumber, button);
    return prepared(event, flags, out);
}

// Release-only cleanup does not acquire fresh authority or move the pointer
// using a new query. The caller supplies its retained known position.
int fr_in_button(void *source, int button, int pressed, int64_t clicks, double x, double y,
                 uint64_t flags) {
    if (pressed) return FR_IN_INVALID;
    void *event = NULL;
    int result = fr_in_button_prepare(source, button, pressed, clicks, x, y, flags, &event);
    if (result) return result;
    result = fr_in_event_post(event);
    CFRelease((CGEventRef)event);
    return result;
}

// Whole lines or signed 16.16 logical pixels. Both retain exactly the native
// distance without display scaling; positive values mean up/left (Quartz).
int fr_in_scroll_prepare(void *source, int32_t delta_y, int32_t delta_x, double x, double y,
                         int pixels, uint64_t flags, void **out) {
    if (!out) return FR_IN_INVALID;
    *out = NULL;
    if (!source || (pixels != 0 && pixels != 1) || !isfinite(x) || !isfinite(y))
        return FR_IN_INVALID;
    CGEventRef event = CGEventCreateScrollWheelEvent2((CGEventSourceRef)source,
        pixels ? kCGScrollEventUnitPixel : kCGScrollEventUnitLine, 2,
        pixels ? delta_y / 65536 : delta_y, pixels ? delta_x / 65536 : delta_x, 0);
    if (!event) return FR_IN_UNAVAILABLE;
    if (pixels) {
        CGEventSetIntegerValueField(event, kCGScrollWheelEventFixedPtDeltaAxis1, delta_y);
        CGEventSetIntegerValueField(event, kCGScrollWheelEventFixedPtDeltaAxis2, delta_x);
    }
    CGEventSetLocation(event, CGPointMake(x, y));
    return prepared(event, flags, out);
}

double fr_in_double_click_interval(void) { return [NSEvent doubleClickInterval]; }

// ---- Last-resort release --------------------------------------------------

static _Atomic uint64_t held_keys[2];
static _Atomic uint32_t held_buttons;
static atomic_flag releasing = ATOMIC_FLAG_INIT;

// The executor's current held set: keycodes 0..127 and buttons 0..4.
void fr_in_record(uint64_t keys_low, uint64_t keys_high, uint32_t buttons) {
    atomic_store(&held_keys[0], keys_low);
    atomic_store(&held_keys[1], keys_high);
    atomic_store(&held_buttons, buttons);
}

static int is_modifier(uint16_t keycode) {
    return keycode >= 0x36 && keycode <= 0x3e && keycode != 0x39;
}

// Release every recorded key and button once per process, on a fresh source
// so it never races the executor's own. Returns 1 only when all release posts
// were submitted; 0 means already attempted or incomplete/unknown cleanup.
int fr_in_emergency_release(void) {
    if (atomic_flag_test_and_set(&releasing)) return 0;
    CGEventSourceRef source = new_source();
    if (!source) return 0;
    BOOL complete = YES;
    uint64_t keys[2] = {atomic_load(&held_keys[0]), atomic_load(&held_keys[1])};
    for (uint16_t keycode = 0; keycode < 128; keycode++) {
        if (!(keys[keycode / 64] & (1ULL << (keycode % 64)))) continue;
        CGEventRef event = CGEventCreateKeyboardEvent(source, keycode, false);
        if (!event) {
            complete = NO;
            continue;
        }
        if (is_modifier(keycode)) CGEventSetType(event, kCGEventFlagsChanged);
        post(event, 0);
    }
    uint32_t buttons = atomic_load(&held_buttons);
    CGPoint at;
    int location = buttons ? current_location(&at) : FR_IN_OK;
    if (location != FR_IN_OK) complete = NO;
    for (int button = 0; location == FR_IN_OK && button < 5; button++) {
        if (!(buttons & (1u << button))) continue;
        CGEventType down, up, dragged;
        mouse_types(button, &down, &up, &dragged);
        CGEventRef event = CGEventCreateMouseEvent(source, up, at, (CGMouseButton)button);
        if (!event) {
            complete = NO;
            continue;
        }
        if (button >= 2) CGEventSetIntegerValueField(event, kCGMouseEventButtonNumber, button);
        post(event, 0);
    }
    CFRelease(source);
    return complete ? 1 : 0;
}

// The macOS form of Linux's parent-death signal: the expected parent's exit
// releases the recorded set and ends this process. Both parent checks refuse
// an executor that was already reparented.
int fr_in_bind_parent(int32_t expected) {
    static dispatch_source_t watch;
    if (expected <= 1 || watch || getppid() != expected) return FR_IN_INVALID;
    watch = dispatch_source_create(DISPATCH_SOURCE_TYPE_PROC, (uintptr_t)expected, DISPATCH_PROC_EXIT,
                                   dispatch_get_global_queue(QOS_CLASS_USER_INTERACTIVE, 0));
    if (!watch) return FR_IN_UNAVAILABLE;
    dispatch_source_set_event_handler(watch, ^{
      fr_in_emergency_release();
      _exit(0);
    });
    dispatch_resume(watch);
    if (getppid() != expected) _exit(0);
    return FR_IN_OK;
}

// ---- The control indicator -------------------------------------------------
//
// A small always-on-top panel at the top of the controlled display, shown for
// the whole lease: the local user can see that the Mac is remotely
// controlled and stop it with one click. Main thread only.

enum { FR_IND_SHOWN = 0, FR_IND_LOCAL_STOP = 1, FR_IND_FAILED = 2 };
static atomic_int indicator_state = FR_IND_SHOWN;

// A click is the local user's unless this executor posted it.
static BOOL posted_here(NSEvent *event) {
    CGEventRef cg = event.CGEvent;
    if (!cg) return NO;
    return CGEventGetIntegerValueField(cg, kCGEventSourceUserData) == FR_IN_TAG ||
           CGEventGetIntegerValueField(cg, kCGEventSourceUnixProcessID) == getpid();
}

@interface FrControlBadge : NSView
@property(nonatomic) BOOL clipboard;
@end

@implementation FrControlBadge
- (BOOL)acceptsFirstMouse:(NSEvent *)event {
    (void)event;
    return YES;
}
- (NSView *)hitTest:(NSPoint)point {
    return NSPointInRect([self convertPoint:point fromView:self.superview], self.bounds) ? self : nil;
}
- (void)stopFrom:(NSEvent *)event {
    if (posted_here(event)) return;
    int shown = FR_IND_SHOWN;
    atomic_compare_exchange_strong(&indicator_state, &shown, FR_IND_LOCAL_STOP);
}
- (void)mouseDown:(NSEvent *)event {
    [self stopFrom:event];
}
- (void)rightMouseDown:(NSEvent *)event {
    [self stopFrom:event];
}
- (void)otherMouseDown:(NSEvent *)event {
    [self stopFrom:event];
}
- (void)drawRect:(NSRect)dirty {
    (void)dirty;
    NSRect frame = NSInsetRect(self.bounds, 0.5, 0.5);
    NSBezierPath *shape = [NSBezierPath bezierPathWithRoundedRect:frame
                                                          xRadius:frame.size.height / 2
                                                          yRadius:frame.size.height / 2];
    [[NSColor colorWithSRGBRed:0.80 green:0.12 blue:0.10 alpha:0.94] setFill];
    [shape fill];
    NSDictionary *attributes = @{
        NSFontAttributeName : [NSFont systemFontOfSize:13 weight:NSFontWeightSemibold],
        NSForegroundColorAttributeName : NSColor.whiteColor,
    };
    NSString *text = self.clipboard ? @"Clipboard sharing — click to turn off"
                                    : @"Remote control active — click to stop";
    NSSize size = [text sizeWithAttributes:attributes];
    [text drawAtPoint:NSMakePoint(NSMidX(self.bounds) - size.width / 2,
                                  NSMidY(self.bounds) - size.height / 2)
       withAttributes:attributes];
}
@end

static NSPanel *indicator;
static BOOL clipboard_indicator;

static NSScreen *screen_for(uint32_t display_id) {
    for (NSScreen *screen in NSScreen.screens) {
        NSNumber *number = screen.deviceDescription[@"NSScreenNumber"];
        if (number && number.unsignedIntValue == display_id) return screen;
    }
    return nil;
}

static BOOL onscreen(NSInteger window) {
    if (window <= 0) return NO;
    CFArrayRef list = CGWindowListCopyWindowInfo(kCGWindowListOptionIncludingWindow,
                                                 (CGWindowID)window);
    if (!list) return NO;
    BOOL shown = NO;
    if (CFArrayGetCount(list) == 1) {
        NSDictionary *info = (__bridge NSDictionary *)CFArrayGetValueAtIndex(list, 0);
        shown = [info[(__bridge NSString *)kCGWindowIsOnscreen] boolValue];
    }
    CFRelease(list);
    return shown;
}

static BOOL clipboard_visible(NSPanel *panel) {
    return panel.isVisible && !panel.isMiniaturized && panel.isOnActiveSpace &&
           !NSApp.isHidden && (panel.occlusionState & NSWindowOcclusionStateVisible) &&
           onscreen(panel.windowNumber);
}

// Show the indicator on `display_id` and wait (bounded) until the window
// server has it on screen. FR_IN_OK only then.
static int indicator_open(uint32_t display_id, uint32_t timeout_ms, BOOL clipboard) {
    if (!pthread_main_np() || indicator) return FR_IN_INVALID;
    @autoreleasepool {
        [NSApplication sharedApplication];
        // No Dock icon, no menu bar, never the active application.
        [NSApp setActivationPolicy:NSApplicationActivationPolicyAccessory];
        [NSApp finishLaunching];
        NSScreen *screen = screen_for(display_id);
        if (!screen) return FR_IN_GEOMETRY;
        NSRect visible = screen.visibleFrame;
        NSSize size = NSMakeSize(clipboard ? 340 : 300, 28);
        NSRect frame = NSMakeRect(round(NSMidX(visible) - size.width / 2),
                                  round(NSMaxY(visible) - size.height - (clipboard ? 40 : 6)),
                                  size.width, size.height);
        NSPanel *panel =
            [[NSPanel alloc] initWithContentRect:frame
                                       styleMask:NSWindowStyleMaskBorderless |
                                                 NSWindowStyleMaskNonactivatingPanel
                                         backing:NSBackingStoreBuffered
                                           defer:NO];
        if (!panel) return FR_IN_UNAVAILABLE;
        panel.level = NSStatusWindowLevel;
        panel.collectionBehavior =
            NSWindowCollectionBehaviorCanJoinAllSpaces | NSWindowCollectionBehaviorStationary |
            NSWindowCollectionBehaviorFullScreenAuxiliary | NSWindowCollectionBehaviorIgnoresCycle;
        panel.opaque = NO;
        panel.backgroundColor = NSColor.clearColor;
        panel.hasShadow = YES;
        panel.hidesOnDeactivate = NO;
        panel.floatingPanel = YES;
        panel.becomesKeyOnlyIfNeeded = YES;
        panel.releasedWhenClosed = NO;
        FrControlBadge *badge =
            [[FrControlBadge alloc] initWithFrame:NSMakeRect(0, 0, size.width, size.height)];
        if (!badge) return FR_IN_UNAVAILABLE;
        badge.clipboard = clipboard;
        panel.contentView = badge;
        clipboard_indicator = clipboard;
        indicator = panel;
        [panel orderFrontRegardless];
        NSDate *until = [NSDate dateWithTimeIntervalSinceNow:timeout_ms / 1000.0];
        while (!(clipboard ? clipboard_visible(panel) : onscreen(panel.windowNumber))) {
            if ([until timeIntervalSinceNow] <= 0) return FR_IN_UNAVAILABLE;
            NSEvent *event = [NSApp nextEventMatchingMask:NSEventMaskAny
                                                untilDate:[NSDate dateWithTimeIntervalSinceNow:0.005]
                                                   inMode:NSDefaultRunLoopMode
                                                  dequeue:YES];
            if (event) [NSApp sendEvent:event];
        }
    }
    return FR_IN_OK;
}

// These roles run in separate children; neither can replace a live panel.
int fr_in_indicator_open(uint32_t display_id, uint32_t timeout_ms) {
    @try {
        return indicator_open(display_id, timeout_ms, NO);
    } @catch (NSException *exception) {
        (void)exception;
        return FR_IN_UNAVAILABLE;
    }
}

int fr_in_clipboard_indicator_open(uint32_t timeout_ms) {
    @try {
        // Clipboard covers the logged-in session; show its independent Off
        // surface on the local main screen, below the control badge.
        return indicator_open(CGMainDisplayID(), timeout_ms, YES);
    } @catch (NSException *exception) {
        (void)exception;
        return FR_IN_UNAVAILABLE;
    }
}

// Handle indicator events for up to `timeout_ms`. FR_IND_SHOWN while shown,
// FR_IND_LOCAL_STOP once the local user clicked it or hid it.
int fr_in_indicator_pump(uint32_t timeout_ms) {
    if (!pthread_main_np() || !indicator) return FR_IND_FAILED;
    @autoreleasepool {
        @try {
            NSDate *until = [NSDate dateWithTimeIntervalSinceNow:timeout_ms / 1000.0];
            // Bounded event work: a flood cannot starve the privacy heartbeat.
            for (unsigned i = 0; i < 64; ++i) {
                NSEvent *event = [NSApp nextEventMatchingMask:NSEventMaskAny
                                                   untilDate:until
                                                      inMode:NSDefaultRunLoopMode
                                                     dequeue:YES];
                if (!event) break;
                [NSApp sendEvent:event];
                until = [NSDate distantPast];
            }
            if (!indicator.isVisible || NSApp.isHidden) {
                int shown = FR_IND_SHOWN;
                atomic_compare_exchange_strong(&indicator_state, &shown, FR_IND_LOCAL_STOP);
            } else if (clipboard_indicator && !clipboard_visible(indicator)) {
                int shown = FR_IND_SHOWN;
                atomic_compare_exchange_strong(&indicator_state, &shown, FR_IND_FAILED);
            }
        } @catch (NSException *exception) {
            (void)exception;
            int shown = FR_IND_SHOWN;
            atomic_compare_exchange_strong(&indicator_state, &shown, FR_IND_FAILED);
        }
    }
    return atomic_load(&indicator_state);
}

void fr_in_indicator_close(void) {
    if (!pthread_main_np() || !indicator) return;
    NSPanel *panel = indicator;
    indicator = nil;
    @try {
        [panel orderOut:nil];
    } @catch (NSException *exception) {
        // Native cleanup cannot unwind across Rust's Drop boundary.
        (void)exception;
    }
}
