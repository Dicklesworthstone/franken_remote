// UIKit stays on the main queue. The caller is the original isolated clipboard
// worker and keeps every buffer and guard alive until dispatch_sync returns.
// No abandoned async block can read or publish after native cleanup completes.
#import <UIKit/UIKit.h>
#include <stdbool.h>
#include <stdint.h>
#include <stddef.h>

typedef bool (*FrClipboardGate)(const void *);
typedef struct {
    uint64_t count;
    uint32_t text;
    uint32_t own;
} FrClipboardMetadata;
_Static_assert(sizeof(FrClipboardMetadata) == 16, "clipboard metadata size");
_Static_assert(offsetof(FrClipboardMetadata, text) == 8, "clipboard metadata layout");

enum { FR_CLIP_OK = 0, FR_CLIP_UNAVAILABLE = 1, FR_CLIP_NO_TEXT = 2,
       FR_CLIP_LIMIT = 3, FR_CLIP_CHANGED = 4 };

// Native foreground evidence supplements the original SwiftUI privacy owner;
// it does not identify a session by private hosting-controller structure. The
// app fences that owner before presenting its own modal controls.
static bool foreground(void) {
    UIApplication *app = UIApplication.sharedApplication;
    if (app.applicationState != UIApplicationStateActive || app.connectedScenes.count > 8)
        return false;
    UIWindow *key = nil;
    NSUInteger activeScenes = 0;
    for (UIScene *scene in app.connectedScenes) {
        if (scene.activationState != UISceneActivationStateForegroundActive) continue;
        if (++activeScenes != 1 || ![scene isKindOfClass:UIWindowScene.class]) return false;
        NSArray<UIWindow *> *windows = ((UIWindowScene *)scene).windows;
        if (windows.count > 16) return false;
        for (UIWindow *window in windows) {
            if (!window.isKeyWindow) continue;
            if (key || window.hidden || window.alpha <= 0) return false;
            key = window;
        }
    }
    return activeScenes == 1 && key && key.rootViewController;
}

static bool permitted(FrClipboardGate gate, const void *context) {
    return [NSThread isMainThread] && foreground() && gate && gate(context);
}

static NSString *marker(const char *value) {
    return value ? [[NSString alloc] initWithUTF8String:value] : nil;
}

static bool metadata(UIPasteboard *board, NSString *owned, FrClipboardMetadata *out) {
    NSInteger count = board.changeCount;
    if (count < 0) return false;
    out->count = (uint64_t)count;
    out->text = board.hasStrings;
    out->own = owned && [board.pasteboardTypes containsObject:owned];
    return true;
}

static bool same(FrClipboardMetadata a, FrClipboardMetadata b) {
    return a.count == b.count && a.text == b.text && a.own == b.own;
}

static void main_call(void (^operation)(void)) {
    if ([NSThread isMainThread]) operation();
    else dispatch_sync(dispatch_get_main_queue(), operation);
}

int fr_ios_clipboard_metadata(const char *owned, FrClipboardGate gate,
                             const void *context, FrClipboardMetadata *out) {
    if (!gate || !context || !out) return FR_CLIP_UNAVAILABLE;
    __block int result = FR_CLIP_UNAVAILABLE;
    main_call(^{ @autoreleasepool { @try {
        if (!permitted(gate, context)) return;
        NSString *type = marker(owned);
        if (owned && !type) return;
        FrClipboardMetadata value = {0};
        if (metadata(UIPasteboard.generalPasteboard, type, &value) && permitted(gate, context)) {
            *out = value;
            result = FR_CLIP_OK;
        }
    } @catch (__unused NSException *exception) {} } });
    return result;
}

int fr_ios_clipboard_read(const char *owned, FrClipboardMetadata expected,
                         FrClipboardGate gate, const void *context,
                         uint8_t *out, size_t capacity, size_t *length) {
    if (!gate || !context || !out || !length || capacity == 0 || capacity > 1048576)
        return FR_CLIP_UNAVAILABLE;
    *length = 0;
    __block int result = FR_CLIP_UNAVAILABLE;
    main_call(^{ @autoreleasepool { @try {
        NSString *type = marker(owned);
        if ((owned && !type) || !permitted(gate, context)) return;
        UIPasteboard *board = UIPasteboard.generalPasteboard;
        FrClipboardMetadata before = {0}, after = {0};
        if (!metadata(board, type, &before) || !same(before, expected)) {
            result = FR_CLIP_CHANGED; return;
        }
        // This is the only content read. iOS controls its cross-app paste
        // permission; denial never becomes empty successful clipboard text.
        if (!permitted(gate, context)) return;
        NSString *value = board.string;
        if (!value) { result = FR_CLIP_NO_TEXT; return; }
        if (value.length > capacity) { result = FR_CLIP_LIMIT; return; }
        NSUInteger used = 0;
        NSRange remaining = NSMakeRange(0, value.length);
        BOOL complete = [value getBytes:out maxLength:capacity usedLength:&used
                              encoding:NSUTF8StringEncoding options:0
                                 range:NSMakeRange(0, value.length) remainingRange:&remaining];
        if ((!complete && value.length != 0) || remaining.length != 0 || used > capacity) {
            result = FR_CLIP_LIMIT; return;
        }
        if (!metadata(board, type, &after) || !same(before, after)) {
            result = FR_CLIP_CHANGED; return;
        }
        if (!permitted(gate, context)) return;
        *length = used;
        result = FR_CLIP_OK;
    } @catch (__unused NSException *exception) {} } });
    return result;
}

int fr_ios_clipboard_write(const char *owned, const char *next,
                          FrClipboardMetadata expected, const uint8_t *text, size_t length,
                          FrClipboardGate gate, const void *context,
                          FrClipboardMetadata *out, uint32_t *submitted) {
    if (!next || !text || length > 1048576 || !gate || !context || !out || !submitted)
        return FR_CLIP_UNAVAILABLE;
    *submitted = 0;
    __block int result = FR_CLIP_UNAVAILABLE;
    main_call(^{ @autoreleasepool { @try {
        NSString *oldType = marker(owned), *newType = marker(next);
        NSString *value = [[NSString alloc] initWithBytes:text length:length encoding:NSUTF8StringEncoding];
        if (!newType || !value || (owned && !oldType) || !permitted(gate, context)) return;
        UIPasteboard *board = UIPasteboard.generalPasteboard;
        FrClipboardMetadata before = {0};
        if (!metadata(board, oldType, &before) || !same(before, expected)) {
            result = FR_CLIP_CHANGED; return;
        }
        const uint8_t provenance = 1;
        NSDictionary *item = @{ @"public.utf8-plain-text": value,
                                newType: [NSData dataWithBytes:&provenance length:1] };
        // This final guard retains the original item's host-clock deadline.
        // No UI callback, wait, string conversion or metadata query follows it
        // before the OS mutation. Even an exception may have changed the board.
        if (!permitted(gate, context)) return;
        *submitted = 1;
        [board setItems:@[item] options:@{ UIPasteboardOptionLocalOnly: @YES }];
        if (metadata(board, newType, out) && permitted(gate, context)) result = FR_CLIP_OK;
    } @catch (__unused NSException *exception) {} } });
    return result;
}
