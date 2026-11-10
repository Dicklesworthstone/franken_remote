// Read-only console-session and sleep evidence. No private lock keys, screen
// capture, input posting, clipboard, permission prompts or background service.
// Positive state comes from fresh public Security/CoreGraphics APIs; AppKit
// notifications are terminal negative signals only. On-console/login-done and
// protected-data availability are NOT a graphical screen-unlocked guarantee.
#import <AppKit/AppKit.h>
#import <CoreGraphics/CoreGraphics.h>
#import <Security/AuthSession.h>
#include <pthread.h>
#include <stdatomic.h>
#include <stdint.h>
#include <unistd.h>

enum {
    FR_MS_OK = 0,
    FR_MS_INACTIVE = 3,
    FR_MS_SUSPENDING = 4,
    FR_MS_SESSION_UNAVAILABLE = 5,
    FR_MS_IDENTITY_CHANGED = 6,
    FR_MS_SERVICE_UNAVAILABLE = 7,
    FR_MS_UNSUPPORTED = 8,
    FR_MS_FAILED = 10,
};

typedef struct {
    uint32_t uid;
    uint32_t security_session;
    uint32_t console_set;
    uint32_t flags; // on-console, login-done, graphic access, protected data, awake display
} FrSessionSnapshot;

@interface FrSessionMonitor : NSObject {
  @public
    atomic_int stopped;
    uint32_t uid;
}
@property(nonatomic, strong) NSNotificationCenter *workspace;
- (void)stop:(int)reason;
- (void)inactive:(NSNotification *)notification;
- (void)suspending:(NSNotification *)notification;
- (void)ending:(NSNotification *)notification;
@end

@implementation FrSessionMonitor
- (instancetype)init {
    self = [super init];
    if (self) atomic_init(&stopped, FR_MS_OK);
    return self;
}
- (void)stop:(int)reason {
    int active = FR_MS_OK;
    atomic_compare_exchange_strong(&stopped, &active, reason);
}
- (void)inactive:(NSNotification *)notification {
    (void)notification;
    [self stop:FR_MS_INACTIVE];
}
- (void)suspending:(NSNotification *)notification {
    (void)notification;
    [self stop:FR_MS_SUSPENDING];
}
- (void)ending:(NSNotification *)notification {
    (void)notification;
    // Protected-data loss and logout both stop, without calling either a lock.
    [self stop:FR_MS_SESSION_UNAVAILABLE];
}
- (void)dealloc {
    // No Objective-C exception may unwind through the Rust owner destructor.
    @try {
        [_workspace removeObserver:self];
    } @catch (NSException *exception) {
        (void)exception;
    }
    @try {
        [[NSNotificationCenter defaultCenter] removeObserver:self];
    } @catch (NSException *exception) {
        (void)exception;
    }
}
@end

// Main-thread only. A native object must be retired before another can open.
static BOOL occupied;

static int security_identity(SecuritySessionId *session) {
    SessionAttributeBits attributes = 0;
    if (SessionGetInfo(callerSecuritySession, session, &attributes) != errSessionSuccess)
        return FR_MS_SERVICE_UNAVAILABLE;
    if (*session == noSecuritySession || *session >= UINT32_MAX - 1 ||
        (attributes & sessionHasGraphicAccess) == 0 ||
        (attributes & (sessionIsRoot | sessionIsRemote)) != 0)
        return FR_MS_UNSUPPORTED;
    return FR_MS_OK;
}

static BOOL number(CFDictionaryRef dictionary, CFStringRef key, uint32_t *out) {
    CFTypeRef value = CFDictionaryGetValue(dictionary, key);
    int64_t integer = 0;
    if (!value || CFGetTypeID(value) != CFNumberGetTypeID() ||
        !CFNumberGetValue((CFNumberRef)value, kCFNumberSInt64Type, &integer) ||
        integer < 0 || integer > UINT32_MAX)
        return NO;
    *out = (uint32_t)integer;
    return YES;
}

static BOOL boolean(CFDictionaryRef dictionary, CFStringRef key, BOOL *out) {
    CFTypeRef value = CFDictionaryGetValue(dictionary, key);
    if (!value || CFGetTypeID(value) != CFBooleanGetTypeID()) return NO;
    *out = CFBooleanGetValue((CFBooleanRef)value);
    return YES;
}

static int snapshot(FrSessionMonitor *owner, FrSessionSnapshot *out) {
    int terminal = atomic_load(&owner->stopped);
    if (terminal != FR_MS_OK) return terminal;
    if (getuid() != owner->uid || geteuid() != owner->uid) return FR_MS_IDENTITY_CHANGED;
    SecuritySessionId first = noSecuritySession, last = noSecuritySession;
    int status = security_identity(&first);
    if (status != FR_MS_OK) return status;
    CFDictionaryRef session = CGSessionCopyCurrentDictionary();
    if (!session) return FR_MS_SESSION_UNAVAILABLE;
    FrSessionSnapshot result = {0};
    BOOL on_console = NO, logged_in = NO;
    BOOL complete = number(session, kCGSessionUserIDKey, &result.uid) &&
                    boolean(session, kCGSessionOnConsoleKey, &on_console) &&
                    boolean(session, kCGSessionLoginDoneKey, &logged_in);
    // macOS 27.0.1 no longer reports a console set. Bind it when present; an
    // absent key reads as UINT32_MAX, so a key that appears, disappears or
    // changes within an epoch is still an identity change.
    result.console_set = UINT32_MAX;
    if (complete && CFDictionaryContainsKey(session, kCGSessionConsoleSetKey))
        complete = number(session, kCGSessionConsoleSetKey, &result.console_set);
    CFRelease(session);
    if (!complete) return FR_MS_SERVICE_UNAVAILABLE;
    if (result.uid != owner->uid) return FR_MS_IDENTITY_CHANGED;
    status = security_identity(&last);
    if (status != FR_MS_OK) return status;
    if (first != last) return FR_MS_IDENTITY_CHANGED;
    // Subscribe to display sleep and also sample current state: the helper may
    // start after the sleep notification. This is display-power evidence only,
    // never proof that the graphical session is unlocked.
    CGDirectDisplayID display = CGMainDisplayID();
    if (display == kCGNullDirectDisplay || !CGDisplayIsOnline(display))
        return FR_MS_SESSION_UNAVAILABLE;
    BOOL display_awake = !CGDisplayIsAsleep(display);
    if (@available(macOS 12.0, *)) {
        result.security_session = first;
        result.flags = (on_console ? 1u : 0u) | (logged_in ? 2u : 0u) | 4u |
                       (NSApp.isProtectedDataAvailable ? 8u : 0u) |
                       (display_awake ? 16u : 0u);
    } else {
        return FR_MS_UNSUPPORTED;
    }
    terminal = atomic_load(&owner->stopped);
    if (terminal != FR_MS_OK) return terminal;
    *out = result;
    return FR_MS_OK;
}

int fr_ms_open(uint32_t uid, void **out) {
    if (!out) return FR_MS_FAILED;
    *out = NULL;
    if (!pthread_main_np() || occupied || uid == 0 || getuid() != uid || geteuid() != uid)
        return FR_MS_UNSUPPORTED;
    SecuritySessionId session = noSecuritySession;
    int status = security_identity(&session);
    if (status != FR_MS_OK) return status;
    @autoreleasepool {
        @try {
            if (@available(macOS 12.0, *)) {
                [NSApplication sharedApplication];
                if (!NSApp) return FR_MS_UNSUPPORTED;
                // This unbundled helper already starts Prohibited, and AppKit
                // answers NO to a switch that changes nothing (macOS 27.0.1).
                // Require the resulting policy, not the call's answer.
                (void)[NSApp setActivationPolicy:NSApplicationActivationPolicyProhibited];
                if (NSApp.activationPolicy != NSApplicationActivationPolicyProhibited)
                    return FR_MS_UNSUPPORTED;
                FrSessionMonitor *owner = [[FrSessionMonitor alloc] init];
                if (!owner) return FR_MS_FAILED;
                owner->uid = uid;
                owner.workspace = [NSWorkspace sharedWorkspace].notificationCenter;
                if (!owner.workspace) return FR_MS_SERVICE_UNAVAILABLE;
                [owner.workspace addObserver:owner selector:@selector(inactive:)
                                         name:NSWorkspaceSessionDidResignActiveNotification object:nil];
                [owner.workspace addObserver:owner selector:@selector(suspending:)
                                         name:NSWorkspaceWillSleepNotification object:nil];
                [owner.workspace addObserver:owner selector:@selector(suspending:)
                                         name:NSWorkspaceScreensDidSleepNotification object:nil];
                [owner.workspace addObserver:owner selector:@selector(ending:)
                                         name:NSWorkspaceWillPowerOffNotification object:nil];
                [[NSNotificationCenter defaultCenter]
                    addObserver:owner selector:@selector(ending:)
                           name:NSApplicationProtectedDataWillBecomeUnavailableNotification
                         object:NSApp];
                // Inactive-session launch notifications occur during this
                // transition. Subscribe before it; no positive event is used.
                [NSApp finishLaunching];
                occupied = YES;
                *out = (__bridge_retained void *)owner;
                return FR_MS_OK;
            }
            return FR_MS_UNSUPPORTED;
        } @catch (NSException *exception) {
            (void)exception;
            return FR_MS_FAILED;
        }
    }
}

int fr_ms_snapshot(void *value, FrSessionSnapshot *out) {
    if (!pthread_main_np() || !value || !out) return FR_MS_FAILED;
    @autoreleasepool {
        FrSessionMonitor *owner = (__bridge FrSessionMonitor *)value;
        @try {
            int status = snapshot(owner, out);
            if (status != FR_MS_OK) [owner stop:status];
            return status;
        } @catch (NSException *exception) {
            (void)exception;
            [owner stop:FR_MS_FAILED];
            return FR_MS_FAILED;
        }
    }
}

int fr_ms_pump(void *value, uint32_t timeout_ms) {
    if (!pthread_main_np() || !value || timeout_ms > 50) return FR_MS_FAILED;
    @autoreleasepool {
        FrSessionMonitor *owner = (__bridge FrSessionMonitor *)value;
        @try {
            NSDate *until = [NSDate dateWithTimeIntervalSinceNow:(double)timeout_ms / 1000.0];
            // The responder cannot refresh evidence; a blocked turn or a flood
            // still expires the original native read in the other thread.
            for (unsigned n = 0; n < 64 && atomic_load(&owner->stopped) == FR_MS_OK; n++) {
                NSEvent *event = [NSApp nextEventMatchingMask:NSEventMaskAny untilDate:until
                                                    inMode:NSDefaultRunLoopMode dequeue:YES];
                if (!event) break;
                [NSApp sendEvent:event];
                if ([until timeIntervalSinceNow] <= 0) break;
            }
            return atomic_load(&owner->stopped);
        } @catch (NSException *exception) {
            (void)exception;
            [owner stop:FR_MS_FAILED];
            return FR_MS_FAILED;
        }
    }
}

void fr_ms_close(void *value) {
    if (!value || !pthread_main_np()) return;
    @autoreleasepool {
        FrSessionMonitor *owner = (__bridge_transfer FrSessionMonitor *)value;
        @try {
            [owner stop:FR_MS_FAILED];
            [owner.workspace removeObserver:owner];
            [[NSNotificationCenter defaultCenter] removeObserver:owner];
        } @catch (NSException *exception) {
            (void)exception;
        }
        occupied = NO;
    }
}
