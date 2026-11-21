/* Exact shipping native composer, exercised through real X11/XTest input.
 * The test owns its window and keyboard map on an isolated X server. This is
 * native producer evidence, not remote input-authority or host delivery proof. */
#define _POSIX_C_SOURCE 200809L
#include "../../src/viewer_input.c"
#include <assert.h>
#include <stdio.h>
#include <time.h>

extern int XTestFakeKeyEvent(Display *, unsigned int, Bool, unsigned long);
extern int XTestFakeButtonEvent(Display *, unsigned int, Bool, unsigned long);
static Display *peer;
static Window window;
static struct fr_viewer_input *capture;
static uint8_t names[256][4];
static unsigned int sends, retired, remote, local, focus_lost, synthetic;
static unsigned int remote_buttons, local_buttons;
static uint32_t sent_at;
static unsigned int peer_errors;
static int previous_error(Display *display, XErrorEvent *event) {
    assert(display == peer && event->error_code == BadWindow);
    peer_errors++; return 0;
}

static uint64_t now_ms(void) {
    struct timespec t; assert(clock_gettime(CLOCK_MONOTONIC, &t) == 0);
    return (uint64_t)t.tv_sec * 1000u + (uint64_t)t.tv_nsec / 1000000u;
}
static void pause_ms(void) { const struct timespec t = {0, 1000000}; nanosleep(&t, NULL); }
static void drain(void) {
    assert(fr_viewer_input_barrier(capture));
    uint64_t until = now_ms() + 2000;
    for (;;) {
        struct fr_viewer_event event;
        int result = fr_viewer_input_next(capture, &event); assert(result >= 0);
        if (result) {
            if (event.kind == BARRIER) break;
            if (event.kind == TEXT_SEND) { sends++; sent_at = event.time; }
            else if (event.kind == TEXT_RETIRED) retired++;
            else if (event.kind == KEY_DOWN || event.kind == KEY_UP) remote++;
            else if (event.kind == TEXT_KEY_DOWN || event.kind == TEXT_KEY_UP) local++;
            else if (event.kind == BUTTON_DOWN || event.kind == BUTTON_UP) remote_buttons++;
            else if (event.kind == TEXT_BUTTON_DOWN || event.kind == TEXT_BUTTON_UP) local_buttons++;
            else if (event.kind == FOCUS_LOST) focus_lost++;
            else if (event.kind == SYNTHETIC) synthetic++;
            else assert(event.kind == IGNORE || event.kind == GEOMETRY || event.kind == MOTION);
        }
        assert(now_ms() < until); pause_ms();
    }
}
static unsigned code(KeySym symbol) { unsigned key = XKeysymToKeycode(peer, symbol); assert(key); return key; }
static void key_code(unsigned key, int down) {
    assert(XTestFakeKeyEvent(peer, key, down, 0)); XSync(peer, False);
}
static void key(KeySym symbol, int down) { key_code(code(symbol), down); }
static void tap(KeySym symbol) { key(symbol, 1); key(symbol, 0); }
static void button(unsigned number, int down) {
    assert(XTestFakeButtonEvent(peer, number, down, 0)); XSync(peer, False);
}
static void move_pointer_inside(void) {
    XWarpPointer(peer, None, window, 0, 0, 0, 0, 40, 80); XSync(peer, False); drain();
}
static void lose_key_release(KeySym symbol) {
    xcb_get_window_attributes_reply_t *attributes = reply(capture,
        xcb_get_window_attributes(capture->c, capture->window).sequence);
    assert(attributes);
    uint32_t original = attributes->your_event_mask;
    uint32_t without_release = original & ~XCB_EVENT_MASK_KEY_RELEASE;
    free(attributes);
    assert(checked(capture, xcb_change_window_attributes_checked(capture->c,
        capture->window, XCB_CW_EVENT_MASK, &without_release)));
    key(symbol, 0);
    assert(checked(capture, xcb_change_window_attributes_checked(capture->c,
        capture->window, XCB_CW_EVENT_MASK, &original)));
    drain();
}
static void reconcile_released(void) {
    assert(fr_viewer_input_held_begin(capture)); drain();
    struct fr_viewer_held actual;
    uint64_t until = now_ms() + 2000;
    for (;;) {
        int result = fr_viewer_input_held_poll(capture, &actual); assert(result >= 0);
        if (result) break;
        assert(now_ms() < until); pause_ms();
    }
    fr_viewer_input_text_reconcile(capture, &actual);
}
static void unicode_map(KeySym old, KeySym value) {
    unsigned key = code(old); int width = 0;
    KeySym *symbols = XGetKeyboardMapping(peer, (KeyCode)key, 1, &width);
    assert(symbols && width > 0);
    for (int i = 0; i < width; i++) symbols[i] = value;
    XChangeKeyboardMapping(peer, (int)key, width, symbols, 1); XFree(symbols);
}
static void setup(const char *test) {
    int unavailable = !strcmp(test, "unavailable-ime");
    int small = !strcmp(test, "small-surface");
    assert(XInitThreads());
    assert(XSetLocaleModifiers(unavailable ? "@im=frankenremote-test-missing" : "@im=none"));
    peer = XOpenDisplay(NULL); assert(peer);
    unicode_map(XK_F8, XK_eacute); unicode_map(XK_F9, 0x01006f22); unicode_map(XK_F10, 0x0101f642);
    unicode_map(XK_F11, XK_Multi_key);
    unsigned int width = small ? 240 : 640, height = small ? 140 : 360;
    window = XCreateSimpleWindow(peer, DefaultRootWindow(peer), 0, 0, width, height, 0, 0, 0);
    XMapWindow(peer, window); XSetInputFocus(peer, window, RevertToParent, CurrentTime); XSync(peer, False);
    int refusal = 0;
    capture = fr_viewer_input_open(getenv("DISPLAY"), (uint32_t)window, width, height, names, &refusal);
    assert(capture && !refusal);
    assert(fr_viewer_input_text_prepare(capture, getenv("DISPLAY")) == !(unavailable || small));
    assert(fr_viewer_input_text_offer(capture, 1, 1)); drain();
}
static void open_editor(void) {
    tap(XK_F7); drain(); assert(capture->text->active);
    Window focused; int revert; XGetInputFocus(peer, &focused, &revert); assert(focused == window);
}
static void send_editor(void) {
    key(XK_Control_L, 1); key(XK_Return, 1); key(XK_Return, 0); key(XK_Control_L, 0); drain();
}
static uint64_t take(const char *expected) {
    uint8_t bytes[FR_TEXT_BYTES]; memset(bytes, 0x7f, sizeof(bytes));
    uint64_t serial = 0; uint32_t length = 0;
    assert(fr_viewer_input_text_take(capture, &serial, bytes, sizeof(bytes), &length) == 1);
    assert(serial && length == strlen(expected) && !memcmp(bytes, expected, length));
    memset(bytes, 0, sizeof(bytes));
    uint64_t duplicate = 123; uint32_t duplicate_length = 456;
    assert(fr_viewer_input_text_take(capture, &duplicate, bytes, sizeof(bytes), &duplicate_length) == 0);
    assert(!duplicate && !duplicate_length);
    return serial;
}
static void utf8_and_focus(void) {
    open_editor(); tap(XK_a); tap(XK_eacute); tap(0x01006f22); tap(0x0101f642); drain();
    assert(!remote && local >= 10 && !focus_lost);
    send_editor();
    if (!(sends == 1 && sent_at && !remote && !capture->text->active))
        fprintf(stderr, "Send state: sends=%u at=%u remote=%u active=%d bytes=%zu invalid=%d enter=%u ready=%d\n",
            sends, sent_at, remote, capture->text->active, capture->text->length,
            capture->text->invalid, capture->text->enter, capture->text->ready);
    assert(sends == 1 && sent_at && !remote && !capture->text->active);
    uint64_t serial = take("a\xc3\xa9\xe6\xbc\xa2\xf0\x9f\x99\x82"); assert(serial == 1);
    fr_viewer_input_text_result(capture, serial, 1);
    assert(fr_viewer_input_text_offer(capture, 2, 1));
    tap(XK_b); drain(); assert(remote == 2);
}
static void cancel_reopen(void) {
    open_editor(); tap(XK_a); tap(XK_Escape); drain();
    assert(retired == 1 && !sends && !capture->text->active && !remote);
    tap(XK_F7); drain(); assert(!capture->text->active);
    assert(take("") == 1);
    tap(XK_F7); drain(); assert(!capture->text->active);
    assert(fr_viewer_input_text_offer(capture, 2, 1)); open_editor();
    tap(XK_b); send_editor(); assert(take("b") == 2);
}
static void compose_before_send(void) {
    open_editor();
    /* No intervening drain: all physical events, including Send, are already
     * queued when the native owner next runs. XIM's completed last character
     * must enter the draft before that original Send is handled. */
    tap(XK_a); tap(XK_Multi_key); tap(XK_apostrophe); tap(XK_e); send_editor();
    assert(sends == 1 && !remote); assert(take("a\xc3\xa9") == 1);
}
static void context_and_readiness(void) {
    open_editor(); tap(XK_a); drain();
    assert(fr_viewer_input_text_offer(capture, 1, 0)); send_editor();
    assert(!sends && capture->text->active && capture->text->length == 1);
    assert(fr_viewer_input_text_offer(capture, 1, 1)); drain(); assert(!sends);
    send_editor(); assert(take("a") == 1);
    assert(fr_viewer_input_text_offer(capture, 2, 1)); open_editor(); tap(XK_b); drain();
    assert(fr_viewer_input_text_offer(capture, 0, 0));
    assert(!capture->text->active && !capture->text->length && !capture->text->pending_serial);
    assert(fr_viewer_input_text_offer(capture, 3, 1)); open_editor();
    tap(XK_c); send_editor(); assert(take("c") == 3);
}
static void bounds(void) {
    open_editor();
    char full[FR_TEXT_BYTES]; memset(full, 'a', sizeof(full));
    assert(text_insert(capture->text, full, sizeof(full)));
    assert(!text_insert(capture->text, "b", 1) && capture->text->invalid);
    send_editor(); assert(!sends);
    tap(XK_BackSpace); drain(); assert(!capture->text->invalid && !capture->text->length);
    const char *invalid[] = {"\xc0\xaf", "\xed\xa0\x80", "\xf4\x90\x80\x80", "\xe6\xbc"};
    for (size_t i = 0; i < sizeof(invalid) / sizeof(invalid[0]); i++) {
        assert(!text_insert(capture->text, invalid[i], strlen(invalid[i])));
        send_editor(); assert(!sends); tap(XK_BackSpace); drain();
    }
    tap(XK_a); send_editor(); assert(take("a") == 1);
}
static void editing(void) {
    open_editor(); tap(XK_eacute); tap(0x01006f22); tap(0x0101f642); tap(XK_Left); tap(XK_BackSpace); drain();
    assert(capture->text->length == 6);
    tap(XK_Home); tap(XK_Delete); tap(XK_End); tap(XK_Return); tap(XK_a); send_editor();
    assert(take("\xf0\x9f\x99\x82\na") == 1);
}
static void lifecycle(void) {
    open_editor(); tap(XK_a); send_editor(); assert(sends == 1);
    XSetInputFocus(peer, DefaultRootWindow(peer), RevertToParent, CurrentTime); XSync(peer, False); drain();
    uint8_t bytes[FR_TEXT_BYTES]; uint64_t serial = 0; uint32_t length = 0;
    assert(focus_lost && !capture->text->active);
    assert(fr_viewer_input_text_take(capture, &serial, bytes, sizeof(bytes), &length) == 0);
}
static void destroy_during_draw(void) {
    open_editor(); tap(XK_a); drain();
    XDestroyWindow(peer, window); XSync(peer, False); window = 0;
    /* Force the actual race: the child's destruction has not been consumed,
     * but a readiness redraw already targets that dead drawable. Xlib's
     * default handler must not kill the process or admit the retained draft. */
    text_draw(capture->text); XSync(capture->text->display, False);
    assert(atomic_load(&capture->text->failed));
    struct fr_viewer_event event;
    assert(fr_viewer_input_next(capture, &event) == -1);
    uint8_t bytes[FR_TEXT_BYTES]; uint64_t serial = 0; uint32_t length = 0;
    assert(fr_viewer_input_text_take(capture, &serial, bytes, sizeof(bytes), &length) == -1);
    assert(!serial && !length && !capture->text->pending_serial && !capture->text->length);
}
static void destroy_before_cleanup(void) {
    open_editor(); tap(XK_a); send_editor(); assert(sends == 1);
    XDestroyWindow(peer, window); XSync(peer, False); window = 0;
    /* A terminal Rust turn may drop native state before any native destruction
     * notification is read. The pending value and all native resources still
     * retire without a process-wide Xlib exit. */
    fr_viewer_input_close(capture); capture = NULL;
}
static void error_isolation(void) {
    Window other = XCreateSimpleWindow(peer, DefaultRootWindow(peer), 0, 0, 10, 10, 0, 0, 0);
    XDestroyWindow(peer, other); XClearWindow(peer, other); XSync(peer, False);
    assert(peer_errors == 1 && !atomic_load(&capture->text->failed));
    open_editor(); tap(XK_a); send_editor(); assert(take("a") == 1);
}
static void synthetic_input(void) {
    open_editor(); tap(XK_a); drain();
    XEvent event = {0}; event.xkey.type = KeyPress; event.xkey.display = peer;
    event.xkey.window = window; event.xkey.root = DefaultRootWindow(peer);
    event.xkey.keycode = code(XK_Return); event.xkey.state = ControlMask; event.xkey.same_screen = True;
    XSendEvent(peer, window, False, KeyPressMask, &event); XSync(peer, False); drain();
    assert(synthetic == 1 && !sends && !capture->text->active && !capture->text->pending_serial);
}
static void emergency_escape(void) {
    open_editor();
    unsigned initial = local;
    key(XK_Control_L, 1); key(XK_Alt_L, 1); key(XK_Shift_L, 1); key(XK_Escape, 1); drain();
    assert(local == initial + 4 && !retired && !sends && !remote);
    key(XK_Escape, 0); key(XK_Shift_L, 0); key(XK_Alt_L, 0); key(XK_Control_L, 0); drain();
}
static void unavailable_text(const char *reason) {
    assert(!capture->text);
    tap(XK_F7); tap(XK_a); drain();
    assert(remote == 4 && !local && !sends && !retired);
    char *title = NULL;
    assert(XFetchName(peer, window, &title) && title);
    assert(strstr(title, reason)); XFree(title);
}
static void same_batch_key_hold(void) {
    /* No offer refresh or Rust decode can occur between these native events. */
    key(XK_a, 1); tap(XK_F7); key(XK_a, 0); drain();
    assert(remote == 4 && !local && !capture->text->active);
    assert(!fr_viewer_input_text_held(capture));
    open_editor(); tap(XK_b); send_editor(); assert(take("b") == 1);
}
static void same_batch_pointer_hold(void) {
    move_pointer_inside();
    button(1, 1); tap(XK_F7); button(1, 0); drain();
    assert(remote == 2 && remote_buttons == 2 && !local && !local_buttons);
    assert(!capture->text->active && !fr_viewer_input_text_held(capture));
    open_editor(); tap(XK_c); send_editor(); assert(take("c") == 1);
}
static void local_pointer_release(void) {
    move_pointer_inside(); open_editor();
    button(1, 1); tap(XK_Escape); drain();
    assert(retired == 1 && !capture->text->active && local_buttons == 1);
    assert(!remote_buttons && fr_viewer_input_text_held(capture));
    button(1, 0); drain();
    assert(local_buttons == 2 && !remote_buttons && !fr_viewer_input_text_held(capture));
    assert(take("") == 1);
    assert(fr_viewer_input_text_offer(capture, 2, 1)); open_editor();
}
static void release_only_reconciliation(void) {
    key(XK_a, 1); drain(); assert(remote == 1 && fr_viewer_input_text_held(capture));
    lose_key_release(XK_a); assert(remote == 1 && fr_viewer_input_text_held(capture));
    reconcile_released(); assert(!fr_viewer_input_text_held(capture));
    struct fr_viewer_held unknown; memset(&unknown, 0xff, sizeof(unknown));
    fr_viewer_input_text_reconcile(capture, &unknown);
    assert(!fr_viewer_input_text_held(capture)); /* A snapshot never adopts a press. */
    key(XK_F7, 1); drain(); assert(capture->text->armed && !capture->text->active);
    lose_key_release(XK_F7); reconcile_released();
    assert(!capture->text->armed && !capture->text->active && !fr_viewer_input_text_held(capture));
    open_editor(); key(XK_b, 1); tap(XK_Escape); drain();
    assert(retired == 1 && fr_viewer_input_text_held(capture));
    lose_key_release(XK_b); reconcile_released();
    assert(!fr_viewer_input_text_held(capture));
    assert(take("") == 1);
    assert(fr_viewer_input_text_offer(capture, 2, 1)); open_editor();
}
/* Fault injection retains the actual original X event across the stable
 * snapshot. Its bytes/time are neither fabricated nor refreshed. This probes
 * late delivery at the native ownership boundary, not normal X wire ordering. */
static xcb_generic_event_t *retain_release(unsigned type, unsigned detail) {
    uint64_t until = now_ms() + 2000;
    for (;;) {
        xcb_generic_event_t *event = xcb_poll_for_event(capture->c);
        if (event) {
            assert((event->response_type & 0x7f) == type);
            assert(((xcb_key_release_event_t *)event)->detail == detail);
            return event;
        }
        assert(now_ms() < until); pause_ms();
    }
}
static void deliver_retained_release(xcb_generic_event_t *event, unsigned kind,
                                     unsigned expected) {
    const xcb_key_release_event_t *key = (const void *)event;
    struct fr_viewer_event out = {kind, key->time, key->detail, key->event_x, key->event_y};
    uint32_t original_time = out.time;
    text_filter(capture, event, &out);
    assert(out.kind == expected && out.time == original_time);
    free(event);
}
static void delayed_local_key_release(void) {
    open_editor(); key(XK_b, 1); tap(XK_Escape); drain();
    assert(retired == 1 && !capture->text->active && fr_viewer_input_text_held(capture));
    assert(take("") == 1);
    key(XK_b, 0);
    xcb_generic_event_t *release = retain_release(XCB_KEY_RELEASE, code(XK_b));
    reconcile_released(); assert(!fr_viewer_input_text_held(capture));
    assert(fr_viewer_input_text_offer(capture, 2, 1));
    deliver_retained_release(release, KEY_UP, TEXT_KEY_UP);
    assert(!fr_viewer_input_text_held(capture) && !remote && sends == 0 && retired == 1);
    // Custody of a retired editor gesture is never a new key press. Fresh
    // physical typing after that local release must still use both remote edges.
    tap(XK_b); drain(); assert(remote == 2);
    open_editor(); tap(XK_c); send_editor(); assert(take("c") == 2);
}
static void delayed_local_button_release(void) {
    move_pointer_inside(); open_editor(); button(1, 1); tap(XK_Escape); drain();
    assert(retired == 1 && local_buttons == 1 && !remote_buttons);
    assert(take("") == 1);
    button(1, 0);
    xcb_generic_event_t *release = retain_release(XCB_BUTTON_RELEASE, 1);
    reconcile_released(); assert(!fr_viewer_input_text_held(capture));
    assert(fr_viewer_input_text_offer(capture, 2, 1));
    deliver_retained_release(release, BUTTON_UP, TEXT_BUTTON_UP);
    assert(!fr_viewer_input_text_held(capture) && !remote_buttons);
    button(1, 1); button(1, 0); drain();
    assert(remote_buttons == 2 && !fr_viewer_input_text_held(capture));
}
static void reconciled_local_new_gesture(void) {
    open_editor(); key(XK_b, 1); tap(XK_Escape); drain();
    assert(take("") == 1);
    key(XK_b, 0); free(retain_release(XCB_KEY_RELEASE, code(XK_b)));
    reconcile_released(); assert(!fr_viewer_input_text_held(capture));
    assert(fr_viewer_input_text_offer(capture, 2, 1));
    tap(XK_b); drain();
    assert(remote == 2 && !fr_viewer_input_text_held(capture));
    move_pointer_inside(); open_editor(); button(1, 1); tap(XK_Escape); drain();
    assert(take("") == 2);
    button(1, 0); free(retain_release(XCB_BUTTON_RELEASE, 1));
    reconcile_released(); assert(!fr_viewer_input_text_held(capture));
    assert(fr_viewer_input_text_offer(capture, 3, 1));
    button(1, 1); button(1, 0); drain();
    assert(remote_buttons == 2 && !fr_viewer_input_text_held(capture));
}
int main(int argc, char **argv) {
    assert(argc >= 2);
    if (!strcmp(argv[1], "error-isolation")) {
        assert(XInitThreads()); XSetErrorHandler(previous_error);
    }
    setup(argv[1]);
    if (!strcmp(argv[1], "utf8-focus")) utf8_and_focus();
    else if (!strcmp(argv[1], "compose-before-send")) compose_before_send();
    else if (!strcmp(argv[1], "cancel-reopen")) cancel_reopen();
    else if (!strcmp(argv[1], "context-readiness")) context_and_readiness();
    else if (!strcmp(argv[1], "bounds")) bounds();
    else if (!strcmp(argv[1], "editing")) editing();
    else if (!strcmp(argv[1], "lifecycle")) lifecycle();
    else if (!strcmp(argv[1], "destroy-draw")) destroy_during_draw();
    else if (!strcmp(argv[1], "destroy-cleanup")) destroy_before_cleanup();
    else if (!strcmp(argv[1], "error-isolation")) error_isolation();
    else if (!strcmp(argv[1], "synthetic")) synthetic_input();
    else if (!strcmp(argv[1], "escape")) emergency_escape();
    else if (!strcmp(argv[1], "unavailable-ime")) unavailable_text("text_input_method_unavailable");
    else if (!strcmp(argv[1], "small-surface")) unavailable_text("text_input_surface_unavailable");
    else if (!strcmp(argv[1], "same-batch-key")) same_batch_key_hold();
    else if (!strcmp(argv[1], "same-batch-pointer")) same_batch_pointer_hold();
    else if (!strcmp(argv[1], "local-pointer-release")) local_pointer_release();
    else if (!strcmp(argv[1], "release-only-reconcile")) release_only_reconciliation();
    else if (!strcmp(argv[1], "delayed-local-key-release")) delayed_local_key_release();
    else if (!strcmp(argv[1], "delayed-local-button-release")) delayed_local_button_release();
    else if (!strcmp(argv[1], "reconciled-local-new-gesture")) reconciled_local_new_gesture();
    else assert(0);
    fr_viewer_input_close(capture);
    if (window) XDestroyWindow(peer, window);
    XCloseDisplay(peer);
    puts("native text case passed"); return 0;
}
