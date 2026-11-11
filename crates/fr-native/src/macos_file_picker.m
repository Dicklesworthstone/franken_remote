// An explicit, modeless local selection BEFORE any remote connection exists.
// NSOpenPanel owns file browsing. Only bounded file-URL metadata crosses FFI;
// the existing Rust selection boundary opens and pins approved descriptors.
// https://developer.apple.com/documentation/appkit/nssavepanel/begin(completionhandler:)
#import <AppKit/AppKit.h>
#include <math.h>
#include <stdint.h>
#include <string.h>

enum { FR_FILE_WAIT = 0, FR_FILE_CHOSEN = 1, FR_FILE_INVALID = -1,
       FR_FILE_THREAD = -2, FR_FILE_NATIVE = -3, FR_FILE_CANCELLED = -4,
       FR_FILE_FLOOD = -5, FR_FILE_EXPIRED = -6 };
#define FR_FILE_BYTES (32 * 1024)
#define FR_FILE_PATHS 8
#define FR_FILE_EVENTS 128

@interface FrFilePickerOwner : NSObject {
@public
    NSOpenPanel *panel;
    int outcome;
    uint8_t bytes[FR_FILE_BYTES];
    size_t length;
    double completed;
    NSNotificationCenter *center;
    NSMutableArray<id> *observers;
}
- (void)complete:(NSModalResponse)result;
@end

@implementation FrFilePickerOwner
- (void)complete:(NSModalResponse)result {
    if (outcome != FR_FILE_WAIT) return;
    if (result != NSModalResponseOK) { outcome = FR_FILE_CANCELLED; return; }
    // The system chooser's positive completion is the explicit local choice.
    // No application/network callback can substitute paths after this point.
    completed = NSProcessInfo.processInfo.systemUptime;
    NSArray<NSURL *> *urls = panel.URLs;
    if (!isfinite(completed) || completed <= 0 || !urls.count || urls.count > FR_FILE_PATHS) {
        outcome = FR_FILE_INVALID;
        return;
    }
    length = 0;
    for (NSURL *url in urls) {
        if (!url.isFileURL) { outcome = FR_FILE_INVALID; return; }
        NSString *uri = url.absoluteString;
        if (!uri || uri.length > FR_FILE_BYTES ||
            [uri rangeOfCharacterFromSet:NSCharacterSet.newlineCharacterSet].location != NSNotFound) {
            outcome = FR_FILE_INVALID;
            return;
        }
        NSUInteger count = [uri lengthOfBytesUsingEncoding:NSUTF8StringEncoding];
        if (!count || count >= FR_FILE_BYTES - length) { outcome = FR_FILE_INVALID; return; }
        NSUInteger used = 0;
        if (![uri getBytes:bytes + length maxLength:count usedLength:&used
                   encoding:NSUTF8StringEncoding options:0
                      range:NSMakeRange(0, uri.length) remainingRange:NULL] || used != count) {
            outcome = FR_FILE_INVALID;
            return;
        }
        length += count;
        bytes[length++] = '\n';
    }
    outcome = FR_FILE_CHOSEN;
}
@end

int fr_mac_file_picker_open(void **out) {
    if (!out) return FR_FILE_INVALID;
    *out = NULL;
    if (![NSThread isMainThread]) return FR_FILE_THREAD;
    if (!NSApp || !NSScreen.mainScreen) return FR_FILE_NATIVE;
    @autoreleasepool {
        FrFilePickerOwner *owner = [FrFilePickerOwner new];
        owner->outcome = FR_FILE_WAIT;
        owner->panel = [NSOpenPanel openPanel];
        if (!owner->panel) return FR_FILE_NATIVE;
        owner->panel.title = @"Choose files to send";
        owner->panel.message = @"Choose up to 8 local files or folders. They will be sent only after the host grants this connection control. Cancel opens no connection.";
        owner->panel.prompt = @"Use selection";
        owner->panel.canChooseFiles = YES;
        owner->panel.canChooseDirectories = YES;
        owner->panel.allowsMultipleSelection = YES;
        owner->panel.canCreateDirectories = NO;
        owner->panel.resolvesAliases = NO;
        owner->panel.treatsFilePackagesAsDirectories = YES;
        owner->panel.releasedWhenClosed = NO;
        owner->center = NSWorkspace.sharedWorkspace.notificationCenter;
        owner->observers = [NSMutableArray arrayWithCapacity:2];
        __weak FrFilePickerOwner *weakOwner = owner;
        for (NSNotificationName name in @[NSWorkspaceWillSleepNotification,
                                          NSWorkspaceSessionDidResignActiveNotification]) {
            id observer = [owner->center addObserverForName:name object:nil
                queue:NSOperationQueue.mainQueue usingBlock:^(NSNotification *notification) {
                    (void)notification;
                    FrFilePickerOwner *live = weakOwner;
                    if (live) live->outcome = FR_FILE_CANCELLED;
                }];
            [owner->observers addObject:observer];
        }
        // This API presents a modeless panel; no runModal/nested wait prevents
        // the CLI's original signal and cancellation owner from being polled.
        [owner->panel beginWithCompletionHandler:^(NSModalResponse result) {
            FrFilePickerOwner *live = weakOwner;
            if (live) [live complete:result];
        }];
        *out = (__bridge_retained void *)owner;
    }
    return FR_FILE_WAIT;
}

int fr_mac_file_picker_poll(void *handle, uint8_t *bytes, size_t capacity,
                            size_t *length, double *age) {
    if (!handle || !bytes || capacity != FR_FILE_BYTES || !length || !age)
        return FR_FILE_INVALID;
    *length = 0;
    *age = 0;
    if (![NSThread isMainThread]) return FR_FILE_THREAD;
    FrFilePickerOwner *owner = (__bridge FrFilePickerOwner *)handle;
    @autoreleasepool {
        size_t count = 0;
        while (count < FR_FILE_EVENTS && owner->outcome >= FR_FILE_WAIT) {
            NSEvent *event = [NSApp nextEventMatchingMask:NSEventMaskAny
                untilDate:[NSDate distantPast] inMode:NSDefaultRunLoopMode dequeue:YES];
            if (!event) break;
            ++count;
            [NSApp sendEvent:event];
        }
        // Never export a selection while a bounded turn could still hide a
        // pending lifecycle veto. The original owner must be closed instead.
        if (count == FR_FILE_EVENTS) owner->outcome = FR_FILE_FLOOD;
        if (owner->outcome >= FR_FILE_WAIT) [NSApp updateWindows];
        if (owner->outcome == FR_FILE_CHOSEN) {
            double now = NSProcessInfo.processInfo.systemUptime;
            if (!isfinite(now) || now < owner->completed || now - owner->completed >= 2.0) {
                owner->outcome = FR_FILE_EXPIRED;
                return owner->outcome;
            }
            if (!owner->length || owner->length > capacity) return FR_FILE_INVALID;
            memcpy(bytes, owner->bytes, owner->length);
            *length = owner->length;
            *age = now - owner->completed;
        }
        return owner->outcome;
    }
}

int fr_mac_file_picker_close(void *handle) {
    if (!handle) return FR_FILE_INVALID;
    if (![NSThread isMainThread]) return FR_FILE_THREAD;
    @autoreleasepool {
        FrFilePickerOwner *owner = (__bridge FrFilePickerOwner *)handle;
        // Fence the weak completion callback BEFORE invoking AppKit teardown.
        owner->outcome = FR_FILE_CANCELLED;
        for (id observer in owner->observers) [owner->center removeObserver:observer];
        [owner->observers removeAllObjects];
        [owner->panel cancel:nil];
        [owner->panel orderOut:nil];
        [owner->panel close];
        if (owner->panel.isVisible) return FR_FILE_NATIVE;
        owner->length = 0;
        memset(owner->bytes, 0, sizeof(owner->bytes));
        owner->panel = nil;
        (void)(__bridge_transfer FrFilePickerOwner *)handle;
    }
    return FR_FILE_WAIT;
}
