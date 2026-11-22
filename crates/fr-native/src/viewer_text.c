/* Included by viewer_input.c. A local, bounded UTF-8 composer for the original
 * viewer. It never injects input, reads the clipboard, selects a remote target,
 * changes process locale or owns authority. Xlib/XIM and its child drawable are
 * confined to the capture thread; keyboard focus stays on the original window.
 * Only an explicit Send exports bytes, with the original X timestamp and Rust's
 * non-reused context serial. IME preedit is never an exported commit. */
#include <X11/Xlib.h>
#include <X11/keysym.h>
#include <pthread.h>
#include <stdatomic.h>

#define FR_TEXT_BYTES 4096u
enum { TEXT_SEND = 13, TEXT_KEY_DOWN = 14, TEXT_KEY_UP = 15, TEXT_RETIRED = 16,
       TEXT_BUTTON_DOWN = 17, TEXT_BUTTON_UP = 18 };
struct fr_viewer_text {
    Display *display;
    XIM im;
    XIC ic;
    XFontSet font;
    Window panel;
    GC gc;
    struct fr_viewer_text *error_next;
    atomic_int failed;
    int error_registered;
    XIMStyle style;
    uint64_t offered, draft_serial, pending_serial, awaiting_result;
    uint32_t pending_kind;
    unsigned int width, height;
    int ready, active, armed, invalid, selected;
    uint8_t f7, enter, escape;
    uint8_t local_keys[32], local_buttons[32];
    uint8_t released_local_keys[32], released_local_buttons[32];
    uint8_t forwarded_keys[32], forwarded_buttons[32];
    size_t length, cursor, pending_length;
    char draft[FR_TEXT_BYTES], pending[FR_TEXT_BYTES];
};

/* Xlib's protocol-error handler is process-wide. Install one dispatcher, never
 * a temporary trap around individual calls. Only our explicitly registered,
 * thread-confined displays are intercepted; every unrelated display retains
 * the previous handler. Parent destruction can invalidate the child between
 * any two requests, including drawing, so event-only cleanup is insufficient.
 * No registry lock is held across an Xlib request. Only handler installation
 * holds it until the prior function is saved; XSetErrorHandler cannot dispatch
 * an error. The entry stays registered through XCloseDisplay, which can deliver
 * already-buffered errors. */
static pthread_once_t text_errors_once = PTHREAD_ONCE_INIT;
static pthread_mutex_t text_errors_lock = PTHREAD_MUTEX_INITIALIZER;
static struct fr_viewer_text *text_error_displays;
static XErrorHandler text_previous_error;
static int text_error(Display *display, XErrorEvent *error) {
    pthread_mutex_lock(&text_errors_lock);
    for (struct fr_viewer_text *t = text_error_displays; t; t = t->error_next) {
        if (t->display == display) {
            atomic_store(&t->failed, 1);
            pthread_mutex_unlock(&text_errors_lock);
            return 0;
        }
    }
    pthread_mutex_unlock(&text_errors_lock);
    return text_previous_error(display, error);
}
static void text_errors_install(void) {
    pthread_mutex_lock(&text_errors_lock);
    text_previous_error = XSetErrorHandler(text_error);
    pthread_mutex_unlock(&text_errors_lock);
}
static int text_register_display(struct fr_viewer_text *t) {
    if (pthread_once(&text_errors_once, text_errors_install)) return 0;
    pthread_mutex_lock(&text_errors_lock);
    t->error_next = text_error_displays;
    text_error_displays = t;
    t->error_registered = 1;
    pthread_mutex_unlock(&text_errors_lock);
    return 1;
}
static void text_unregister_display(struct fr_viewer_text *t) {
    if (!t->error_registered) return;
    pthread_mutex_lock(&text_errors_lock);
    struct fr_viewer_text **entry = &text_error_displays;
    while (*entry && *entry != t) entry = &(*entry)->error_next;
    if (*entry) *entry = t->error_next;
    t->error_registered = 0;
    pthread_mutex_unlock(&text_errors_lock);
}

static int text_bits(const uint8_t *bits) {
    for (size_t i = 0; i < 32; i++) if (bits[i]) return 1;
    return 0;
}
static int text_bit(const uint8_t *bits, unsigned int key) {
    return key < 256 && (bits[key / 8] & (1u << (key % 8)));
}
static void text_set(uint8_t *bits, unsigned int key, int down) {
    if (key >= 256) return;
    if (down) bits[key / 8] |= (uint8_t)(1u << (key % 8));
    else bits[key / 8] &= (uint8_t)~(1u << (key % 8));
}
/* Physical ownership is recorded before Rust decodes the complete native
 * batch. A later F7 in that batch must not steal a previously forwarded up. */
int fr_viewer_input_text_held(struct fr_viewer_input *h) {
    struct fr_viewer_text *t = h ? h->text : NULL;
    return t && (text_bits(t->forwarded_keys) || text_bits(t->forwarded_buttons) ||
        text_bits(t->local_keys) || text_bits(t->local_buttons));
}
/* Called only after Rust accepts its original, transition-free server-barrier
 * snapshot. Intersect existing ownership; never adopt a press or open a draft. */
void fr_viewer_input_text_reconcile(struct fr_viewer_input *h,
                                  const struct fr_viewer_held *actual) {
    struct fr_viewer_text *t = h ? h->text : NULL;
    if (!t || !actual) return;
    for (size_t i = 0; i < 32; i++) {
        t->forwarded_keys[i] &= actual->keys[i];
        t->forwarded_buttons[i] &= actual->buttons[i];
        /* Retain only negative gesture custody. A delayed old up stays local
         * after the editor closes, but this does not count as a held key or
         * prevent a fresh editor. A new down retires its old allowance. */
        t->released_local_keys[i] |= t->local_keys[i] & (uint8_t)~actual->keys[i];
        t->released_local_buttons[i] |= t->local_buttons[i] & (uint8_t)~actual->buttons[i];
        t->local_keys[i] &= actual->keys[i];
        t->local_buttons[i] &= actual->buttons[i];
    }
    if (t->armed && !text_bit(t->local_keys, t->f7)) t->armed = 0;
}
/* Refuse malformed/overlong UTF-8, surrogates and values above U+10FFFF. */
static size_t text_scalar(const char *bytes, size_t length) {
    if (!length) return 0;
    const uint8_t *s = (const uint8_t *)bytes;
    if (s[0] < 0x80) return 1;
    size_t n = s[0] >= 0xc2 && s[0] <= 0xdf ? 2 :
        s[0] >= 0xe0 && s[0] <= 0xef ? 3 : s[0] >= 0xf0 && s[0] <= 0xf4 ? 4 : 0;
    if (!n || n > length) return 0;
    for (size_t i = 1; i < n; i++) if ((s[i] & 0xc0) != 0x80) return 0;
    if ((s[0] == 0xe0 && s[1] < 0xa0) || (s[0] == 0xed && s[1] >= 0xa0) ||
        (s[0] == 0xf0 && s[1] < 0x90) || (s[0] == 0xf4 && s[1] >= 0x90)) return 0;
    return n;
}
static size_t text_previous(const char *bytes, size_t cursor) {
    if (!cursor) return 0;
    cursor--;
    while (cursor && (((uint8_t)bytes[cursor] & 0xc0) == 0x80)) cursor--;
    return cursor;
}
static int text_insert(struct fr_viewer_text *t, const char *bytes, size_t length) {
    for (size_t at = 0; at < length;) {
        size_t n = text_scalar(bytes + at, length - at);
        if (!n) { t->invalid = 1; return 0; }
        at += n;
    }
    size_t current = t->selected ? 0 : t->length;
    if (t->invalid || length > FR_TEXT_BYTES - current) { t->invalid = 1; return 0; }
    if (t->selected) {
        memset(t->draft, 0, sizeof(t->draft)); t->length = t->cursor = 0; t->selected = 0;
    }
    memmove(t->draft + t->cursor + length, t->draft + t->cursor, t->length - t->cursor);
    memcpy(t->draft + t->cursor, bytes, length);
    t->length += length; t->cursor += length;
    return 1;
}
static void text_erase(struct fr_viewer_text *t, int backwards) {
    if (t->selected || t->invalid) {
        memset(t->draft, 0, sizeof(t->draft));
        t->length = t->cursor = 0; t->invalid = t->selected = 0; return;
    }
    size_t from = t->cursor, to = t->cursor;
    if (backwards) from = text_previous(t->draft, from);
    else if (to < t->length) to += text_scalar(t->draft + to, t->length - to);
    memmove(t->draft + from, t->draft + to, t->length - to);
    size_t next = t->length - (to - from);
    memset(t->draft + next, 0, t->length - next);
    t->length = next; t->cursor = from;
}
static void text_title(struct fr_viewer_input *h, const char *message) {
    if (!h->c || xcb_connection_has_error(h->c)) return;
    xcb_change_property(h->c, XCB_PROP_MODE_REPLACE, h->window,
        XCB_ATOM_WM_NAME, XCB_ATOM_STRING, 8, (uint32_t)strlen(message), message);
    xcb_change_property(h->c, XCB_PROP_MODE_REPLACE, h->window,
        h->net_name, h->utf8, 8, (uint32_t)strlen(message), message);
    (void)xcb_flush(h->c);
}
static void text_draw(struct fr_viewer_text *t) {
    if (!t->active || !t->panel || atomic_load(&t->failed)) return;
    XClearWindow(t->display, t->panel);
    const char *heading = "Send Text - local draft";
    const char *hint = "Ctrl+Enter sends; Esc/F7 cancels; Ctrl+A selects all";
    Xutf8DrawString(t->display, t->panel, t->font, t->gc, 10, 22, heading, (int)strlen(heading));
    Xutf8DrawString(t->display, t->panel, t->font, t->gc, 10, 42, hint, (int)strlen(hint));
    /* Render bounded complete scalars. Scroll the preview to keep the insertion
     * point's final lines visible; never put draft text in a title or property. */
    size_t starts[FR_TEXT_BYTES + 1]; size_t lines = 1, at = 0;
    int x = 10;
    starts[0] = 0;
    size_t caret_line = 0;
    while (at < t->length) {
        if (at == t->cursor) caret_line = lines - 1;
        size_t n = text_scalar(t->draft + at, t->length - at);
        if (!n) break;
        int advance = t->draft[at] == '\t' ? 32 :
            Xutf8TextEscapement(t->font, t->draft + at, (int)n);
        if (t->draft[at] == '\n') {
            at += n; starts[lines++] = at; x = 10; continue;
        }
        if (x > 10 && x + advance > (int)t->width - 12) {
            starts[lines++] = at; x = 10;
        }
        x += advance; at += n;
    }
    if (t->cursor == t->length) caret_line = lines - 1;
    size_t visible = (t->height - 100u) / 18u;
    if (!visible) visible = 1;
    size_t first = caret_line >= visible ? caret_line - visible + 1 : 0;
    for (size_t row = first; row < lines && row < first + visible; row++) {
        size_t start = starts[row], end = row + 1 < lines ? starts[row + 1] : t->length;
        x = 10;
        for (size_t p = start; p < end;) {
            size_t n = text_scalar(t->draft + p, end - p); if (!n) break;
            int y = 65 + (int)(row - first) * 18;
            if (p == t->cursor) XDrawLine(t->display, t->panel, t->gc, x, y - 12, x, y + 2);
            if (t->draft[p] == '\t') x += 32;
            else if (t->draft[p] != '\n' && t->draft[p] != '\r' && t->draft[p] != '\0') {
                Xutf8DrawString(t->display, t->panel, t->font, t->gc, x, y, t->draft + p, (int)n);
                x += Xutf8TextEscapement(t->font, t->draft + p, (int)n);
            }
            p += n;
        }
        if (t->cursor == end && row == caret_line) {
            int y = 65 + (int)(row - first) * 18;
            XDrawLine(t->display, t->panel, t->gc, x, y - 12, x, y + 2);
        }
    }
    const char *state = t->invalid ? "Text refused: invalid UTF-8 or 4096-byte limit. Backspace clears." :
        t->selected ? "All text selected" : !t->ready ? "Waiting for the original view to be ready" :
        "Only committed text is sent; IME preedit stays local.";
    Xutf8DrawString(t->display, t->panel, t->font, t->gc, 10, (int)t->height - 14,
        state, (int)strlen(state));
    XFlush(t->display);
}
static void text_close(struct fr_viewer_text *t) {
    if (t->ic) { XUnsetICFocus(t->ic); XDestroyIC(t->ic); t->ic = NULL; }
    if (t->active && t->panel) { XUnmapWindow(t->display, t->panel); XFlush(t->display); }
    t->active = t->armed = t->invalid = t->selected = 0;
    t->draft_serial = 0; t->length = t->cursor = 0;
    memset(t->draft, 0, sizeof(t->draft));
}
static void text_clear(struct fr_viewer_text *t) {
    text_close(t);
    t->pending_serial = t->awaiting_result = 0;
    t->pending_length = t->pending_kind = 0;
    memset(t->pending, 0, sizeof(t->pending));
}
static void text_destroy(struct fr_viewer_input *h) {
    struct fr_viewer_text *t = h->text; if (!t) return;
    text_clear(t);
    if (t->gc) XFreeGC(t->display, t->gc);
    if (t->font) XFreeFontSet(t->display, t->font);
    if (t->panel) XDestroyWindow(t->display, t->panel);
    if (t->im) XCloseIM(t->im);
    if (t->display) XCloseDisplay(t->display);
    text_unregister_display(t);
    memset(t, 0, sizeof(*t)); free(t); h->text = NULL;
}
/* XCB only: usable even when the optional process-wide Xlib/XIM preparation
 * refused. No locale-dependent call or text payload belongs in this result. */
void fr_viewer_input_text_unavailable(struct fr_viewer_input *h) {
    if (h) text_title(h, "FrankenRemote - text_input_method_unavailable - physical keys remain available");
}
/* Setup only, after Rust initializes Xlib threads and after original control
 * negotiated Text. Native IME/font absence affects only this local feature. */
int fr_viewer_input_text_prepare(struct fr_viewer_input *h, const char *display) {
    if (!h || h->text) return 0;
    if (!display) { fr_viewer_input_text_unavailable(h); return 0; }
    if (h->width < 280 || h->height < 150) {
        text_title(h, "FrankenRemote - text_input_surface_unavailable - physical keys remain available");
        return 0;
    }
    struct fr_viewer_text *t = calloc(1, sizeof(*t));
    if (!t) { fr_viewer_input_text_unavailable(h); return 0; }
    atomic_init(&t->failed, 0);
    h->text = t;
    for (unsigned i = 8; i < 256; i++) {
        if (!memcmp(h->keynames[i], "FK07", 4)) t->f7 = (uint8_t)i;
        if (!memcmp(h->keynames[i], "RTRN", 4)) t->enter = (uint8_t)i;
        if (!memcmp(h->keynames[i], "ESC\0", 4)) t->escape = (uint8_t)i;
    }
    if (!t->f7 || !t->enter || !t->escape) goto unavailable;
    t->display = fr_x11_open_display(display); if (!t->display) goto unavailable;
    if (!text_register_display(t)) goto unavailable;
    t->im = XOpenIM(t->display, NULL, NULL, NULL); if (!t->im) goto unavailable;
    XIMStyles *styles = NULL;
    if (XGetIMValues(t->im, XNQueryInputStyle, &styles, NULL) || !styles) goto unavailable;
    for (unsigned i = 0; i < styles->count_styles; i++) {
        if (styles->supported_styles[i] == (XIMPreeditNothing | XIMStatusNothing))
            t->style = styles->supported_styles[i];
    }
    XFree(styles); if (!t->style) goto unavailable;
    char **missing = NULL, *fallback = NULL; int missing_count = 0;
    t->font = XCreateFontSet(t->display, "-misc-fixed-medium-r-normal--14-*-*-*-*-*-*-*,fixed",
        &missing, &missing_count, &fallback);
    if (missing) XFreeStringList(missing);
    if (!t->font) goto unavailable;
    t->width = h->width > 660 ? 640 : h->width - 20;
    t->height = h->height > 260 ? 240 : h->height - 20;
    int screen = DefaultScreen(t->display);
    t->panel = XCreateSimpleWindow(t->display, h->window, 8, 8, t->width, t->height,
        1, BlackPixel(t->display, screen), WhitePixel(t->display, screen));
    XSelectInput(t->display, t->panel, ExposureMask | StructureNotifyMask);
    t->gc = XCreateGC(t->display, t->panel, 0, NULL);
    if (!t->panel || !t->gc) goto unavailable;
    XSetForeground(t->display, t->gc, BlackPixel(t->display, screen));
    /* Setup may synchronize; event production below remains bounded by the
     * original Rust turn. Refuse a parent that disappeared during creation. */
    XSync(t->display, False);
    if (atomic_load(&t->failed)) goto unavailable;
    text_title(h, "FrankenRemote - F7: Send Text - Ctrl+Alt+Shift+Escape releases control");
    return 1;
unavailable:
    text_destroy(h);
    fr_viewer_input_text_unavailable(h);
    return 0;
}
int fr_viewer_input_text_offer(struct fr_viewer_input *h, uint64_t serial, int ready) {
    if (!h || xcb_connection_has_error(h->c)) return 0;
    struct fr_viewer_text *t = h->text; if (!t) return 1;
    if (atomic_load(&t->failed)) return 0;
    if (t->offered != serial) { text_clear(t); t->offered = serial; }
    int changed = t->ready != (!!ready && serial != 0);
    t->ready = !!ready && serial != 0;
    if (changed && t->active) text_draw(t);
    return 1;
}
static int text_open(struct fr_viewer_input *h) {
    struct fr_viewer_text *t = h->text;
    if (!t || !t->offered || !t->ready || t->pending_serial || t->awaiting_result) return 0;
    /* A fresh native IC per draft prevents a delayed old IME completion from
     * being interpreted using the successor's input context. */
    t->ic = XCreateIC(t->im, XNInputStyle, t->style,
        XNClientWindow, (Window)h->window, XNFocusWindow, (Window)h->window, NULL);
    if (!t->ic) {
        text_title(h, "FrankenRemote - text_input_method_unavailable"); return 0;
    }
    t->active = 1; t->draft_serial = t->offered;
    XSetICFocus(t->ic);
    XMapRaised(t->display, t->panel); text_draw(t);
    return 1;
}
static void text_finish(struct fr_viewer_input *h, struct fr_viewer_event *out, int send) {
    struct fr_viewer_text *t = h->text;
    if (!t->active || !t->draft_serial || t->pending_serial) return;
    if (send && (!t->ready || t->invalid || !t->length)) { text_draw(t); return; }
    t->pending_serial = t->draft_serial;
    t->pending_length = send ? t->length : 0;
    t->pending_kind = send ? TEXT_SEND : TEXT_RETIRED;
    if (send) memcpy(t->pending, t->draft, t->length);
    text_close(t);
    out->kind = t->pending_kind;
    if (!send) text_title(h, "FrankenRemote - Text cancelled - F7: Send Text");
}
int fr_viewer_input_text_take(struct fr_viewer_input *h, uint64_t *serial,
        uint8_t *bytes, uint32_t capacity, uint32_t *length) {
    if (!h || !serial || !bytes || !length) return -1;
    *serial = 0; *length = 0;
    struct fr_viewer_text *t = h->text;
    if (t && atomic_load(&t->failed)) { text_clear(t); return -1; }
    if (!t || !t->pending_serial) return 0;
    if (t->pending_length > capacity) { text_clear(t); return -1; }
    *serial = t->pending_serial; *length = (uint32_t)t->pending_length;
    memcpy(bytes, t->pending, t->pending_length);
    t->awaiting_result = t->pending_serial;
    t->pending_serial = 0; t->pending_length = t->pending_kind = 0;
    memset(t->pending, 0, sizeof(t->pending));
    return 1;
}
void fr_viewer_input_text_result(struct fr_viewer_input *h, uint64_t serial, int queued) {
    if (!h || !h->text || !serial || h->text->awaiting_result != serial) return;
    h->text->awaiting_result = 0;
    text_title(h, queued ? "FrankenRemote - Text queued (host result pending) - F7: Send Text" :
        "FrankenRemote - Text refused; nothing retried - F7: Send Text");
}
static void text_lookup(struct fr_viewer_text *t, XKeyEvent *key) {
    if (!t->active || !t->ic) return;
    XEvent event = {0}; event.xkey = *key;
    if (XFilterEvent(&event, key->window)) return;
    if (key->type != KeyPress) return;
    char bytes[FR_TEXT_BYTES]; KeySym symbol = NoSymbol; Status status = XLookupNone;
    int length = Xutf8LookupString(t->ic, key, bytes, sizeof(bytes), &symbol, &status);
    if (status == XBufferOverflow || length < 0 || length > (int)sizeof(bytes)) {
        t->invalid = 1;
    } else if (key->state & ControlMask) {
        if (symbol == XK_a || symbol == XK_A) t->selected = 1;
    } else if (symbol == XK_BackSpace || symbol == XK_Delete) {
        text_erase(t, symbol == XK_BackSpace);
    } else if (symbol == XK_Left) {
        t->cursor = text_previous(t->draft, t->cursor); t->selected = 0;
    } else if (symbol == XK_Right) {
        if (t->cursor < t->length) t->cursor += text_scalar(t->draft + t->cursor, t->length - t->cursor);
        t->selected = 0;
    } else if (symbol == XK_Home || symbol == XK_End) {
        t->cursor = symbol == XK_Home ? 0 : t->length; t->selected = 0;
    } else if (status == XLookupChars || status == XLookupBoth) {
        if (symbol == XK_Return) { bytes[0] = '\n'; length = 1; }
        (void)text_insert(t, bytes, (size_t)length);
    }
    memset(bytes, 0, sizeof(bytes)); text_draw(t);
}
static int text_poll(struct fr_viewer_input *h, struct fr_viewer_event *out) {
    struct fr_viewer_text *t = h->text;
    if (!t) return 0;
    if (atomic_load(&t->failed)) return -1;
    int queued = XEventsQueued(t->display, QueuedAfterReading);
    if (atomic_load(&t->failed)) return -1;
    if (!queued) return 0;
    XEvent event; XNextEvent(t->display, &event);
    /* One event per bounded Rust poll. Only our original XIC filters IME
     * protocol events. An IME's forwarded key may update a LOCAL draft, never
     * create a Send or provide positive input/focus evidence. */
    if (event.type == DestroyNotify && event.xdestroywindow.window == t->panel) {
        t->panel = 0;
        text_clear(t);
        atomic_store(&t->failed, 1);
    } else if (t->active && event.type == KeyPress && event.xkey.window == h->window) {
        text_lookup(t, &event.xkey);
    } else if (!XFilterEvent(&event, None) && event.type == Expose && event.xexpose.window == t->panel) {
        text_draw(t);
    }
    memset(out, 0, sizeof(*out)); return atomic_load(&t->failed) ? -1 : 1;
}
static void text_filter(struct fr_viewer_input *h, const xcb_generic_event_t *event,
                        struct fr_viewer_event *out) {
    struct fr_viewer_text *t = h->text; if (!t) return;
    if (out->kind >= FOCUS_LOST && out->kind <= DEVICES_CHANGED) {
        if (out->kind != GEOMETRY || out->x != (int32_t)h->width || out->y != (int32_t)h->height)
            text_clear(t);
        return;
    }
    if (out->kind == BUTTON_DOWN || out->kind == BUTTON_UP || out->kind == MOTION) {
        if (out->kind != MOTION) {
            int down = out->kind == BUTTON_DOWN;
            int released = text_bit(t->released_local_buttons, out->detail);
            text_set(t->released_local_buttons, out->detail, 0);
            if (!down && released) { out->kind = TEXT_BUTTON_UP; return; }
        }
        if (out->kind != MOTION && text_bit(t->forwarded_buttons, out->detail)) {
            text_set(t->forwarded_buttons, out->detail, out->kind == BUTTON_DOWN);
            return;
        }
        if (t->active || text_bits(t->local_keys) || text_bits(t->local_buttons)) {
            if (out->kind != MOTION) {
                int down = out->kind == BUTTON_DOWN;
                text_set(t->local_buttons, out->detail, down);
                out->kind = down ? TEXT_BUTTON_DOWN : TEXT_BUTTON_UP;
            } else out->kind = IGNORE;
        } else if (out->kind != MOTION) {
            text_set(t->forwarded_buttons, out->detail, out->kind == BUTTON_DOWN);
        }
        return;
    }
    if (out->kind != KEY_DOWN && out->kind != KEY_UP) return;
    const xcb_key_press_event_t *key = (const void *)event;
    int down = out->kind == KEY_DOWN, was_local = text_bit(t->local_keys, key->detail);
    int released = text_bit(t->released_local_keys, key->detail);
    text_set(t->released_local_keys, key->detail, 0);
    if (!down && released) { out->kind = TEXT_KEY_UP; return; }
    if (text_bit(t->forwarded_keys, key->detail)) {
        text_set(t->forwarded_keys, key->detail, down);
        return;
    }
    const unsigned int modifiers = XCB_MOD_MASK_SHIFT | XCB_MOD_MASK_CONTROL | XCB_MOD_MASK_1 |
        XCB_MOD_MASK_3 | XCB_MOD_MASK_4 | XCB_MOD_MASK_5;
    if (!t->active && !was_local && !text_bits(t->local_keys) && !text_bits(t->local_buttons)) {
        if (!(t->offered && t->ready && down && key->detail == t->f7 && !(key->state & modifiers)) ||
            text_bits(t->forwarded_keys) || text_bits(t->forwarded_buttons)) {
            text_set(t->forwarded_keys, key->detail, down); return;
        }
        t->armed = 1;
    }
    text_set(t->local_keys, key->detail, down);
    out->kind = down ? TEXT_KEY_DOWN : TEXT_KEY_UP;
    if (t->armed && key->detail == t->f7 && !down) {
        t->armed = 0; (void)text_open(h); return;
    }
    if (!t->active) return;
    if (key->detail == t->escape && down) {
        /* Keep the whole physical emergency chord observable to Rust. */
        if ((key->state & (XCB_MOD_MASK_SHIFT | XCB_MOD_MASK_CONTROL | XCB_MOD_MASK_1)) ==
            (XCB_MOD_MASK_SHIFT | XCB_MOD_MASK_CONTROL | XCB_MOD_MASK_1)) return;
        text_finish(h, out, 0); return;
    }
    if (key->detail == t->f7 && down) { text_finish(h, out, 0); return; }
    if (key->detail == t->enter && down && (key->state & XCB_MOD_MASK_CONTROL)) {
        text_finish(h, out, 1); return;
    }
    XKeyEvent local = {0};
    local.type = down ? KeyPress : KeyRelease; local.display = t->display;
    local.window = h->window; local.root = key->root; local.subwindow = key->child;
    local.time = key->time; local.x = key->event_x; local.y = key->event_y;
    local.x_root = key->root_x; local.y_root = key->root_y;
    local.state = key->state; local.keycode = key->detail; local.same_screen = True;
    text_lookup(t, &local);
}
