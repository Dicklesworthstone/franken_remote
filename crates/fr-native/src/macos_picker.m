// Bounded, nonmodal AppKit host/display choices. Rust retains the actual
// immutable catalog; this owner copies labels and returns one row ordinal.
// All methods/callbacks run on the main thread. There are no dispatch blocks,
// worker threads, Rust pointers, network handles, media calls or modal loops.
#import <AppKit/AppKit.h>
#include <math.h>
#include <stdint.h>

enum {
    FR_PICK_WAIT = 0,
    FR_PICK_CHOSEN = 1,
    FR_PICK_INVALID = -1,
    FR_PICK_THREAD = -2,
    FR_PICK_NATIVE = -3,
    FR_PICK_CANCELLED = -4,
    FR_PICK_FLOOD = -5,
};
#define FR_PICK_ROWS 1024
#define FR_PICK_LABEL 384
#define FR_PICK_EVENTS 128

// Must match RawRow in macos/picker.rs. Each byte range is borrowed only for
// fr_mac_picker_open; NSString makes its own immutable copy before return.
typedef struct {
    const char *title;
    size_t title_len;
    const char *detail;
    size_t detail_len;
} FrPickerRow;

@interface FrPickerCell : NSTableCellView
@property(strong) NSTextField *detail;
@end
@implementation FrPickerCell
- (instancetype)initWithFrame:(NSRect)frame {
    self = [super initWithFrame:frame];
    if (!self) return nil;
    self.textField = [NSTextField labelWithString:@""];
    self.textField.font = [NSFont systemFontOfSize:13];
    self.textField.frame = NSMakeRect(8, 22, fmax(1, frame.size.width - 16), 18);
    self.textField.autoresizingMask = NSViewWidthSizable;
    self.textField.lineBreakMode = NSLineBreakByClipping;
    [self addSubview:self.textField];
    self.detail = [NSTextField labelWithString:@""];
    self.detail.font = [NSFont systemFontOfSize:11];
    self.detail.textColor = NSColor.secondaryLabelColor;
    self.detail.frame = NSMakeRect(8, 4, fmax(1, frame.size.width - 16), 16);
    self.detail.autoresizingMask = NSViewWidthSizable;
    self.detail.lineBreakMode = NSLineBreakByClipping;
    [self addSubview:self.detail];
    return self;
}
@end

@interface FrPickerOwner : NSObject <NSWindowDelegate, NSTableViewDataSource, NSTableViewDelegate> {
@public
    NSWindow *window;
    NSTableView *table;
    NSScrollView *scroll;
    NSButton *accept;
    NSButton *cancel;
    NSArray<NSString *> *titles;
    NSArray<NSString *> *details;
    int outcome;
    NSUInteger chosen;
    double validAfter;
}
- (BOOL)fresh:(NSEvent *)event;
- (void)choose:(id)sender;
- (void)cancel:(id)sender;
- (void)input:(NSEvent *)event;
@end

static BOOL fr_picker_hit(NSView *view, NSEvent *event) {
    if (!view || view.hidden) return NO;
    return NSPointInRect([view convertPoint:event.locationInWindow fromView:nil], view.bounds);
}

@implementation FrPickerOwner
- (NSInteger)numberOfRowsInTableView:(NSTableView *)view {
    (void)view;
    return (NSInteger)titles.count;
}
- (NSView *)tableView:(NSTableView *)view viewForTableColumn:(NSTableColumn *)column row:(NSInteger)row {
    (void)column;
    if (row < 0 || (NSUInteger)row >= titles.count) return nil;
    FrPickerCell *cell = (FrPickerCell *)[view makeViewWithIdentifier:@"choice" owner:nil];
    if (!cell) {
        cell = [[FrPickerCell alloc] initWithFrame:NSMakeRect(0, 0, view.bounds.size.width, 44)];
        cell.identifier = @"choice";
    }
    cell.textField.stringValue = titles[(NSUInteger)row];
    cell.detail.stringValue = details[(NSUInteger)row];
    return cell;
}
- (void)tableViewSelectionDidChange:(NSNotification *)notification {
    (void)notification;
    accept.enabled = outcome == FR_PICK_WAIT && table.selectedRow >= 0;
}
- (BOOL)windowShouldClose:(NSWindow *)sender {
    (void)sender;
    outcome = FR_PICK_CANCELLED;
    return NO; // The retained Rust owner performs the actual close.
}
- (void)windowDidResignKey:(NSNotification *)notification {
    (void)notification;
    [table deselectAll:nil];
    validAfter = NSProcessInfo.processInfo.systemUptime;
}
- (void)windowDidBecomeKey:(NSNotification *)notification {
    (void)notification;
    validAfter = NSProcessInfo.processInfo.systemUptime;
}
- (BOOL)fresh:(NSEvent *)event {
    double now = NSProcessInfo.processInfo.systemUptime;
    return event && event.window == window && window.isKeyWindow && window.isVisible && !window.isMiniaturized &&
           window.isOnActiveSpace && NSApp.isActive && isfinite(event.timestamp) &&
           event.timestamp >= validAfter && event.timestamp <= now &&
           now - event.timestamp < 0.250;
}
- (void)choose:(id)sender {
    (void)sender;
    // Native controls can also dispatch an action outside our poll input
    // helper. They still need one current, focused local event, never merely
    // a selected row or a queued key held from the preceding window.
    NSEvent *event = NSApp.currentEvent;
    if (![self fresh:event] || outcome != FR_PICK_WAIT ||
        (event.type == NSEventTypeKeyDown && event.isARepeat)) return;
    NSInteger row = table.selectedRow;
    if (row < 0 || (NSUInteger)row >= titles.count) return;
    chosen = (NSUInteger)row;
    outcome = FR_PICK_CHOSEN;
    accept.enabled = NO;
}
- (void)cancel:(id)sender {
    (void)sender;
    // Cancellation is deliberately independent of age, focus or readiness.
    outcome = FR_PICK_CANCELLED;
}
- (void)input:(NSEvent *)event {
    // Dispatch our input directly, instead of entering NSControl, scroller,
    // table drag or title-bar tracking loops that can wait for a held mouse.
    if (event.type == NSEventTypeKeyDown && event.keyCode == 53) {
        [self cancel:nil];
        return;
    }
    if (event.type == NSEventTypeLeftMouseDown &&
        (fr_picker_hit(cancel, event) ||
         fr_picker_hit([window standardWindowButton:NSWindowCloseButton], event))) {
        [self cancel:nil];
        return;
    }
    if (outcome != FR_PICK_WAIT || ![self fresh:event]) return;
    if (event.type == NSEventTypeLeftMouseDown) {
        if (fr_picker_hit(accept, event)) {
            NSInteger row = table.selectedRow;
            if (accept.enabled && row >= 0 && (NSUInteger)row < titles.count) {
                chosen = (NSUInteger)row;
                outcome = FR_PICK_CHOSEN;
                accept.enabled = NO;
            }
            return;
        }
        if (!fr_picker_hit(scroll.contentView, event)) return;
        NSPoint point = [table convertPoint:event.locationInWindow fromView:nil];
        NSInteger row = [table rowAtPoint:point];
        if (row < 0 || (NSUInteger)row >= titles.count) return;
        [table selectRowIndexes:[NSIndexSet indexSetWithIndex:(NSUInteger)row] byExtendingSelection:NO];
        [window makeFirstResponder:table];
        // A second current click is an explicit confirmation of this row.
        if (event.clickCount == 2) {
            chosen = (NSUInteger)row;
            outcome = FR_PICK_CHOSEN;
            accept.enabled = NO;
        }
        return;
    }
    if (event.type == NSEventTypeScrollWheel && fr_picker_hit(scroll.contentView, event)) {
        NSClipView *clip = scroll.contentView;
        NSPoint point = clip.bounds.origin;
        double scale = event.hasPreciseScrollingDeltas ? 1 : 24;
        double dx = event.scrollingDeltaX * scale;
        double dy = event.scrollingDeltaY * scale;
        if (!isfinite(dx) || !isfinite(dy)) return;
        point.x = fmax(0, fmin(table.bounds.size.width - clip.bounds.size.width, point.x - dx));
        point.y = fmax(0, fmin(table.bounds.size.height - clip.bounds.size.height, point.y - dy));
        [clip scrollToPoint:point];
        [scroll reflectScrolledClipView:clip];
        return;
    }
    if (event.type != NSEventTypeKeyDown || event.isARepeat ||
        (event.modifierFlags & (NSEventModifierFlagCommand | NSEventModifierFlagControl |
                               NSEventModifierFlagOption))) return;
    if (event.keyCode == 123 || event.keyCode == 124) {
        NSClipView *clip = scroll.contentView;
        NSPoint point = clip.bounds.origin;
        double offset = event.keyCode == 123 ? -160 : 160;
        point.x = fmax(0, fmin(table.bounds.size.width - clip.bounds.size.width, point.x + offset));
        [clip scrollToPoint:point];
        [scroll reflectScrolledClipView:clip];
        return;
    }
    NSInteger row = table.selectedRow;
    switch (event.keyCode) {
        case 36: case 76: // Return/keypad Enter confirm an explicitly selected row.
            if (row >= 0 && (NSUInteger)row < titles.count) {
                chosen = (NSUInteger)row;
                outcome = FR_PICK_CHOSEN;
                accept.enabled = NO;
            }
            return;
        case 125: row = row < 0 ? 0 : MIN(row + 1, (NSInteger)titles.count - 1); break;
        case 126: row = row < 0 ? (NSInteger)titles.count - 1 : MAX(row - 1, 0); break;
        case 115: row = 0; break; // Home
        case 119: row = (NSInteger)titles.count - 1; break; // End
        case 116: row = MAX(row - 8, 0); break; // Page Up
        case 121: row = MIN(MAX(row, 0) + 8, (NSInteger)titles.count - 1); break;
        default: return;
    }
    [table selectRowIndexes:[NSIndexSet indexSetWithIndex:(NSUInteger)row] byExtendingSelection:NO];
    [table scrollRowToVisible:row];
}
@end

static NSString *fr_picker_string(const char *bytes, size_t length) {
    if (!bytes || !length || length > FR_PICK_LABEL) return nil;
    for (size_t i = 0; i < length; ++i)
        if ((unsigned char)bytes[i] < 32 || (unsigned char)bytes[i] > 126) return nil;
    return [[NSString alloc] initWithBytes:bytes length:length encoding:NSASCIIStringEncoding];
}

int fr_mac_picker_open(uint32_t kind, const FrPickerRow *rows, size_t count, void **out) {
    if (!out) return FR_PICK_INVALID;
    *out = NULL;
    if ((kind != 1 && kind != 2) || !rows || !count || count > FR_PICK_ROWS ||
        (kind == 2 && count > 8)) return FR_PICK_INVALID;
    if (![NSThread isMainThread]) return FR_PICK_THREAD;
    if (!NSApp || !NSScreen.mainScreen) return FR_PICK_NATIVE;
    @autoreleasepool {
        NSMutableArray<NSString *> *titles = [NSMutableArray arrayWithCapacity:count];
        NSMutableArray<NSString *> *details = [NSMutableArray arrayWithCapacity:count];
        CGFloat columnWidth = 680;
        for (size_t i = 0; i < count; ++i) {
            NSString *title = fr_picker_string(rows[i].title, rows[i].title_len);
            NSString *detail = fr_picker_string(rows[i].detail, rows[i].detail_len);
            if (!title || !detail) return FR_PICK_INVALID;
            [titles addObject:title];
            [details addObject:detail];
            columnWidth = fmax(columnWidth, 24 + [title sizeWithAttributes:@{NSFontAttributeName:[NSFont systemFontOfSize:13]}].width);
            columnWidth = fmax(columnWidth, 24 + [detail sizeWithAttributes:@{NSFontAttributeName:[NSFont systemFontOfSize:11]}].width);
        }
        if (!isfinite(columnWidth) || columnWidth > 8192) return FR_PICK_INVALID;
        NSRect available = NSScreen.mainScreen.visibleFrame;
        CGFloat width = fmin(760, available.size.width - 40);
        CGFloat height = fmin(520, available.size.height - 60);
        if (width < 360 || height < 240) return FR_PICK_NATIVE;
        FrPickerOwner *owner = [FrPickerOwner new];
        owner->titles = [titles copy];
        owner->details = [details copy];
        owner->outcome = FR_PICK_WAIT;
        owner->chosen = NSNotFound;
        owner->validAfter = NSProcessInfo.processInfo.systemUptime;
        owner->window = [[NSWindow alloc] initWithContentRect:NSMakeRect(0, 0, width, height)
            styleMask:NSWindowStyleMaskTitled | NSWindowStyleMaskClosable
            backing:NSBackingStoreBuffered defer:NO];
        NSWindow *window = owner->window;
        if (!window) return FR_PICK_NATIVE;
        window.releasedWhenClosed = NO;
        window.movable = NO;
        window.delegate = owner;
        window.title = kind == 1 ? @"Choose a tailnet host" : @"Choose a remote display";
        NSView *content = window.contentView;
        NSTextField *heading = [NSTextField labelWithString:window.title];
        heading.font = [NSFont boldSystemFontOfSize:17];
        heading.frame = NSMakeRect(20, height - 42, width - 40, 24);
        [content addSubview:heading];
        NSTextField *subtitle = [NSTextField labelWithString:kind == 1
            ? @"Choose a machine; its identity will be checked before connecting."
            : @"Choose one display from this approved session."];
        subtitle.frame = NSMakeRect(20, height - 68, width - 40, 20);
        subtitle.font = [NSFont systemFontOfSize:11];
        [content addSubview:subtitle];
        owner->scroll = [[NSScrollView alloc] initWithFrame:NSMakeRect(20, 78, width - 40, height - 160)];
        owner->scroll.borderType = NSBezelBorder;
        // Wheel/trackpad and keyboard scrolling are handled as bounded events,
        // never a scroller's mouse-tracking loop. Wide labels stay intact and
        // are horizontally scrollable instead of silently changing identity.
        owner->scroll.hasVerticalScroller = NO;
        owner->scroll.hasHorizontalScroller = NO;
        owner->table = [[NSTableView alloc] initWithFrame:NSMakeRect(0, 0, columnWidth, count * 44)];
        NSTableView *table = owner->table;
        table.headerView = nil;
        table.rowHeight = 44;
        table.intercellSpacing = NSMakeSize(0, 0);
        table.allowsMultipleSelection = NO;
        table.allowsEmptySelection = YES;
        table.usesAlternatingRowBackgroundColors = YES;
        table.dataSource = owner;
        table.delegate = owner;
        NSTableColumn *column = [[NSTableColumn alloc] initWithIdentifier:@"choice"];
        column.minWidth = columnWidth;
        column.maxWidth = columnWidth;
        column.width = columnWidth;
        [table addTableColumn:column];
        owner->scroll.documentView = table;
        [content addSubview:owner->scroll];
        NSTextField *hint = [NSTextField labelWithString:[NSString stringWithFormat:
            @"%zu %@. Scroll or use arrows; Return confirms, Escape cancels.", count,
            kind == 1 ? @"hosts" : @"displays"]];
        hint.font = [NSFont systemFontOfSize:10];
        hint.textColor = NSColor.secondaryLabelColor;
        hint.frame = NSMakeRect(20, 54, width - 40, 18);
        [content addSubview:hint];
        owner->accept = [NSButton buttonWithTitle:kind == 1 ? @"Connect" : @"View display"
                                          target:owner action:@selector(choose:)];
        owner->accept.frame = NSMakeRect(width - 150, 15, 130, 30);
        owner->accept.enabled = NO;
        [content addSubview:owner->accept];
        owner->cancel = [NSButton buttonWithTitle:@"Cancel" target:owner action:@selector(cancel:)];
        owner->cancel.frame = NSMakeRect(width - 260, 15, 100, 30);
        [content addSubview:owner->cancel];
        [table reloadData];
        [table deselectAll:nil];
        [window makeFirstResponder:table];
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
        *out = (__bridge_retained void *)owner;
    }
    return FR_PICK_WAIT;
}

int fr_mac_picker_poll(void *handle, size_t *row) {
    if (!handle || !row) return FR_PICK_INVALID;
    *row = SIZE_MAX;
    if (![NSThread isMainThread]) return FR_PICK_THREAD;
    FrPickerOwner *owner = (__bridge FrPickerOwner *)handle;
    @autoreleasepool {
        size_t count = 0;
        while (count < FR_PICK_EVENTS && owner->outcome >= FR_PICK_WAIT) {
            NSEvent *event = [NSApp nextEventMatchingMask:NSEventMaskAny
                untilDate:[NSDate distantPast] inMode:NSDefaultRunLoopMode dequeue:YES];
            if (!event) break;
            ++count; // Include ignored/application/foreign-window events.
            if (event.window == owner->window) {
                // A first click can focus the picker but cannot also select
                // a row using an event from before the new focus boundary.
                if (event.type == NSEventTypeLeftMouseDown &&
                    (!owner->window.isKeyWindow || !NSApp.isActive)) {
                    [owner->window makeKeyAndOrderFront:nil];
                    if (@available(macOS 14.0, *)) {
                        [NSApp activate];
                    } else {
#pragma clang diagnostic push
#pragma clang diagnostic ignored "-Wdeprecated-declarations"
                        [NSApp activateIgnoringOtherApps:YES];
#pragma clang diagnostic pop
                    }
                    owner->validAfter = NSProcessInfo.processInfo.systemUptime;
                    continue;
                }
                if (event.type == NSEventTypeKeyDown || event.type == NSEventTypeKeyUp ||
                    event.type == NSEventTypeFlagsChanged ||
                    (event.type >= NSEventTypeLeftMouseDown && event.type <= NSEventTypeMouseExited) ||
                    event.type == NSEventTypeScrollWheel ||
                    event.type == NSEventTypeOtherMouseDown || event.type == NSEventTypeOtherMouseUp ||
                    event.type == NSEventTypeOtherMouseDragged) {
                    [owner input:event];
                    continue;
                }
            }
            [NSApp sendEvent:event];
        }
        // Even an exactly full batch is incomplete evidence: a queued local
        // close may remain uninspected. Do not accept a choice in that turn.
        if (count == FR_PICK_EVENTS) owner->outcome = FR_PICK_FLOOD;
        if (owner->outcome >= FR_PICK_WAIT) [NSApp updateWindows];
        if (owner->outcome == FR_PICK_CHOSEN) {
            if (owner->chosen >= owner->titles.count) return FR_PICK_INVALID;
            *row = owner->chosen;
        }
        return owner->outcome;
    }
}

int fr_mac_picker_close(void *handle) {
    if (!handle) return FR_PICK_INVALID;
    if (![NSThread isMainThread]) return FR_PICK_THREAD;
    @autoreleasepool {
        FrPickerOwner *owner = (__bridge FrPickerOwner *)handle;
        // Stop admission before native retirement. Detached delegate/targets
        // cannot call a retired owner, including through queued native events.
        owner->outcome = FR_PICK_CANCELLED;
        owner->table.delegate = nil;
        owner->table.dataSource = nil;
        owner->accept.target = nil;
        owner->cancel.target = nil;
        owner->window.delegate = nil;
        [owner->window orderOut:nil];
        [owner->window close];
        owner->window = nil;
        owner->table = nil;
        owner->scroll = nil;
        owner->accept = nil;
        owner->cancel = nil;
        owner->titles = nil;
        owner->details = nil;
        // Consume only the original bridge retain. Rust clears its pointer
        // solely after this complete synchronous retirement has returned.
        (void)CFBridgingRelease(handle);
    }
    return FR_PICK_WAIT;
}
