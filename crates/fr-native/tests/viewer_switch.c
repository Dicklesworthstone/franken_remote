/* Actual XCB window boundary against a private X server. XTest supplies logical
 * server input; SendEvent is the untrusted comparison. These tests do not claim
 * optical visibility, completed session cleanup, or a new authenticated owner. */
#define _POSIX_C_SOURCE 200809L
#include <X11/Xlib.h>
#include <X11/keysym.h>
#include <assert.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

/* The same narrow installed libXtst ABI used by the native input adapter and
 * keyboard-map regression; no injected replacement implementation. */
extern Bool XTestQueryExtension(Display *, int *, int *, int *, int *);
extern int XTestFakeKeyEvent(Display *, unsigned int, Bool, unsigned long);

struct fr_viewer_window;
struct fr_viewer_window *fr_viewer_window_open(const char *, uint32_t, uint32_t, uint32_t *);
void fr_viewer_window_close(struct fr_viewer_window *);
int fr_viewer_window_next(struct fr_viewer_window *, uint32_t *);
int fr_viewer_window_control_offer(struct fr_viewer_window *, int);
int fr_viewer_window_control_released(struct fr_viewer_window *);
int fr_viewer_window_display_switch_offer(struct fr_viewer_window *, int);
void fr_viewer_window_display_switch_reset(struct fr_viewer_window *);
uint32_t fr_viewer_window_display_switch_age(struct fr_viewer_window *);
int fr_viewer_window_display_switch_released(struct fr_viewer_window *);

enum { HIDDEN = 2, STOP = 5, CONTROL_CHOSEN = 6, CONTROL_REMAP = 8,
       FOCUS_LEFT = 13, SWITCH_CHOSEN = 14,
       SWITCH_RESET = 15, SWITCH_REMAP = 16, SWITCH_CLOCK = 17, EVENT_COUNT = 18 };
struct observed { unsigned count[EVENT_COUNT]; };
struct fixture {
    Display *peer;
    struct fr_viewer_window *native;
    Window window;
    unsigned keys[7];
};
static uint64_t millis(void) {
    struct timespec now;
    assert(clock_gettime(CLOCK_MONOTONIC, &now) == 0);
    return (uint64_t)now.tv_sec * 1000 + (uint64_t)now.tv_nsec / 1000000;
}
static void pause_ms(unsigned ms) {
    struct timespec wait = { ms / 1000, (long)(ms % 1000) * 1000000 };
    while (nanosleep(&wait, &wait) != 0) {}
}
/* Drain the real native API, including its asynchronous server-time barrier.
 * A short quiet wait permits the server to answer; no event time is rewritten. */
static struct observed drain(struct fixture *f, unsigned ms) {
    struct observed seen = {{0}};
    uint64_t until = millis() + ms;
    unsigned consumed = 0;
    do {
        uint32_t event = 0;
        int result = fr_viewer_window_next(f->native, &event);
        assert(result >= 0);
        if (result) {
            assert(event < EVENT_COUNT && ++consumed < 4096);
            ++seen.count[event];
        } else pause_ms(1);
    } while (millis() < until);
    return seen;
}
static void focus(struct fixture *f, Window window) {
    XSetInputFocus(f->peer, window, RevertToPointerRoot, CurrentTime);
    XSync(f->peer, False);
}
static void key(struct fixture *f, unsigned code, int pressed) {
    assert(XTestFakeKeyEvent(f->peer, code, pressed, 0));
}
static void sync_keys(struct fixture *f) { XSync(f->peer, False); }
static struct fixture create(void) {
    const char *display = getenv("DISPLAY");
    assert(display && *display);
    struct fixture f = {0};
    f.peer = XOpenDisplay(display);
    assert(f.peer);
    int event, error, major, minor;
    assert(XTestQueryExtension(f.peer, &event, &error, &major, &minor));
    const KeySym symbols[] = { XK_Control_L, XK_Alt_L, XK_Shift_L,
                              XK_Control_R, XK_Alt_R, XK_Shift_R, XK_F7 };
    for (unsigned i = 0; i < 7; ++i) {
        f.keys[i] = XKeysymToKeycode(f.peer, symbols[i]);
        assert(f.keys[i] >= 8);
    }
    /* Xvfb's first XTest key activates its slave keyboard and announces a
     * changed keyboard. Establish that test device before the production
     * owner captures its map; no held gesture survives into window startup. */
    key(&f, f.keys[6], True);
    key(&f, f.keys[6], False);
    sync_keys(&f);
    uint32_t window = 0;
    f.native = fr_viewer_window_open(display, 320, 240, &window);
    assert(f.native && window);
    f.window = window;
    focus(&f, f.window);
    (void)drain(&f, 20);
    return f;
}
static void finish(struct fixture *f) {
    for (unsigned i = 0; i < 7; ++i) key(f, f->keys[i], False);
    sync_keys(f);
    fr_viewer_window_close(f->native);
    XCloseDisplay(f->peer);
}
static void offer(struct fixture *f) {
    assert(fr_viewer_window_display_switch_offer(f->native, 1) == 1);
    struct observed seen = drain(f, 20);
    assert(seen.count[SWITCH_CHOSEN] == 0);
    assert(fr_viewer_window_display_switch_released(f->native) == 0);
    pause_ms(2); /* the press must be later than the server's arm baseline */
}
static void chord_down(struct fixture *f, unsigned side) {
    for (unsigned i = 0; i < 3; ++i) key(f, f->keys[side + i], True);
    key(f, f->keys[6], True);
    sync_keys(f);
}
static void chord_up(struct fixture *f, unsigned side) {
    key(f, f->keys[6], False);
    for (unsigned i = 0; i < 3; ++i) key(f, f->keys[side + i], False);
    sync_keys(f);
}
static void assert_chosen(struct fixture *f, struct observed seen) {
    if (seen.count[SWITCH_CLOCK] != 1 || seen.count[SWITCH_CHOSEN] != 1) {
        for (unsigned i = 0; i < EVENT_COUNT; ++i)
            if (seen.count[i]) fprintf(stderr, "native_event[%u]=%u\n", i, seen.count[i]);
        fprintf(stderr, "switch_age=%u released=%d\n",
            fr_viewer_window_display_switch_age(f->native),
            fr_viewer_window_display_switch_released(f->native));
    }
    assert(seen.count[SWITCH_CLOCK] == 1);
    assert(seen.count[SWITCH_CHOSEN] == 1);
    assert(fr_viewer_window_display_switch_age(f->native) < 250);
    assert(fr_viewer_window_display_switch_released(f->native) == 1);
}
static void physical_chords(void) {
    for (unsigned side = 0; side <= 3; side += 3) {
        struct fixture f = create();
        offer(&f);
        chord_down(&f, side);
        chord_up(&f, side);
        assert_chosen(&f, drain(&f, 25));
        fr_viewer_window_display_switch_reset(f.native);
        assert(fr_viewer_window_display_switch_released(f.native) == 0);
        finish(&f);
    }
    puts("physical_left_and_right_chords: passed");
}
static void incomplete_and_held(void) {
    for (unsigned missing = 0; missing < 3; ++missing) {
        struct fixture f = create();
        offer(&f);
        for (unsigned i = 0; i < 3; ++i) if (i != missing) key(&f, f.keys[i], True);
        key(&f, f.keys[6], True);
        chord_up(&f, 0);
        struct observed seen = drain(&f, 25);
        assert(seen.count[SWITCH_CHOSEN] == 0);
        assert(fr_viewer_window_display_switch_released(f.native) == 0);
        finish(&f);
    }
    struct fixture f = create();
    chord_down(&f, 0);
    offer(&f);
    chord_up(&f, 0);
    assert(drain(&f, 25).count[SWITCH_CHOSEN] == 0);
    pause_ms(2);
    chord_down(&f, 0);
    assert(drain(&f, 25).count[SWITCH_CHOSEN] == 0);
    assert(fr_viewer_window_display_switch_released(f.native) == 0);
    chord_up(&f, 0);
    assert_chosen(&f, drain(&f, 25));
    finish(&f);
    puts("incomplete_initially_held_and_unreleased_chords: passed");
}
static void synthetic_keys(void) {
    struct fixture f = create();
    offer(&f);
    for (unsigned pressed = 1; ; --pressed) {
        for (unsigned i = 0; i < 4; ++i) {
            XEvent event;
            memset(&event, 0, sizeof(event));
            event.xkey.type = pressed ? KeyPress : KeyRelease;
            event.xkey.display = f.peer;
            event.xkey.window = f.window;
            event.xkey.root = DefaultRootWindow(f.peer);
            event.xkey.same_screen = True;
            event.xkey.keycode = f.keys[i == 3 ? 6 : i];
            event.xkey.state = ControlMask | Mod1Mask | ShiftMask;
            assert(XSendEvent(f.peer, f.window, False,
                pressed ? KeyPressMask : KeyReleaseMask, &event));
        }
        if (!pressed) break;
    }
    XSync(f.peer, False);
    struct observed seen = drain(&f, 25);
    assert(seen.count[SWITCH_CHOSEN] == 0);
    assert(fr_viewer_window_display_switch_released(f.native) == 0);
    finish(&f);
    puts("synthetic_chord_refused: passed");
}
static void focus_changes(void) {
    struct fixture f = create();
    offer(&f);
    chord_down(&f, 0);
    chord_up(&f, 0);
    assert_chosen(&f, drain(&f, 25));
    focus(&f, PointerRoot);
    /* The final native query must refuse even before queued FocusOut is read. */
    assert(fr_viewer_window_display_switch_released(f.native) == 0);
    focus(&f, f.window);
    struct observed seen = drain(&f, 25);
    assert(seen.count[FOCUS_LEFT] != 0);
    assert(fr_viewer_window_display_switch_released(f.native) == 0);
    finish(&f);
    puts("focus_loss_and_same_batch_regain_refuse: passed");
}
static void close_and_hide(void) {
    for (int close = 0; close <= 1; ++close) {
        struct fixture f = create();
        offer(&f);
        chord_down(&f, 0);
        chord_up(&f, 0);
        assert_chosen(&f, drain(&f, 25));
        if (close) {
            XEvent event;
            memset(&event, 0, sizeof(event));
            event.xclient.type = ClientMessage;
            event.xclient.display = f.peer;
            event.xclient.window = f.window;
            event.xclient.message_type = XInternAtom(f.peer, "WM_PROTOCOLS", False);
            event.xclient.format = 32;
            event.xclient.data.l[0] = (long)XInternAtom(f.peer, "WM_DELETE_WINDOW", False);
            assert(XSendEvent(f.peer, f.window, False, NoEventMask, &event));
        } else XUnmapWindow(f.peer, f.window);
        XSync(f.peer, False);
        struct observed seen = drain(&f, 25);
        assert(seen.count[close ? STOP : HIDDEN] != 0);
        assert(fr_viewer_window_display_switch_released(f.native) == 0);
        finish(&f);
    }
    puts("window_close_and_unmap_override_pending_choice: passed");
}
static void stale_events(void) {
    struct fixture f = create();
    offer(&f);
    chord_down(&f, 0);
    chord_up(&f, 0);
    pause_ms(300);
    struct observed seen = drain(&f, 25);
    assert(seen.count[SWITCH_CHOSEN] == 0);
    assert(fr_viewer_window_display_switch_released(f.native) == 0);
    finish(&f);
    puts("old_server_timestamps_never_refresh_on_dequeue: passed");
}
static void held_repeat_cannot_refresh(void) {
    struct fixture f = create();
    offer(&f);
    chord_down(&f, 0);
    assert(drain(&f, 25).count[SWITCH_CHOSEN] == 0);
    pause_ms(300);
    key(&f, f.keys[6], True); /* a repeated press cannot replace the original */
    sync_keys(&f);
    chord_up(&f, 0);
    assert(drain(&f, 25).count[SWITCH_CHOSEN] == 0);
    assert(fr_viewer_window_display_switch_released(f.native) == 0);
    finish(&f);
    puts("held_key_repeat_cannot_renew_original_press: passed");
}
static void mapping_change(void) {
    struct fixture f = create();
    offer(&f);
    chord_down(&f, 0);
    chord_up(&f, 0);
    assert_chosen(&f, drain(&f, 25));
    int per_key = 0;
    KeySym *mapping = XGetKeyboardMapping(f.peer, (KeyCode)f.keys[6], 1, &per_key);
    assert(mapping && per_key > 0);
    /* Even a same-value map notification invalidates the retained gesture. */
    XChangeKeyboardMapping(f.peer, (int)f.keys[6], per_key, mapping, 1);
    XFree(mapping);
    XSync(f.peer, False);
    struct observed seen = drain(&f, 25);
    /* Core MappingNotify retains the existing shared-remap event; XKB uses
     * the switch-specific event. Both invalidate the same native intent. */
    assert(seen.count[CONTROL_REMAP] + seen.count[SWITCH_REMAP] != 0);
    assert(fr_viewer_window_display_switch_released(f.native) == 0);
    finish(&f);
    puts("keyboard_mapping_change_invalidates_intent: passed");
}
static void modes_and_control(void) {
    struct fixture f = create();
    for (int mode = 0; mode <= 2; mode += 2) {
        assert(fr_viewer_window_display_switch_offer(f.native, mode) == 1);
        (void)drain(&f, 20);
        if (mode == 2) {
            char *title = NULL;
            assert(XFetchName(f.peer, f.window, &title) && title);
            assert(strstr(title, "Ctrl+Alt+Shift+F7"));
            XFree(title);
        }
        chord_down(&f, 0);
        chord_up(&f, 0);
        assert(drain(&f, 25).count[SWITCH_CHOSEN] == 0);
        assert(fr_viewer_window_display_switch_released(f.native) == 0);
    }
    offer(&f);
    assert(fr_viewer_window_control_offer(f.native, 1) == 1);
    (void)drain(&f, 20);
    pause_ms(2);
    unsigned f8 = XKeysymToKeycode(f.peer, XK_F8);
    assert(f8 >= 8);
    key(&f, f8, True); key(&f, f8, False); sync_keys(&f);
    assert(drain(&f, 25).count[CONTROL_CHOSEN] == 1);
    assert(fr_viewer_window_control_released(f.native) == 1);
    assert(fr_viewer_window_control_offer(f.native, 0) == 1);
    (void)drain(&f, 20);
    pause_ms(2);
    chord_down(&f, 0);
    chord_up(&f, 0);
    assert_chosen(&f, drain(&f, 25));
    finish(&f);
    puts("disabled_title_only_and_existing_F8_coexistence: passed");
}
int main(void) {
    assert(setvbuf(stdout, NULL, _IOLBF, 0) == 0);
    physical_chords();
    incomplete_and_held();
    synthetic_keys();
    focus_changes();
    close_and_hide();
    stale_events();
    held_repeat_cannot_refresh();
    mapping_change();
    modes_and_control();
    puts("viewer_switch_native_cases=9");
    return 0;
}
