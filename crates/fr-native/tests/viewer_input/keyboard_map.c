/* Real XCB/XKB boundary regression tests, not an input-authority substitute.
 * Include the shipping implementation to exercise its bounded verification
 * state without adding test hooks or exporting native pointers to Rust. */
#define _POSIX_C_SOURCE 200809L
#include "../../src/viewer_input.c"
#include <X11/Xlib.h>
#include <X11/XKBlib.h>
#include <X11/keysym.h>
#include <assert.h>
#include <signal.h>
#include <stdio.h>
#include <time.h>
#include <unistd.h>

extern int XTestFakeKeyEvent(Display *, unsigned int, Bool, unsigned long);
static Display *peer;
static Window window;
static struct fr_viewer_input *capture;
static uint8_t names[256][4];
static unsigned keycode;

static uint64_t now_ms(void) {
    struct timespec t;
    assert(clock_gettime(CLOCK_MONOTONIC, &t) == 0);
    return (uint64_t)t.tv_sec * 1000 + (uint64_t)t.tv_nsec / 1000000;
}
static void tick(void) {
    const struct timespec t = {0, 1000000};
    nanosleep(&t, NULL);
}
static void setup(void) {
    peer = XOpenDisplay(NULL); assert(peer);
    window = XCreateSimpleWindow(peer, DefaultRootWindow(peer), 0, 0, 320, 240, 0, 0, 0);
    XMapWindow(peer, window); XSetInputFocus(peer, window, RevertToParent, CurrentTime);
    XSync(peer, False);
    capture = fr_viewer_input_open(getenv("DISPLAY"), (uint32_t)window, 320, 240, names);
    assert(capture);
    for (unsigned i = 8; i < 256; i++) if (!memcmp(names[i], "AC01", 4)) keycode = i;
    assert(keycode && capture->keymap_size <= FR_KEYMAP_BYTES);
    assert(fr_viewer_input_barrier(capture) == 1);
    uint64_t until = now_ms() + 1000;
    for (;;) {
        struct fr_viewer_event event;
        int r = fr_viewer_input_next(capture, &event); assert(r >= 0);
        if (r && event.kind == BARRIER) break;
        assert(!r || event.kind == IGNORE);
        assert(now_ms() < until); tick();
    }
}
static xkbNewKeyboardNotify candidate(void) {
    xkbNewKeyboardNotify n = {0};
    n.type = capture->xkb_event; n.xkbType = XkbNewKeyboardNotify;
    n.deviceID = n.oldDeviceID = capture->keyboard_id;
    n.minKeyCode = n.oldMinKeyCode = capture->min_keycode;
    n.maxKeyCode = n.oldMaxKeyCode = capture->max_keycode;
    n.changed = XkbNKN_KeycodesMask | XkbNKN_GeometryMask;
    return n;
}
static void key(int pressed) {
    assert(XTestFakeKeyEvent(peer, keycode, pressed, 0)); XSync(peer, False);
}
static void wait_for_rejection(void) {
    uint64_t until = now_ms() + 1000;
    for (;;) {
        struct fr_viewer_event event;
        int r = fr_viewer_input_next(capture, &event); assert(r >= 0);
        if (r && event.kind == KEYMAP_CHANGED) break;
        assert(!r || event.kind == IGNORE);
        assert(now_ms() < until); tick();
    }
    /* Refusal is latched, not a request to silently adopt the changed map. */
    for (unsigned i = 0; i < 4; i++) {
        struct fr_viewer_event event;
        assert(fr_viewer_input_next(capture, &event) == 1);
        assert(event.kind == KEYMAP_CHANGED);
    }
}
static void cold_keyboard(void) {
    key(1); key(0);
    assert(fr_viewer_input_barrier(capture) == 1);
    unsigned events = 0, checks = 0;
    uint32_t previous = 0;
    uint64_t until = now_ms() + 1000;
    for (;;) {
        struct fr_viewer_event event;
        int r = fr_viewer_input_next(capture, &event); assert(r >= 0);
        if (r) {
            if (capture->map_query || capture->names_query) checks++;
            if (event.kind == BARRIER) break;
            assert(event.kind == IGNORE || event.kind == KEY_DOWN || event.kind == KEY_UP);
            if (event.kind != IGNORE) {
                assert(event.detail == keycode);
                assert(event.kind == (events == 0 ? KEY_DOWN : KEY_UP));
                assert(events < 2 && (!events || event.time - previous < (1u << 31)));
                previous = event.time; events++;
            }
        }
        assert(now_ms() < until); tick();
    }
    assert(events == 2 && checks == 1);
    assert(!memcmp(names, capture->keynames, sizeof(names)));
    assert(!capture->map_failed);
}
static void remap(void) {
    int width = 0;
    KeySym *old = XGetKeyboardMapping(peer, (KeyCode)keycode, 1, &width);
    assert(old && width > 0);
    old[0] = old[0] == XK_a ? XK_b : XK_a;
    XChangeKeyboardMapping(peer, (int)keycode, width, old, 1);
    XFree(old); XSync(peer, False);
}
static void changed_map_during_verification(void) {
    /* Exercise the comparison itself, not only the queued MapNotify fence. */
    remap(); xkbNewKeyboardNotify n = candidate();
    assert(recheck_keyboard(capture, &n));
    wait_for_rejection();
}
static void changed_names_during_verification(void) {
    XkbDescPtr xkb_map = XkbGetMap(peer, XkbAllMapComponentsMask, XkbUseCoreKbd);
    assert(xkb_map && XkbGetNames(peer, XkbKeyNamesMask, xkb_map) == Success);
    memcpy(xkb_map->names->keys[keycode].name, "ZZZZ", 4);
    assert(XkbSetNames(peer, XkbKeyNamesMask, keycode, 1, xkb_map));
    XkbFreeKeyboard(xkb_map, XkbAllComponentsMask, True); XSync(peer, False);
    xkbNewKeyboardNotify n = candidate(); assert(recheck_keyboard(capture, &n));
    wait_for_rejection();
}
static void invalid_notifications(void) {
    xkbNewKeyboardNotify original = candidate();
    for (unsigned i = 0; i < 9; i++) {
        xkbNewKeyboardNotify n = original;
        switch (i) {
        case 0: n.type |= 0x80; break;
        case 1: n.xkbType = XkbMapNotify; break;
        case 2: n.deviceID++; break;
        case 3: n.oldDeviceID++; break;
        case 4: n.minKeyCode++; break;
        case 5: n.oldMinKeyCode++; break;
        case 6: n.maxKeyCode--; break;
        case 7: n.oldMaxKeyCode--; break;
        case 8: n.changed |= XkbNKN_DeviceIDMask; break;
        }
        assert(!recheck_keyboard(capture, &n));
        assert(!capture->map_query && !capture->names_query);
    }
}
static void malformed_replies(void) {
    xkbGetMapReply r = {0};
    assert(!keymap_size(NULL)); assert(!keymap_size(&r));
    r.type = 1; r.length = 2; r.minKeyCode = 8; r.maxKeyCode = 255;
    assert(keymap_size(&r) == sizeof(r));
    r.sequenceNumber = 123; r.pad1 = 65535; r.pad2 = 255;
    assert(keymap_size(&r) == sizeof(r));
    assert(!r.sequenceNumber && !r.pad1 && !r.pad2);
    r.length = (FR_KEYMAP_BYTES - 32) / 4; assert(keymap_size(&r) == FR_KEYMAP_BYTES);
    r.length++; assert(!keymap_size(&r));
    r.length = UINT32_MAX; assert(!keymap_size(&r));
    r.length = 1; assert(!keymap_size(&r));
    r.length = 2; r.minKeyCode = 7; assert(!keymap_size(&r));
    r.minKeyCode = 9; r.maxKeyCode = 8; assert(!keymap_size(&r));
}
static void stalled_server(pid_t server) {
    assert(kill(server, SIGSTOP) == 0);
    /* No reply can arrive, but both requests and every poll remain nonblocking.
     * Rust owns the unchanged original 50ms turn deadline and cancellation. */
    xkbNewKeyboardNotify n = candidate(); assert(recheck_keyboard(capture, &n));
    uint32_t map = capture->map_query, names_request = capture->names_query;
    uint64_t start = now_ms();
    for (unsigned i = 0; i < 100; i++) {
        struct fr_viewer_event event;
        assert(fr_viewer_input_next(capture, &event) == 0);
        assert(capture->map_query == map && capture->names_query == names_request);
    }
    assert(now_ms() - start < 200);
    assert(kill(server, SIGCONT) == 0);
    uint64_t until = now_ms() + 1000;
    while (capture->map_query || capture->names_query) {
        struct fr_viewer_event event;
        assert(fr_viewer_input_next(capture, &event) >= 0);
        assert(!capture->map_failed && now_ms() < until); tick();
    }
}
int main(int argc, char **argv) {
    assert(argc == 3); setup();
    if (!strcmp(argv[1], "cold-keyboard")) cold_keyboard();
    else if (!strcmp(argv[1], "real-remap")) { remap(); wait_for_rejection(); }
    else if (!strcmp(argv[1], "map-revalidation")) changed_map_during_verification();
    else if (!strcmp(argv[1], "name-revalidation")) changed_names_during_verification();
    else if (!strcmp(argv[1], "invalid-notifications")) invalid_notifications();
    else if (!strcmp(argv[1], "malformed-replies")) malformed_replies();
    else if (!strcmp(argv[1], "stalled-server")) {
        long server = strtol(argv[2], NULL, 10); assert(server > 1 && server <= INT32_MAX);
        stalled_server((pid_t)server);
    } else assert(0);
    fr_viewer_input_close(capture);
    XDestroyWindow(peer, window); XCloseDisplay(peer);
    printf("PASS %s\n", argv[1]); return 0;
}
