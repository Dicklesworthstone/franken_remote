// Synchronous NSPasteboard boundary for the original clipboard worker. This
// file creates no threads, dispatch blocks, observers, windows or data-provider
// callbacks. Every object is confined to its creating thread, with an explicit
// autorelease pool per call. Rust owns the retained opaque handle and callbacks
// are borrowed only for the enclosing synchronous call.
#import <AppKit/AppKit.h>
#include <pthread.h>
#include <stdint.h>
#include <string.h>

enum {
    FR_CLIP_OK = 0,
    FR_CLIP_NATIVE = -1,
    FR_CLIP_PERMISSION = -2,
    FR_CLIP_CHANGED = -3,
    FR_CLIP_NO_TEXT = -4,
    FR_CLIP_LIMIT = -5,
    FR_CLIP_DENIED = -6,
};
typedef int (*FrClipCheck)(void *);

@interface FrClipboardOwner : NSObject {
@public
    NSPasteboard *board;
    NSArray<NSPasteboardType> *types;
    NSMutableData *prepared;
    pthread_t thread;
    size_t limit;
    BOOL unique;
}
- (void)erasePrepared;
@end

@implementation FrClipboardOwner
- (void)erasePrepared {
    NSMutableData *private = prepared;
    prepared = nil;
    @try {
        if (private) {
            NSUInteger length = private.length;
            volatile uint8_t *bytes = private.mutableBytes;
            for (NSUInteger i = 0; i < length; ++i) bytes[i] = 0;
        }
    } @catch (NSException *exception) {
        // Cleanup must not unwind across the Rust FFI or run an OS restore.
        (void)exception;
    }
}
- (void)dealloc {
    [self erasePrepared];
}
@end

static FrClipboardOwner *fr_clip_owner(void *raw) {
    if (!raw) return nil;
    FrClipboardOwner *owner = (__bridge FrClipboardOwner *)raw;
    return pthread_equal(owner->thread, pthread_self()) ? owner : nil;
}

// This property is additive: older SDKs/OS versions still use the original
// pasteboard API and its typed nil/exception outcomes. No entitlement, setting,
// synthetic Paste gesture or permission prompt is bypassed here.
static BOOL fr_clip_denied(NSPasteboard *board) {
#if __MAC_OS_X_VERSION_MAX_ALLOWED >= 260000
    if (@available(macOS 26.0, *)) {
        return board.accessBehavior == NSPasteboardAccessBehaviorAlwaysDeny;
    }
#else
    (void)board;
#endif
    return NO;
}

int fr_mac_clip_open(int unique, size_t limit, void **out) {
    if (!out || !limit || (unique != 0 && unique != 1)) return FR_CLIP_NATIVE;
    *out = NULL;
    @autoreleasepool {
        @try {
            FrClipboardOwner *owner = [FrClipboardOwner new];
            if (!owner) return FR_CLIP_NATIVE;
            owner->thread = pthread_self();
            owner->limit = limit;
            owner->unique = unique != 0;
            // The private-board branch is reachable only from Rust unit tests.
            // Production always opens the general board after its live gate.
            owner->board = unique ? [NSPasteboard pasteboardWithUniqueName]
                                  : [NSPasteboard generalPasteboard];
            owner->types = @[NSPasteboardTypeString];
            if (!owner->board) return FR_CLIP_NATIVE;
            *out = (__bridge_retained void *)owner;
            return FR_CLIP_OK;
        } @catch (NSException *exception) {
            (void)exception;
            return FR_CLIP_NATIVE;
        }
    }
}

void fr_mac_clip_cancel(void *raw) {
    @autoreleasepool {
        FrClipboardOwner *owner = fr_clip_owner(raw);
        if (owner) [owner erasePrepared];
    }
}

void fr_mac_clip_close(void *raw) {
    @autoreleasepool {
        FrClipboardOwner *owner = fr_clip_owner(raw);
        if (!owner) return;
        [owner erasePrepared];
        @try {
            // Never clear, restore or release the user's general pasteboard.
            if (owner->unique) [owner->board releaseGlobally];
        } @catch (NSException *exception) {
            (void)exception;
        }
        (void)(__bridge_transfer FrClipboardOwner *)raw;
    }
}

int fr_mac_clip_observe(void *raw, int64_t *count, int *has_text) {
    if (!count || !has_text) return FR_CLIP_NATIVE;
    @autoreleasepool {
        @try {
            FrClipboardOwner *owner = fr_clip_owner(raw);
            if (!owner) return FR_CLIP_NATIVE;
            NSInteger before = owner->board.changeCount;
            // Type discovery only; no string conversion, filtering service,
            // URL/file materialization or clipboard data request is performed.
            BOOL text = [owner->board availableTypeFromArray:owner->types] != nil;
            NSInteger after = owner->board.changeCount;
            if (before < 0 || after < 0) return FR_CLIP_NATIVE;
            if (before != after) return FR_CLIP_CHANGED;
            *count = (int64_t)after;
            *has_text = text ? 1 : 0;
            return FR_CLIP_OK;
        } @catch (NSException *exception) {
            (void)exception;
            return FR_CLIP_NATIVE;
        }
    }
}

int fr_mac_clip_prepare(void *raw, const uint8_t *bytes, size_t len) {
    if (len && !bytes) return FR_CLIP_NATIVE;
    @autoreleasepool {
        @try {
            FrClipboardOwner *owner = fr_clip_owner(raw);
            if (!owner) return FR_CLIP_NATIVE;
            [owner erasePrepared];
            if (len > owner->limit) return FR_CLIP_LIMIT;
            owner->prepared = [NSMutableData dataWithLength:len];
            if (!owner->prepared) return FR_CLIP_NATIVE;
            if (len) memcpy(owner->prepared.mutableBytes, bytes, len);
            return FR_CLIP_OK;
        } @catch (NSException *exception) {
            (void)exception;
            return FR_CLIP_NATIVE;
        }
    }
}

int fr_mac_clip_read(void *raw, int64_t expected, uint8_t *bytes,
                     size_t capacity, size_t *used, FrClipCheck check, void *context) {
    if (!bytes || !used || !check || !context || expected < 0) return FR_CLIP_NATIVE;
    *used = 0;
    @autoreleasepool {
        @try {
            FrClipboardOwner *owner = fr_clip_owner(raw);
            if (!owner) return FR_CLIP_NATIVE;
            if (capacity > owner->limit) return FR_CLIP_LIMIT;
            if (!check(context)) return FR_CLIP_DENIED;
            if (owner->board.changeCount != expected) return FR_CLIP_CHANGED;
            if (fr_clip_denied(owner->board)) return FR_CLIP_PERMISSION;
            if (![owner->board availableTypeFromArray:owner->types]) return FR_CLIP_NO_TEXT;
            // Metadata/server access above may block. Check again at the actual
            // native read boundary, then discard any result if the gate lapses.
            if (!check(context)) return FR_CLIP_DENIED;
            NSData *data = [owner->board dataForType:NSPasteboardTypeString];
            if (!check(context)) return FR_CLIP_DENIED;
            if (owner->board.changeCount != expected) return FR_CLIP_CHANGED;
            if (!data) return fr_clip_denied(owner->board) ? FR_CLIP_PERMISSION : FR_CLIP_NATIVE;
            // Foundation may already have materialized an OS provider's data.
            // No app-owned copy or UTF-8 conversion precedes this byte bound.
            NSUInteger length = data.length;
            if (length > capacity) return FR_CLIP_LIMIT;
            if (!check(context)) return FR_CLIP_DENIED;
            if (length) [data getBytes:bytes length:length];
            if (!check(context)) return FR_CLIP_DENIED;
            *used = length;
            return FR_CLIP_OK;
        } @catch (NSException *exception) {
            (void)exception;
            return FR_CLIP_NATIVE;
        }
    }
}

int fr_mac_clip_declare(void *raw, int64_t expected, int64_t *count, int *attempted,
                        FrClipCheck check, void *context) {
    if (!count || !attempted || !check || !context || expected < 0) return FR_CLIP_NATIVE;
    *attempted = 0;
    @autoreleasepool {
        @try {
            FrClipboardOwner *owner = fr_clip_owner(raw);
            if (!owner || !owner->prepared) return FR_CLIP_NATIVE;
            if (!check(context)) return FR_CLIP_DENIED;
            if (owner->board.changeCount != expected) return FR_CLIP_CHANGED;
            if (!check(context)) return FR_CLIP_DENIED;
            // This is already an external mutation, even if it throws or the
            // next original-deadline check refuses. Rust retains UnknownEffect.
            *attempted = 1;
            NSInteger declared = [owner->board declareTypes:owner->types owner:nil];
            *count = (int64_t)declared;
            if (!check(context)) return FR_CLIP_DENIED;
            return declared >= 0 ? FR_CLIP_OK : FR_CLIP_NATIVE;
        } @catch (NSException *exception) {
            (void)exception;
            return FR_CLIP_NATIVE;
        }
    }
}

int fr_mac_clip_publish(void *raw, int64_t expected, int64_t *count,
                        FrClipCheck check, void *context) {
    if (!count || !check || !context || expected < 0) return FR_CLIP_NATIVE;
    @autoreleasepool {
        @try {
            FrClipboardOwner *owner = fr_clip_owner(raw);
            if (!owner || !owner->prepared) return FR_CLIP_NATIVE;
            if (!check(context)) return FR_CLIP_DENIED;
            // The OS may retain its submitted immutable value. Keep that value
            // independent from our mutable, zero-on-cancel preparation buffer.
            NSData *submitted = [owner->prepared copy];
            if (!submitted) return FR_CLIP_NATIVE;
            if (owner->board.changeCount != expected) return FR_CLIP_CHANGED;
            // Allocation and ownership queries cannot consume the last check's
            // lifetime. The callback rechecks the original host-clock deadline.
            if (!check(context)) return FR_CLIP_DENIED;
            BOOL written = [owner->board setData:submitted forType:NSPasteboardTypeString];
            if (!check(context)) return FR_CLIP_DENIED;
            if (!written) return FR_CLIP_CHANGED;
            NSInteger after = owner->board.changeCount;
            *count = (int64_t)after;
            if (!check(context)) return FR_CLIP_DENIED;
            return after == expected ? FR_CLIP_OK : FR_CLIP_CHANGED;
        } @catch (NSException *exception) {
            (void)exception;
            return FR_CLIP_NATIVE;
        }
    }
}

// Rust declares this symbol only under cfg(test). It rejects the general board
// and provides a genuine second local copy/invalid native representation for
// private-board tests without changing the user's clipboard or OS permissions.
int fr_mac_clip_test_replace(void *raw, const uint8_t *bytes, size_t len) {
    if (len && !bytes) return FR_CLIP_NATIVE;
    @autoreleasepool {
        @try {
            FrClipboardOwner *owner = fr_clip_owner(raw);
            if (!owner || !owner->unique) return FR_CLIP_NATIVE;
            NSData *data = [NSData dataWithBytes:bytes length:len];
            if (!data) return FR_CLIP_NATIVE;
            [owner->board declareTypes:owner->types owner:nil];
            return [owner->board setData:data forType:NSPasteboardTypeString]
                ? FR_CLIP_OK : FR_CLIP_CHANGED;
        } @catch (NSException *exception) {
            (void)exception;
            return FR_CLIP_NATIVE;
        }
    }
}
