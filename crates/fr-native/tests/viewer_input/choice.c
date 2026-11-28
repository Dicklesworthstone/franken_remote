/* Exact shipping pause/probe boundary on a fresh, owned X server. These prove
 * native event and query behavior, not remote authority or transfer delivery. */
#define _POSIX_C_SOURCE 200809L
#include "../../src/viewer_input.c"
#include <assert.h>
#include <signal.h>
#include <stdio.h>
#include <unistd.h>

extern int XTestFakeKeyEvent(Display *, unsigned int, Bool, unsigned long);
extern int XTestFakeButtonEvent(Display *, unsigned int, Bool, unsigned long);
static Display *peer;
static Window window, other, cover;
static struct fr_viewer_input *capture;
static unsigned seen[32];
static uint8_t names[256][4];

static void tick(void) { const struct timespec t = {0, 1000000}; nanosleep(&t, NULL); }
static uint64_t now(void) { uint64_t value = fr_viewer_input_choice_now(); assert(value); return value; }
static int terminal(const struct fr_viewer_event *event) {
    return (event->kind >= FOCUS_LOST && event->kind <= DEVICES_CHANGED && event->kind != GEOMETRY) ||
        (event->kind == GEOMETRY && (event->x != 320 || event->y != 240));
}
static int drain(void) {
    assert(fr_viewer_input_barrier(capture));
    uint64_t until = now() + UINT64_C(1000000);
    for (;;) {
        struct fr_viewer_event event;
        int result = fr_viewer_input_next(capture, &event); assert(result >= 0);
        if (result) {
            assert(event.kind < 32); seen[event.kind]++;
            if (terminal(&event)) return (int)event.kind;
            if (event.kind == BARRIER) return 0;
        } else tick();
        assert(now() < until);
    }
}
static void setup(void) {
    assert(XInitThreads());
    assert(XSetLocaleModifiers("@im=none"));
    peer = XOpenDisplay(NULL); assert(peer);
    Window root = DefaultRootWindow(peer);
    window = XCreateSimpleWindow(peer, root, 0, 0, 320, 240, 0, 0, 0);
    other = XCreateSimpleWindow(peer, root, 350, 0, 200, 160, 0, 0, 0);
    cover = XCreateSimpleWindow(peer, root, 0, 0, 320, 240, 0, 0, 0);
    XMapWindow(peer, window); XMapWindow(peer, other);
    XSetInputFocus(peer, window, RevertToParent, CurrentTime);
    XWarpPointer(peer, None, window, 0, 0, 0, 0, 80, 80); XSync(peer, False);
    int refusal = 0;
    capture = fr_viewer_input_open(getenv("DISPLAY"), (uint32_t)window, 320, 240, names, &refusal);
    assert(capture && !drain());
}
static void focus(Window target) { XSetInputFocus(peer, target, RevertToParent, CurrentTime); XSync(peer, False); }
static void key(KeySym symbol, int pressed) {
    unsigned int code = XKeysymToKeycode(peer, symbol); assert(code);
    assert(XTestFakeKeyEvent(peer, code, pressed, 0)); XSync(peer, False);
}
static void no_queries(void) {
    assert(!capture->choice_focus && !capture->choice_geometry && !capture->choice_attributes &&
        !capture->choice_keys && !capture->choice_pointer);
}
static int poll_probe(void) {
    uint64_t until = now() + FR_CHOICE_TURN_US;
    for (;;) {
        int result = fr_viewer_input_choice_probe_poll(capture);
        assert(result >= 0);
        if (result) {
            no_queries();
            assert(!drain());
            result = fr_viewer_input_choice_probe_poll(capture);
            assert(result == 1 || result == 2);
            return result;
        }
        assert(now() < until); tick();
    }
}
static int probe(void) { assert(fr_viewer_input_choice_probe_begin(capture) == 1); return poll_probe(); }
static void pause_input(void) {
    assert(fr_viewer_input_choice_begin(capture) == 1);
    assert(capture->choice_mode && !drain());
    assert(probe() == 1);
}
static void focus_round_trip(void) {
    pause_input();
    focus(other); assert(!drain());
    assert(!seen[FOCUS_LOST] && fr_viewer_input_choice_end(capture) == 0);
    for (unsigned i = 0; i < 3; i++) assert(probe() == 2);
    no_queries();
    focus(window); assert(!drain());
    assert(probe() == 1 && fr_viewer_input_choice_end(capture) == 1);
    assert(!capture->choice_mode);
    focus(other); assert(drain() == FOCUS_LOST); /* ordinary behavior restored */
}
static void visibility_round_trip(void) {
    pause_input();
    XMapRaised(peer, cover); XSync(peer, False); assert(!drain());
    assert(!capture->unobscured && probe() == 2);
    assert(fr_viewer_input_choice_end(capture) == 0);
    XUnmapWindow(peer, cover); XSync(peer, False); assert(!drain());
    assert(capture->unobscured && probe() == 1);
    /* A positive snapshot cannot overwrite a later real visibility change. */
    XMapRaised(peer, cover); XSync(peer, False); assert(!drain());
    assert(fr_viewer_input_choice_probe_poll(capture) == 2);
    assert(fr_viewer_input_choice_end(capture) == 0);
    XUnmapWindow(peer, cover); XSync(peer, False); assert(!drain());
    assert(probe() == 1 && fr_viewer_input_choice_end(capture) == 1);
    XMapRaised(peer, cover); XSync(peer, False); assert(drain() == WINDOW_LOST);
}
static void stale_focus_reply(void) {
    pause_input();
    assert(fr_viewer_input_choice_probe_begin(capture) == 1);
    /* A test-only synchronous fence makes the queued answers definitely older
     * than the real away-and-back transition. Production never waits here. */
    void *fence = reply(capture, xcb_get_input_focus(capture->c).sequence); assert(fence); free(fence);
    focus(other); focus(window); assert(!drain());
    assert(poll_probe() == 2 && fr_viewer_input_choice_end(capture) == 0);
    assert(probe() == 1 && fr_viewer_input_choice_end(capture) == 1);
}
static void held_input(void) {
    pause_input();
    key(XK_a, 1); assert(!drain());
    assert(probe() == 2 && fr_viewer_input_choice_end(capture) == 0);
    key(XK_a, 0); assert(!drain());
    assert(probe() == 1);
    assert(XTestFakeButtonEvent(peer, 8, True, 0)); XSync(peer, False); assert(!drain());
    assert(probe() == 2); no_queries(); /* XI2 includes Back/Forward */
    assert(XTestFakeButtonEvent(peer, 8, False, 0)); XSync(peer, False); assert(!drain());
    assert(probe() == 1);
    XWarpPointer(peer, None, window, 0, 0, 0, 0, 90, 90); XSync(peer, False); assert(!drain());
    assert(fr_viewer_input_choice_end(capture) == 0);
    assert(probe() == 1 && fr_viewer_input_choice_end(capture) == 1);
    assert(seen[KEY_DOWN] && seen[KEY_UP] && seen[BUTTON_DOWN] && seen[BUTTON_UP] && seen[MOTION]);
}
static void text_pause(void) {
    assert(fr_viewer_input_text_prepare(capture, getenv("DISPLAY")) == 1);
    assert(fr_viewer_input_text_offer(capture, 19, 1)); assert(!drain());
    key(XK_F7, 1); key(XK_F7, 0); assert(!drain());
    assert(capture->text->active);
    assert(fr_viewer_input_choice_begin(capture) == 0 && !capture->choice_mode && capture->text->active);
    key(XK_Escape, 1); key(XK_Escape, 0); assert(!drain());
    uint8_t bytes[FR_TEXT_BYTES]; uint64_t serial; uint32_t length;
    assert(fr_viewer_input_text_take(capture, &serial, bytes, sizeof(bytes), &length) == 1);
    fr_viewer_input_text_result(capture, serial, 0);
    assert(!drain());
    pause_input();
    unsigned sends = seen[TEXT_SEND], local = seen[TEXT_KEY_DOWN];
    key(XK_F7, 1); key(XK_F7, 0); key(XK_Return, 1); key(XK_Return, 0);
    assert(!drain());
    assert(!capture->text->active && seen[TEXT_SEND] == sends && seen[TEXT_KEY_DOWN] == local);
    assert(probe() == 1 && fr_viewer_input_choice_end(capture) == 1);
}
static void pending_refusal(void) {
    assert(fr_viewer_input_held_begin(capture));
    uint32_t query = capture->keys_query;
    assert(fr_viewer_input_choice_begin(capture) == 0);
    assert(!capture->choice_mode && capture->held_pending && capture->keys_query == query);
    assert(!drain());
    struct fr_viewer_held held;
    uint64_t until = now() + FR_CHOICE_TURN_US;
    while (!fr_viewer_input_held_poll(capture, &held)) { assert(now() < until); tick(); }
    pause_input();
    assert(fr_viewer_input_choice_begin(capture) == 0 && capture->choice_mode);
    assert(fr_viewer_input_choice_end(capture) == 1);
}
static void grab(void) {
    assert(fr_viewer_input_grab_begin(capture));
    uint64_t until = now() + FR_CHOICE_TURN_US;
    for (;;) {
        int result = fr_viewer_input_grab_poll(capture); assert(result >= 0);
        if (result) break;
        assert(now() < until); tick();
    }
    assert(!drain());
}
static void shortcuts_round_trip(void) {
    grab(); assert(capture->grab_state == FR_GRAB_ACQUIRED);
    pause_input();
    assert(!capture->grab_state && !capture->grab_title);
    assert(XGrabKeyboard(peer, window, False, GrabModeAsync, GrabModeAsync, CurrentTime) == GrabSuccess);
    XUngrabKeyboard(peer, CurrentTime); XSync(peer, False); assert(!drain());
    assert(probe() == 1); grab();
    assert(probe() == 1 && fr_viewer_input_choice_end(capture) == 1);
    assert(XGrabKeyboard(peer, window, False, GrabModeAsync, GrabModeAsync, CurrentTime) == AlreadyGrabbed);
    fr_viewer_input_close(capture); capture = NULL;
    assert(XGrabKeyboard(peer, window, False, GrabModeAsync, GrabModeAsync, CurrentTime) == GrabSuccess);
    XUngrabKeyboard(peer, CurrentTime);
}
static void terminal_event(const char *which) {
    pause_input();
    if (!strcmp(which, "unmap")) XUnmapWindow(peer, window);
    else if (!strcmp(which, "geometry")) XResizeWindow(peer, window, 321, 240);
    else {
        XEvent event = {0};
        event.xfocus.type = FocusIn; event.xfocus.window = window;
        event.xfocus.mode = NotifyNormal; event.xfocus.detail = NotifyNonlinear;
        assert(XSendEvent(peer, window, False, FocusChangeMask, &event));
    }
    XSync(peer, False);
    int result = drain(); assert(result == WINDOW_LOST || result == GEOMETRY || result == SYNTHETIC);
    assert(capture->choice_mode && fr_viewer_input_choice_end(capture) == -1); no_queries();
}
static void stalled_probe(pid_t server) {
    pause_input();
    assert(kill(server, SIGSTOP) == 0);
    uint64_t started = now();
    assert(fr_viewer_input_choice_probe_begin(capture) == 1);
    int result;
    do { result = fr_viewer_input_choice_probe_poll(capture); if (!result) tick(); } while (!result);
    uint64_t elapsed = now() - started;
    assert(result == -1 && elapsed >= FR_CHOICE_TURN_US && elapsed < UINT64_C(250000));
    no_queries(); assert(fr_viewer_input_choice_end(capture) == -1);
    assert(kill(server, SIGCONT) == 0);
}
int main(int argc, char **argv) {
    assert(argc == 3); setup();
    if (!strcmp(argv[1], "focus")) focus_round_trip();
    else if (!strcmp(argv[1], "visibility")) visibility_round_trip();
    else if (!strcmp(argv[1], "stale-focus-reply")) stale_focus_reply();
    else if (!strcmp(argv[1], "held-input")) held_input();
    else if (!strcmp(argv[1], "text-pause")) text_pause();
    else if (!strcmp(argv[1], "pending-refusal")) pending_refusal();
    else if (!strcmp(argv[1], "shortcuts")) shortcuts_round_trip();
    else if (!strcmp(argv[1], "unmap") || !strcmp(argv[1], "geometry") || !strcmp(argv[1], "synthetic")) terminal_event(argv[1]);
    else if (!strcmp(argv[1], "stalled-probe")) {
        long server = strtol(argv[2], NULL, 10); assert(server > 1 && server <= INT32_MAX);
        stalled_probe((pid_t)server);
    } else assert(0);
    if (capture) fr_viewer_input_close(capture);
    XDestroyWindow(peer, cover); XDestroyWindow(peer, other); XDestroyWindow(peer, window); XCloseDisplay(peer);
    printf("PASS %s\n", argv[1]); return 0;
}
