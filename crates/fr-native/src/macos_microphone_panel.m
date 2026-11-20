/* Original visible microphone controls, main AppKit thread only. No capture,
 * permission request, network or nested event loop is performed here. Rust's
 * terminal heartbeat independently fences a stalled native/UI call. */
#import <AppKit/AppKit.h>
#import <CoreGraphics/CoreGraphics.h>
#include <math.h>
#include <stdint.h>
#include <unistd.h>

typedef int (*FrMicAction)(void *, uint32_t, uint64_t);
typedef void (*FrMicRelease)(void *);
@class FrMicOwner;
@interface FrMicPanel : NSPanel
@end
@implementation FrMicPanel
- (BOOL)canBecomeKeyWindow { return NO; }
- (BOOL)canBecomeMainWindow { return NO; }
@end
@interface FrMicView : NSView
@property(nonatomic) BOOL drawn;
@end
@implementation FrMicView
- (BOOL)isFlipped { return YES; }
- (BOOL)isOpaque { return YES; }
- (void)drawRect:(NSRect)dirty {
    (void)dirty;
    [NSColor.windowBackgroundColor setFill]; NSRectFill(self.bounds);
    self.drawn = YES;
}
@end
@interface FrMicButton : NSButton
@end
@implementation FrMicButton
- (BOOL)acceptsFirstResponder { return NO; }
- (BOOL)acceptsFirstMouse:(NSEvent *)event { (void)event; return YES; }
- (void)mouseDown:(NSEvent *)event {
    NSPoint point = [self convertPoint:event.locationInWindow fromView:nil];
    if (self.enabled && NSPointInRect(point, self.bounds))
        [NSApp sendAction:self.action to:self.target from:self];
}
- (void)mouseDragged:(NSEvent *)event { (void)event; }
- (void)mouseUp:(NSEvent *)event { (void)event; }
@end
@interface FrMicOwner : NSObject {
@public
    FrMicPanel *panel;
    FrMicView *surface;
    NSTextField *label;
    FrMicButton *allow, *talk, *mute;
    void *context;
    FrMicAction action;
    FrMicRelease release;
    BOOL stopped;
    double enabledAt;
}
- (void)choose:(id)sender;
- (void)stop;
@end
/* Retain even partial construction/quarantined cleanup. No new owner replaces
 * an original panel that has not positively closed. */
static FrMicOwner *fr_mic_owner;

static BOOL visible(FrMicOwner *owner) {
    FrMicPanel *panel = owner->panel;
    FrMicView *surface = owner->surface;
    if (!panel || !surface || !owner->mute || !surface.drawn || NSApp.isHidden ||
        owner->stopped || !panel.isVisible || panel.isMiniaturized || !panel.isOnActiveSpace ||
        !(panel.occlusionState & NSWindowOcclusionStateVisible) || surface.hidden ||
        owner->mute.hidden || !owner->mute.enabled || surface.window != panel ||
        owner->mute.window != panel || !NSContainsRect(surface.visibleRect, surface.bounds) ||
        !NSContainsRect(surface.visibleRect, owner->mute.frame)) return NO;
    NSInteger number = panel.windowNumber;
    if (number <= 0 || number > UINT32_MAX) return NO;
    CFArrayRef windows = CGWindowListCopyWindowInfo(kCGWindowListOptionIncludingWindow,
                                                   (CGWindowID)number);
    if (!windows) return NO;
    BOOL shown = NO;
    if (CFArrayGetCount(windows) == 1) {
        NSDictionary *info = (__bridge NSDictionary *)CFArrayGetValueAtIndex(windows, 0);
        shown = [info[(__bridge NSString *)kCGWindowIsOnscreen] boolValue];
    }
    CFRelease(windows);
    return shown;
}
@implementation FrMicOwner
- (void)stop {
    if (context && action) action(context, 0, 0);
    stopped = YES;
    @try { allow.enabled = NO; talk.enabled = NO; }
    @catch (NSException *exception) { (void)exception; }
}
- (void)choose:(id)sender {
    @try {
    if (!context || !action || stopped) return;
    /* Negative consent is immediate, including stale mouse events. */
    if (sender == mute) { action(context, 3, 0); return; }
    FrMicButton *button = sender == allow ? allow : (sender == talk ? talk : nil);
    NSEvent *event = NSApp.currentEvent;
    double now = NSProcessInfo.processInfo.systemUptime;
    double captured = event ? event.timestamp : 0;
    if (!button || !button.enabled || !visible(self) || !event ||
        event.type != NSEventTypeLeftMouseDown || event.window != panel ||
        !isfinite(captured) || captured <= 0 || captured < enabledAt ||
        !isfinite(now) || now < captured || now - captured >= 0.250 ||
        (event.modifierFlags & (NSEventModifierFlagShift | NSEventModifierFlagControl |
                               NSEventModifierFlagOption | NSEventModifierFlagCommand))) return;
    CGEventRef native = event.CGEvent;
    if (!native || CGEventGetIntegerValueField(native, kCGEventSourceUserData) == INT64_C(0x46524350) ||
        CGEventGetIntegerValueField(native, kCGEventSourceUnixProcessID) == getpid()) return;
    /* Carry the original event time, not an age sampled before the possibly
     * blocking visibility query above. Rust correlates it conservatively. */
    if (captured > (double)UINT64_MAX / 1e9) return;
    if (action(context, sender == allow ? 1 : 2, (uint64_t)floor(captured * 1e9))) {
        allow.enabled = NO; talk.enabled = NO;
    }
    } @catch (NSException *exception) { (void)exception; [self stop]; }
}
@end

static FrMicButton *button(FrMicOwner *owner, NSString *title, NSRect frame) {
    FrMicButton *value = [[FrMicButton alloc] initWithFrame:frame];
    value.title = title; value.bezelStyle = NSBezelStyleRounded;
    value.controlSize = NSControlSizeSmall; value.target = owner;
    value.action = @selector(choose:); value.font = [NSFont systemFontOfSize:11];
    [owner->surface addSubview:value]; return value;
}

/* Context ownership is adopted iff *out is non-null, even after failed open.
 * Rust then requests close; successful close alone invokes release once. */
int fr_mac_mic_panel_open(void *context, FrMicAction action, FrMicRelease release, void **out) {
    if (!out) return -1;
    *out = NULL;
    if (![NSThread isMainThread] || !context || !action || !release || fr_mic_owner) return -1;
    @try {
        FrMicOwner *owner = [FrMicOwner new];
        if (!owner) return -1;
        fr_mic_owner = owner;
        owner->context = context; owner->action = action; owner->release = release;
        *out = (__bridge void *)owner;
        NSScreen *screen = NSScreen.mainScreen;
        NSRect available = screen.visibleFrame;
        if (!screen || NSWidth(available) < 570 || NSHeight(available) < 120) return -1;
        NSRect frame = NSMakeRect(floor(NSMidX(available) - 285),
            floor(NSMaxY(available) - 104), 570, 44);
        owner->panel = [[FrMicPanel alloc] initWithContentRect:frame
            styleMask:NSWindowStyleMaskBorderless | NSWindowStyleMaskNonactivatingPanel
            backing:NSBackingStoreBuffered defer:NO];
        FrMicPanel *panel = owner->panel;
        panel.title = @"FrankenRemote microphone";
        panel.level = NSStatusWindowLevel;
        panel.collectionBehavior = NSWindowCollectionBehaviorCanJoinAllSpaces |
            NSWindowCollectionBehaviorStationary | NSWindowCollectionBehaviorFullScreenAuxiliary |
            NSWindowCollectionBehaviorIgnoresCycle;
        panel.hidesOnDeactivate = NO; panel.floatingPanel = YES;
        panel.becomesKeyOnlyIfNeeded = YES; panel.releasedWhenClosed = NO; panel.hasShadow = YES;
        owner->surface = [[FrMicView alloc] initWithFrame:NSMakeRect(0, 0, 570, 44)];
        panel.contentView = owner->surface;
        owner->label = [NSTextField labelWithString:@"Microphone: muted"];
        owner->label.frame = NSMakeRect(12, 14, 205, 18);
        owner->label.font = [NSFont systemFontOfSize:11];
        [owner->surface addSubview:owner->label];
        owner->allow = button(owner, @"Allow microphone", NSMakeRect(222, 8, 150, 28));
        owner->talk = button(owner, @"Talk", NSMakeRect(380, 8, 80, 28));
        owner->mute = button(owner, @"Mute", NSMakeRect(470, 8, 88, 28));
        if (!panel || !owner->surface || !owner->label || !owner->allow || !owner->talk || !owner->mute)
            return -1;
        owner->allow.enabled = NO; owner->talk.enabled = NO;
        owner->mute.accessibilityLabel = @"Immediately mute FrankenRemote microphone";
        [panel orderFrontRegardless]; [panel displayIfNeeded];
        return visible(owner) ? 0 : -1;
    } @catch (NSException *exception) { (void)exception; return -1; }
}

int fr_mac_mic_panel_update(void *handle, uint32_t state, int can_allow, int can_talk, int *shown) {
    if (![NSThread isMainThread] || !handle || !shown || state > 5) return -1;
    FrMicOwner *owner = (__bridge FrMicOwner *)handle;
    if (owner != fr_mic_owner) return -1;
    *shown = 0;
    @try {
        if (!visible(owner)) { [owner stop]; return 0; }
        NSString *const labels[] = {@"Microphone: muted", @"Microphone: requesting access",
            @"Microphone: starting", @"Microphone: LIVE", @"Microphone: unavailable", @"Microphone: stopped"};
        owner->label.stringValue = labels[state];
        BOOL enableAllow = can_allow != 0 && !owner->stopped;
        BOOL enableTalk = can_talk != 0 && !owner->stopped;
        if ((enableAllow && !owner->allow.enabled) || (enableTalk && !owner->talk.enabled))
            owner->enabledAt = NSProcessInfo.processInfo.systemUptime;
        owner->allow.enabled = enableAllow; owner->talk.enabled = enableTalk;
        *shown = visible(owner) ? 1 : 0;
        return 0;
    } @catch (NSException *exception) { (void)exception; [owner stop]; return -1; }
}

int fr_mac_mic_panel_close(void *handle) {
    if (![NSThread isMainThread] || !handle) return -1;
    FrMicOwner *owner = (__bridge FrMicOwner *)handle;
    if (owner != fr_mic_owner) return -1;
    @try {
        [owner stop];
        owner->allow.target = nil; owner->talk.target = nil; owner->mute.target = nil;
        [owner->panel orderOut:nil]; [owner->panel close];
        if (owner->panel.isVisible) return -1;
        void *context = owner->context;
        FrMicRelease release = owner->release;
        owner->context = NULL; owner->action = NULL; owner->release = NULL;
        if (context && release) release(context);
        fr_mic_owner = nil;
        return 0;
    } @catch (NSException *exception) { (void)exception; return -1; }
}
